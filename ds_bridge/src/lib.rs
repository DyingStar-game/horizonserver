use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use horizon_event_system::{
    create_simple_plugin, EventSystem, PluginError, ServerContext, SimplePlugin,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

mod config;
use config::{BridgeConfig, BridgeEventEnvelope};

// Channel buffer: absorbs bursts without blocking event handlers. Sized well above
// the per-tick drain so a momentary spike in emitted events cannot overflow it;
// sustained overload is handled by coalescing in the writer, not by more buffer.
const CHANNEL_CAPACITY: usize = 8192;
// Maximum envelopes pulled from the channel in a single drain before writing.
// Draining in batches lets `coalesce_batch` collapse redundant updates that
// queued up while the previous batch was being written.
const MAX_DRAIN_BATCH: usize = 1024;
// How often each service reports dropped-event counts (if any).
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(5);
// Delay before attempting to reconnect after a WebSocket error or disconnect.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub struct DyingstarBridgePlugin {
    name: String,
    /// Dedicated tokio runtime — avoids cross-DLL async runtime issues.
    runtime: Arc<tokio::runtime::Runtime>,
    /// Loaded in register_handlers; shared with WS tasks and event handlers.
    config: Arc<BridgeConfig>,
    /// One mpsc Sender per external service (keyed by service name).
    /// Event handlers clone the Sender and push envelopes into it.
    service_senders: Arc<DashMap<String, mpsc::Sender<BridgeEventEnvelope>>>,
    /// Dropped-event counter per service, bumped by handlers when the outgoing
    /// channel is full and reported periodically instead of per event.
    drop_counters: Arc<DashMap<String, Arc<AtomicU64>>>,
}

impl DyingstarBridgePlugin {
    pub fn new() -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build DyingstarBridgePlugin runtime"),
        );
        // Config is loaded inside register_handlers so we can log properly;
        // use a placeholder here and overwrite below.
        let config = Arc::new(BridgeConfig {
            log_level: "info".to_string(),
            services: vec![],
        });
        Self {
            name: "dyingstar_bridge".to_string(),
            runtime,
            config,
            service_senders: Arc::new(DashMap::new()),
            drop_counters: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl SimplePlugin for DyingstarBridgePlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn on_init(&mut self, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("ds_bridge plugin initialised ({} service(s))", self.config.services.len());
        Ok(())
    }

    async fn register_handlers(
        &mut self,
        events: Arc<EventSystem>,
        _context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        // Load config — panics (via ds_common) if plugins.toml is missing.
        let cfg = Arc::new(BridgeConfig::load());
        self.config = cfg.clone();

        // Initialise the tracing subscriber here (not in on_init) so that all
        // logs emitted during handler registration and spawned WS tasks are
        // captured. The bridge .so has its own tracing global; calling
        // try_init() in on_init would be too late.
        {
            use tracing::Level;
            use tracing_subscriber::FmtSubscriber;
            let level = match cfg.log_level.as_str() {
                "trace" => Level::TRACE,
                "debug" => Level::DEBUG,
                "warn" => Level::WARN,
                "error" => Level::ERROR,
                _ => Level::INFO,
            };
            FmtSubscriber::builder()
                .with_max_level(level)
                .try_init()
                .ok();
        }

        for service in &cfg.services {
            info!(
                service = %service.name,
                url = %service.url,
                "loaded bridge service from config"
            );
            let (tx, rx) = mpsc::channel::<BridgeEventEnvelope>(CHANNEL_CAPACITY);
            self.service_senders.insert(service.name.clone(), tx);

            // Dropped-event counter, shared with this service's event handlers.
            // Handlers only bump the counter; a reporter task logs a periodic
            // summary. Logging per dropped event floods the log at exactly the
            // moment the system is already overloaded.
            let dropped = Arc::new(AtomicU64::new(0));
            self.drop_counters
                .insert(service.name.clone(), dropped.clone());

            let report_name = service.name.clone();
            let report_dropped = dropped.clone();
            self.runtime.spawn(async move {
                let mut ticker = tokio::time::interval(DROP_REPORT_INTERVAL);
                loop {
                    ticker.tick().await;
                    let n = report_dropped.swap(0, Ordering::Relaxed);
                    if n > 0 {
                        warn!(
                            service = %report_name,
                            dropped = n,
                            interval_secs = DROP_REPORT_INTERVAL.as_secs(),
                            "outgoing channel full — events dropped (service cannot keep up)"
                        );
                    }
                }
            });

            // Spawn a persistent WS task for this service.
            let svc_name = service.name.clone();
            let svc_url = service.url.clone();
            let events_clone = events.clone();
            self.runtime.spawn(run_service_connection(
                svc_name,
                svc_url,
                rx,
                events_clone,
            ));
        }

        // Register event handlers for each service's subscribe list.
        for service in &cfg.services {
            for key in &service.subscribe {
                let parts: Vec<&str> = key.splitn(3, ':').collect();
                let sender = match self.service_senders.get(&service.name) {
                    Some(s) => s.clone(),
                    None => continue,
                };
                let dropped = match self.drop_counters.get(&service.name) {
                    Some(d) => d.clone(),
                    None => continue,
                };

                match parts.as_slice() {
                    ["core", event_name] => {
                        let event_name = event_name.to_string();
                        let event_name_key = event_name.clone();
                        events
                            .on_core(&event_name_key, move |payload: serde_json::Value| {
                                let envelope = BridgeEventEnvelope {
                                    event_type: "core".to_string(),
                                    namespace: None,
                                    name: event_name.clone(),
                                    payload,
                                };
                                if sender.try_send(envelope).is_err() {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(())
                            })
                            .await
                            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
                        debug!(service = %service.name, key = %key, "registered event listener");
                    }
                    ["plugin", plugin_ns, event_name] => {
                        let plugin_ns = plugin_ns.to_string();
                        let event_name = event_name.to_string();
                        let plugin_ns_key = plugin_ns.clone();
                        let event_name_key = event_name.clone();
                        events
                            .on_plugin(&plugin_ns_key, &event_name_key, move |payload: serde_json::Value| {
                                let envelope = BridgeEventEnvelope {
                                    event_type: "plugin".to_string(),
                                    namespace: Some(plugin_ns.clone()),
                                    name: event_name.clone(),
                                    payload,
                                };
                                if sender.try_send(envelope).is_err() {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(())
                            })
                            .await
                            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
                        debug!(service = %service.name, key = %key, "registered event listener");
                    }
                    _ => {
                        warn!(
                            key = %key,
                            service = %service.name,
                            "unknown subscribe key format, skipping"
                        );
                    }
                }
            }
        }

        info!(
            "ds_bridge: registered handlers for {} service(s)",
            cfg.services.len()
        );
        Ok(())
    }

    async fn on_shutdown(&mut self, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        // Dropping DashMap closes all mpsc channels, which signals WS tasks to stop.
        self.service_senders.clear();
        info!("ds_bridge: shutdown");
        Ok(())
    }
}

/// Events whose `object_data` is a last-write-wins partial update, and which can
/// therefore be merged when several queue up for the same object before the
/// writer drains them. Anything not listed here is forwarded untouched.
fn is_coalescable(env: &BridgeEventEnvelope) -> bool {
    matches!(
        env.name.as_str(),
        "update_object" | "update_object_from_external"
    )
}

fn object_uuid(env: &BridgeEventEnvelope) -> Option<String> {
    env.payload
        .get("object_uuid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Merge `src`'s object_data into `dst`'s, with `src` (the newer event) winning
/// per field. Both are partial updates, so a shallow merge at the object_data
/// level preserves fields that the newer update did not mention.
fn merge_update(dst: &mut BridgeEventEnvelope, src: BridgeEventEnvelope) {
    let src_data = match src.payload.get("object_data") {
        Some(v) => v.clone(),
        None => return,
    };
    match dst.payload.get_mut("object_data") {
        Some(dst_data) => match (dst_data, src_data) {
            (serde_json::Value::Object(d), serde_json::Value::Object(s)) => {
                for (k, v) in s {
                    d.insert(k, v);
                }
            }
            // Non-object payloads cannot be field-merged; newest wins wholesale.
            (d, s) => *d = s,
        },
        None => {
            if let Some(obj) = dst.payload.as_object_mut() {
                obj.insert("object_data".to_string(), src_data);
            }
        }
    }
}

/// Collapse redundant partial updates within a drained batch.
///
/// Only consecutive updates for the same (namespace, event, object) are merged.
/// Any other event for that object (create/delete/...) acts as an ordering
/// barrier: updates after it get a fresh slot, so an update can never be
/// reordered across a create or delete for the same object.
fn coalesce_batch(batch: Vec<BridgeEventEnvelope>) -> Vec<BridgeEventEnvelope> {
    let mut out: Vec<BridgeEventEnvelope> = Vec::with_capacity(batch.len());
    // (namespace, event name, object uuid) -> index in `out` of the open slot.
    let mut pending: HashMap<(String, String, String), usize> = HashMap::new();

    for env in batch {
        let uuid = match object_uuid(&env) {
            Some(u) => u,
            // No object identity — cannot be coalesced, forward as-is.
            None => {
                out.push(env);
                continue;
            }
        };

        if is_coalescable(&env) {
            let key = (
                env.namespace.clone().unwrap_or_default(),
                env.name.clone(),
                uuid,
            );
            if let Some(&idx) = pending.get(&key) {
                merge_update(&mut out[idx], env);
            } else {
                pending.insert(key, out.len());
                out.push(env);
            }
        } else {
            // Ordering barrier for this object.
            pending.retain(|(_, _, u), _| u != &uuid);
            out.push(env);
        }
    }

    out
}

/// Maintains a persistent WebSocket connection to `url`.
/// - Reads messages from `rx` and forwards them as WS text frames.
/// - Reads WS text frames and re-emits them into the Horizon event system.
/// - Reconnects automatically with a RECONNECT_DELAY on any error or disconnect.
async fn run_service_connection(
    name: String,
    url: String,
    mut rx: mpsc::Receiver<BridgeEventEnvelope>,
    events: Arc<EventSystem>,
) {
    // Only flips to true after the first *successful* connection completes.
    // Failed connect_async attempts do not change it, so a service that is
    // unreachable at Horizon startup still receives is_reconnection: false
    // on its first real handshake.
    let mut is_reconnection = false;

    loop {
        info!(service = %name, "connecting to {}", url);
        match connect_async(&url).await {
            Err(e) => {
                error!(service = %name, "connection failed: {}", e);
            }
            Ok((ws_stream, _)) => {
                info!(service = %name, "connected");
                let (mut sink, mut stream) = ws_stream.split();

                // ── Handshake ─────────────────────────────────────────────────
                // Tell the external service whether Horizon just started fresh
                // (is_reconnection: false) or is recovering a lost session
                // (is_reconnection: true). Sent before any event traffic.
                let handshake = BridgeEventEnvelope {
                    event_type: "bridge".to_string(),
                    namespace: None,
                    name: "connected".to_string(),
                    payload: json!({ "is_reconnection": is_reconnection }),
                };
                match serde_json::to_string(&handshake) {
                    Ok(text) => {
                        if let Err(e) = sink.send(Message::Text(text.into())).await {
                            error!(service = %name, "failed to send connection handshake: {}", e);
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            continue;
                        }
                        debug!(service = %name, is_reconnection, "sent connection handshake");
                    }
                    Err(e) => {
                        error!(service = %name, "failed to serialise connection handshake: {}", e);
                    }
                }
                // All future connections for this service are reconnections.
                is_reconnection = true;
                // ─────────────────────────────────────────────────────────────

                let mut closed = false;
                // Reused across drains to avoid reallocating every batch.
                let mut drain_buf: Vec<BridgeEventEnvelope> = Vec::with_capacity(MAX_DRAIN_BATCH);

                loop {
                    tokio::select! {
                        // Incoming message from external service → re-emit into Horizon.
                        msg = stream.next() => {
                            match msg {
                                None => {
                                    info!(service = %name, "WebSocket stream closed");
                                    closed = true;
                                    break;
                                }
                                Some(Err(e)) => {
                                    error!(service = %name, "WebSocket read error: {}", e);
                                    closed = true;
                                    break;
                                }
                                Some(Ok(Message::Text(text))) => {
                                    info!(service = %name, "← received message from service ({} bytes)", text.len());
                                    handle_incoming(&name, text.as_str(), &events).await;
                                }
                                Some(Ok(Message::Close(_))) => {
                                    info!(service = %name, "received Close frame");
                                    closed = true;
                                    break;
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    // Respond to pings to keep connection alive.
                                    if let Err(e) = sink.send(Message::Pong(data)).await {
                                        error!(service = %name, "pong send error: {}", e);
                                        closed = true;
                                        break;
                                    }
                                }
                                Some(Ok(_)) => {} // Binary / Pong / other — ignore
                            }
                        }
                        // Outgoing envelopes from Horizon → forward to external service.
                        // Drained in batches so that redundant partial updates queued
                        // behind the previous write can be collapsed before hitting the
                        // socket. Each envelope is still sent as its own frame: the
                        // services parse exactly one envelope per message.
                        received = rx.recv_many(&mut drain_buf, MAX_DRAIN_BATCH) => {
                            if received == 0 {
                                // Channel closed (plugin shutting down).
                                info!(service = %name, "outgoing channel closed, stopping");
                                let _ = sink.send(Message::Close(None)).await;
                                return;
                            }

                            let batch = coalesce_batch(std::mem::take(&mut drain_buf));
                            if received > batch.len() {
                                debug!(
                                    service = %name,
                                    drained = received,
                                    sent = batch.len(),
                                    "coalesced redundant updates in batch"
                                );
                            }

                            let mut write_failed = false;
                            for env in batch {
                                if env.namespace.as_deref() == Some("bridge_persistence") && env.name == "player_spawn" {
                                    let object_uuid = env.payload
                                        .get("object_uuid")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown");
                                    info!(
                                        service = %name,
                                        player_uuid = %object_uuid,
                                        "bridge outgoing: forwarding bridge_persistence:player_spawn to service"
                                    );
                                }
                                match serde_json::to_string(&env) {
                                    Ok(text) => {
                                        debug!(
                                            service = %name,
                                            event = %env.name,
                                            namespace = ?env.namespace,
                                            "→ forwarding event to service"
                                        );
                                        if let Err(e) = sink.send(Message::Text(text.into())).await {
                                            error!(service = %name, "WebSocket write error: {}", e);
                                            // Re-queue is not possible once sink is broken;
                                            // the event is dropped and we reconnect.
                                            write_failed = true;
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        error!(service = %name, "envelope serialisation error: {}", e);
                                    }
                                }
                            }
                            if write_failed {
                                closed = true;
                                break;
                            }
                        }
                    }
                }

                if closed {
                    info!(service = %name, "disconnected — reconnecting in {}s", RECONNECT_DELAY.as_secs());
                }
            }
        }

        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Parse a JSON text frame received from an external service and re-emit the
/// contained event into the Horizon event system.
async fn handle_incoming(service_name: &str, text: &str, events: &Arc<EventSystem>) {
    let envelope: BridgeEventEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                service = %service_name,
                "received invalid JSON envelope: {} — raw: {}",
                e,
                &text[..text.len().min(200)]
            );
            return;
        }
    };

    info!(
        service = %service_name,
        event_type = %envelope.event_type,
        namespace = ?envelope.namespace,
        event = %envelope.name,
        "← re-emitting event from service into Horizon"
    );

    if envelope.namespace.as_deref() == Some("genericprops")
        && (envelope.name == "items_chunk" || envelope.name == "items_end")
    {
        info!(
            service = %service_name,
            event = %envelope.name,
            "bridge incoming: persistence payload routed to genericprops"
        );
    }

    let result = match envelope.event_type.as_str() {
        "core" => {
            events.emit_core(&envelope.name, &envelope.payload).await
        }
        "plugin" => {
            let ns = envelope.namespace.as_deref().unwrap_or("");
            events.emit_plugin(ns, &envelope.name, &envelope.payload).await
        }
        other => {
            warn!(
                service = %service_name,
                "unknown event_type {:?} in incoming envelope, dropping",
                other
            );
            return;
        }
    };

    if let Err(e) = result {
        error!(
            service = %service_name,
            event = %envelope.name,
            "failed to re-emit incoming event: {}",
            e
        );
    } else {
        info!(
            service = %service_name,
            event = %envelope.name,
            "re-emitted incoming event successfully"
        );
    }
}

create_simple_plugin!(DyingstarBridgePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, uuid: &str, data: serde_json::Value) -> BridgeEventEnvelope {
        BridgeEventEnvelope {
            event_type: "plugin".to_string(),
            namespace: Some("genericprops".to_string()),
            name: name.to_string(),
            payload: json!({
                "object_type": "box50cm",
                "object_uuid": uuid,
                "object_data": data,
            }),
        }
    }

    fn data_of(e: &BridgeEventEnvelope) -> &serde_json::Value {
        e.payload.get("object_data").unwrap()
    }

    #[test]
    fn merges_updates_for_same_object_and_preserves_unmentioned_fields() {
        let batch = vec![
            env("update_object", "u1", json!({ "position": {"x": 1}, "hp": 10 })),
            env("update_object", "u1", json!({ "rotation": {"y": 2} })),
            env("update_object", "u1", json!({ "position": {"x": 9} })),
        ];
        let out = coalesce_batch(batch);
        assert_eq!(out.len(), 1, "three updates for one object collapse to one");
        // Newest position wins, rotation is kept, hp from the first survives.
        assert_eq!(
            data_of(&out[0]),
            &json!({ "position": {"x": 9}, "hp": 10, "rotation": {"y": 2} })
        );
    }

    #[test]
    fn does_not_merge_across_different_objects() {
        let batch = vec![
            env("update_object", "u1", json!({ "hp": 1 })),
            env("update_object", "u2", json!({ "hp": 2 })),
        ];
        assert_eq!(coalesce_batch(batch).len(), 2);
    }

    #[test]
    fn create_and_delete_are_ordering_barriers() {
        // update, delete, update for the same object must NOT collapse into one:
        // the second update must stay after the delete.
        let batch = vec![
            env("update_object", "u1", json!({ "hp": 1 })),
            env("delete_object", "u1", json!({})),
            env("update_object", "u1", json!({ "hp": 2 })),
        ];
        let out = coalesce_batch(batch);
        assert_eq!(out.len(), 3, "updates must not reorder across a delete");
        assert_eq!(out[0].name, "update_object");
        assert_eq!(out[1].name, "delete_object");
        assert_eq!(out[2].name, "update_object");
        assert_eq!(data_of(&out[2]), &json!({ "hp": 2 }));
    }

    #[test]
    fn non_coalescable_events_pass_through_untouched() {
        let batch = vec![
            env("create_object", "u1", json!({ "hp": 1 })),
            env("create_object", "u1", json!({ "hp": 2 })),
        ];
        assert_eq!(coalesce_batch(batch).len(), 2, "creates are never merged");
    }

    #[test]
    fn different_event_names_do_not_merge_together() {
        let batch = vec![
            env("update_object", "u1", json!({ "hp": 1 })),
            env("update_object_from_external", "u1", json!({ "hp": 2 })),
        ];
        assert_eq!(coalesce_batch(batch).len(), 2);
    }

    #[test]
    fn envelopes_without_object_uuid_pass_through() {
        let mut e = env("update_object", "u1", json!({ "hp": 1 }));
        e.payload = json!({ "no_uuid": true });
        let batch = vec![e.clone(), e];
        assert_eq!(coalesce_batch(batch).len(), 2);
    }

    #[test]
    fn order_is_preserved_for_interleaved_objects() {
        let batch = vec![
            env("update_object", "u1", json!({ "a": 1 })),
            env("update_object", "u2", json!({ "b": 1 })),
            env("update_object", "u1", json!({ "a": 2 })),
        ];
        let out = coalesce_batch(batch);
        assert_eq!(out.len(), 2);
        // u1 keeps its original slot (index 0), merged to the newest value.
        assert_eq!(object_uuid(&out[0]).unwrap(), "u1");
        assert_eq!(data_of(&out[0]), &json!({ "a": 2 }));
        assert_eq!(object_uuid(&out[1]).unwrap(), "u2");
    }

    #[test]
    fn empty_batch_is_empty() {
        assert!(coalesce_batch(vec![]).is_empty());
    }
}

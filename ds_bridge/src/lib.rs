use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use horizon_event_system::{
    create_simple_plugin, EventSystem, PluginError, ServerContext, SimplePlugin,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

mod config;
use config::{BridgeConfig, BridgeEventEnvelope};

// Channel buffer: enough to absorb bursts without blocking event handlers.
const CHANNEL_CAPACITY: usize = 256;
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

                match parts.as_slice() {
                    ["core", event_name] => {
                        let event_name = event_name.to_string();
                        let event_name_key = event_name.clone();
                        let service_name = service.name.clone();
                        events
                            .on_core(&event_name_key, move |payload: serde_json::Value| {
                                let envelope = BridgeEventEnvelope {
                                    event_type: "core".to_string(),
                                    namespace: None,
                                    name: event_name.clone(),
                                    payload,
                                };
                                if let Err(e) = sender.try_send(envelope) {
                                    warn!(
                                        service = %service_name,
                                        "outgoing channel full or closed: {}",
                                        e
                                    );
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
                        let service_name = service.name.clone();
                        events
                            .on_plugin(&plugin_ns_key, &event_name_key, move |payload: serde_json::Value| {
                                let envelope = BridgeEventEnvelope {
                                    event_type: "plugin".to_string(),
                                    namespace: Some(plugin_ns.clone()),
                                    name: event_name.clone(),
                                    payload,
                                };
                                if let Err(e) = sender.try_send(envelope) {
                                    warn!(
                                        service = %service_name,
                                        "outgoing channel full or closed: {}",
                                        e
                                    );
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
                        // Outgoing envelope from Horizon → forward to external service.
                        envelope = rx.recv() => {
                            match envelope {
                                None => {
                                    // Channel closed (plugin shutting down).
                                    info!(service = %name, "outgoing channel closed, stopping");
                                    let _ = sink.send(Message::Close(None)).await;
                                    return;
                                }
                                Some(env) => {
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
                                                closed = true;
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            error!(service = %name, "envelope serialisation error: {}", e);
                                        }
                                    }
                                }
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

//! Distance-based outbound rate limiting for GORC channel payloads.
//!
//! GORC only knows a binary rule: a player inside a channel's `distance` is
//! subscribed and receives every payload the server emits, a player outside
//! receives nothing at all. The `frequency` declared in `*_def.json` was never
//! enforced anywhere — `ReplicationLayer::update_interval()` has no callers in
//! Horizon, and the `needs_update` / `last_updates` bookkeeping on
//! `ObjectInstance` is written but never consumed.
//!
//! This module supplies the missing half without touching the Horizon submodule.
//! Handlers hand payloads to [`queue`] instead of emitting them directly, and a
//! background ticker delivers the newest queued payload to each recipient at the
//! rate its distance to the object earns it (the `lod` ladder of the channel).
//!
//! Throttling is safe here because every payload is a **full snapshot** of the
//! channel, never a delta, so dropping intermediate versions only costs
//! freshness. The per-recipient `(version, sent_at)` bookkeeping guarantees the
//! *last* version always goes out eventually: an object that stops moving never
//! leaves a client stuck on a stale position, which a plain "too soon, drop it"
//! throttle would.
//!
//! The one exception is a key a handler sends **once**, on change, rather than
//! in every payload — `parent_id` on a player `move`, which only the packet of
//! the reparent carries. Overwritten by the next position before the tick
//! delivered it, that packet was simply lost, and the client then applied
//! planet-local coordinates under the building it still believed it was in.
//! [`queue_sticky`] names such keys: the stream remembers their last value and
//! every recipient that has not yet been served a payload carrying it gets it
//! merged into whatever payload it is served next. Nobody else pays for it —
//! the key goes out once per recipient and per change, exactly as if the
//! original packet had been delivered.
//!
//! Recipients are computed here rather than read from
//! `ObjectInstance::subscribers` on purpose: `GorcInstanceManager::get_object`
//! deep-clones the whole instance (boxed object, zone manager, subscriber sets)
//! and doing that on every tick for every moving object is far more expensive
//! than re-evaluating the radius predicate. `find_players_in_radius` applies the
//! exact same test against the exact same `player_positions` map that GORC's own
//! subscription sweep uses, so the two agree.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use horizon_event_system::{current_timestamp, EventSystem, GorcObjectId, PlayerId};
use serde_json::json;
use tracing::{debug, error, info};

use crate::objectdefinition::{LodTier, ObjectDefinition};

/// How often the delivery loop wakes up.
///
/// This is the quantisation grid of every rate in the definitions, so it has to
/// stay well below the shortest interval in play (33 ms for the common 30 Hz
/// channels). It is not itself a send rate: a tick with nothing due costs one
/// pass over the pending streams.
const TICK: Duration = Duration::from_millis(10);

/// How long a fully-delivered stream is kept around after its last update.
///
/// The per-recipient send timestamps live in the stream, so dropping it the
/// moment everyone is up to date would reset the rate limiter and let the very
/// next update through immediately — which for a continuously moving object is
/// most updates. Retention has to exceed the longest interval any ladder can
/// ask for; 30 s covers every frequency down to 0.1 Hz with room to spare.
const IDLE_RETENTION: Duration = Duration::from_secs(30);

/// Ticks between two sweeps for expired streams. Cheap enough at this spacing
/// that it need not be smarter than a full pass over the map.
const GC_EVERY_TICKS: u64 = 500;

/// Ticks between two `[lod]` report lines (10 s).
const REPORT_EVERY_TICKS: u64 = 1000;

/// What the delivery loop did since the last report, per event name. Queued
/// counts come from the handlers' threads (atomics); the rest is the loop's own.
#[derive(Default)]
struct Counters {
    /// Streams evaluated (pending and due) by the tick.
    evaluated: u64,
    /// Radius queries run for those evaluations.
    radius_scans: u64,
    /// Recipients considered over all evaluations (audience size, summed).
    recipients: u64,
    /// Messages handed to `send_to_client`, and their bytes.
    sent: HashMap<String, u64>,
    bytes: u64,
    /// Largest audience a single evaluation saw.
    max_recipients: u64,
}

fn queued_counters() -> &'static DashMap<String, AtomicU64> {
    static QUEUED: OnceLock<DashMap<String, AtomicU64>> = OnceLock::new();
    QUEUED.get_or_init(DashMap::new)
}

fn count_queued(event_name: &str) {
    match queued_counters().get(event_name) {
        Some(counter) => {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            queued_counters().entry(event_name.to_string()).or_default().fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One `[lod]` line every `REPORT_EVERY_TICKS`: the numbers to compare a load
/// test against (queued updates in, messages out, audience size). Rates are per
/// second so runs of different lengths compare directly.
fn report(counters: &mut Counters, streams_total: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64().max(f64::EPSILON);
    let mut queued: Vec<String> = queued_counters()
        .iter()
        .map(|entry| format!("{}={:.0}/s", entry.key(), entry.value().swap(0, Ordering::Relaxed) as f64 / secs))
        .collect();
    queued.sort();
    let mut sent: Vec<String> = counters
        .sent
        .iter()
        .map(|(name, n)| format!("{}={:.0}/s", name, *n as f64 / secs))
        .collect();
    sent.sort();
    let sent_total: u64 = counters.sent.values().sum();
    info!(
        "[lod] queued [{}] | streams={} evaluated={:.0}/s radius_scans={:.0}/s | sent [{}] total={:.0}/s {:.1} KB/s | recipients/eval avg={:.1} max={}",
        queued.join(" "),
        streams_total,
        counters.evaluated as f64 / secs,
        counters.radius_scans as f64 / secs,
        sent.join(" "),
        sent_total as f64 / secs,
        counters.bytes as f64 / secs / 1024.0,
        if counters.evaluated == 0 { 0.0 } else { counters.recipients as f64 / counters.evaluated as f64 },
        counters.max_recipients,
    );
    *counters = Counters::default();
}

/// Identifies one payload stream: a channel of an object, per event name.
///
/// The event name is part of the key because a single channel carries several
/// unrelated event types (`move` for players, `update_property` for props) and
/// coalescing them into one another would silently drop data.
type StreamKey = (GorcObjectId, u8, String);

struct Stream {
    /// Bumped by every [`queue`] call; recipients are chasing this number.
    version: u64,
    object_type: String,
    /// Client envelope, serialized once per update rather than once per recipient.
    bytes: Arc<Vec<u8>>,
    /// Same envelope with the sticky keys merged into the payload, for the
    /// recipients still owed them. `None` while no sticky key has been seen.
    bytes_sticky: Option<Arc<Vec<u8>>>,
    /// Last value of every sticky key ever queued on this stream.
    sticky: serde_json::Map<String, serde_json::Value>,
    /// Version at which a sticky key last changed. A recipient whose delivered
    /// version is older has never been served that value and gets `bytes_sticky`.
    sticky_version: u64,
    /// Last version delivered to each recipient, and when.
    sent: HashMap<PlayerId, (u64, Instant)>,
    /// Whether some recipient is still behind `version`. A settled stream is
    /// skipped by the tick for the cost of one map lookup — without it an
    /// object updated once would keep running radius queries until it expired.
    pending: bool,
    /// Earliest instant any recipient may be served again, or `None` when the
    /// stream has never been evaluated. Lets the tick skip streams outright
    /// instead of running radius queries for them.
    next_due: Option<Instant>,
    last_queued: Instant,
}

fn streams() -> &'static DashMap<StreamKey, Stream> {
    static STREAMS: OnceLock<DashMap<StreamKey, Stream>> = OnceLock::new();
    STREAMS.get_or_init(DashMap::new)
}

/// Queue a channel payload for rate-limited delivery to nearby clients.
///
/// Replaces a direct `emit_gorc_instance(..., Dest::Client)`. Cheap and
/// synchronous: it serializes the envelope once and returns. If a payload for
/// the same stream is still pending it is overwritten — latest wins, which is
/// exactly right for snapshots. A key that is not in every payload must be
/// declared through [`queue_sticky`] or it can be lost that way.
pub fn queue(
    gorc_id: GorcObjectId,
    channel: u8,
    object_type: &str,
    event_name: &str,
    data: &serde_json::Value,
) {
    queue_sticky(gorc_id, channel, object_type, event_name, data, &[]);
}

/// [`queue`], with `sticky_keys` naming the keys of `data` that are sent on
/// change only. Their last value is kept on the stream and merged into the
/// next payload served to each recipient that has not seen it yet, so a
/// one-shot key survives being overwritten before delivery. Keys absent from
/// `data` keep the value they had; `data` must be a JSON object for the merge
/// to apply.
pub fn queue_sticky(
    gorc_id: GorcObjectId,
    channel: u8,
    object_type: &str,
    event_name: &str,
    data: &serde_json::Value,
    sticky_keys: &[&str],
) {
    // One timestamp for both variants: they are the same update.
    let timestamp = current_timestamp();
    let Some(bytes) = serialize_envelope(gorc_id, channel, object_type, event_name, data, timestamp) else {
        return;
    };

    count_queued(event_name);
    let now = Instant::now();
    let key = (gorc_id, channel, event_name.to_string());
    let mut stream = streams().entry(key).or_insert_with(|| Stream {
        version: 0,
        object_type: object_type.to_string(),
        bytes: Arc::clone(&bytes),
        bytes_sticky: None,
        sticky: serde_json::Map::new(),
        sticky_version: 0,
        sent: HashMap::new(),
        pending: false,
        next_due: None,
        last_queued: now,
    });

    stream.version = stream.version.wrapping_add(1);
    stream.bytes = bytes;
    stream.pending = true;
    stream.last_queued = now;

    if let Some(payload) = data.as_object() {
        for sticky_key in sticky_keys {
            if let Some(value) = payload.get(*sticky_key) {
                if stream.sticky.get(*sticky_key) != Some(value) {
                    stream.sticky.insert(sticky_key.to_string(), value.clone());
                    stream.sticky_version = stream.version;
                }
            }
        }
        if stream.sticky.is_empty() {
            stream.bytes_sticky = None;
        } else if stream.sticky.iter().all(|(k, v)| payload.get(k) == Some(v)) {
            // The payload already carries every sticky value: one serialization
            // serves both variants.
            stream.bytes_sticky = Some(Arc::clone(&stream.bytes));
        } else {
            let mut merged = payload.clone();
            for (k, v) in &stream.sticky {
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
            stream.bytes_sticky = serialize_envelope(
                gorc_id,
                channel,
                object_type,
                event_name,
                &serde_json::Value::Object(merged),
                timestamp,
            );
        }
    }
}

/// The client envelope around a channel payload, serialized.
fn serialize_envelope(
    gorc_id: GorcObjectId,
    channel: u8,
    object_type: &str,
    event_name: &str,
    data: &serde_json::Value,
    timestamp: u64,
) -> Option<Arc<Vec<u8>>> {
    // Byte-for-byte the envelope Horizon's own emit_to_gorc_subscribers builds,
    // including the `player_id` field that actually carries the object id —
    // clients parse this shape and must not be able to tell the two apart.
    let envelope = json!({
        "event_type": event_name,
        "object_id": gorc_id.to_string(),
        "object_type": object_type,
        "channel": channel,
        "player_id": gorc_id.to_string(),
        "data": data,
        "timestamp": timestamp,
    });
    match serde_json::to_vec(&envelope) {
        Ok(bytes) => Some(Arc::new(bytes)),
        Err(e) => {
            error!("🚀 LOD: ❌ Failed to serialize {} payload for object {}: {}", event_name, gorc_id, e);
            None
        }
    }
}

/// Start the delivery loop. Call once, at plugin startup.
///
/// Runs on its own thread with its own current-thread Tokio runtime rather than
/// on the Luminal handle, for two reasons: Luminal has no timer driver, and the
/// server's `server_tick` event — the other available clock — disappears
/// entirely when `tick_interval_ms = 0`, which would silently stop all
/// replication. Everything the loop awaits (`RwLock`, `DashMap`, an `mpsc`
/// `try_send` behind `send_to_client`) is runtime-agnostic.
pub fn start(events: Arc<EventSystem>, definitions: Arc<DashMap<String, ObjectDefinition>>) {
    let spawned = std::thread::Builder::new()
        .name("genericprops-lod".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    error!("🚀 LOD: ❌ Failed to build delivery runtime: {} - channel updates will not be sent", e);
                    return;
                }
            };

            runtime.block_on(async move {
                let mut ticker = tokio::time::interval(TICK);
                // A stalled tick must not be repaid with a burst of catch-up
                // ticks; delivery is rate limited, replaying the backlog is
                // pointless work.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut ticks: u64 = 0;
                let mut counters = Counters::default();
                let mut last_report = Instant::now();
                loop {
                    ticker.tick().await;
                    flush(&events, &definitions, &mut counters).await;

                    ticks = ticks.wrapping_add(1);
                    if ticks % REPORT_EVERY_TICKS == 0 {
                        report(&mut counters, streams().len(), last_report.elapsed());
                        last_report = Instant::now();
                    }
                    if ticks % GC_EVERY_TICKS == 0 {
                        streams().retain(|_, stream| {
                            stream.pending || stream.last_queued.elapsed() <= IDLE_RETENTION
                        });
                    }
                }
            });
        });

    match spawned {
        Ok(_) => info!("🚀 LOD: delivery loop started ({} ms tick)", TICK.as_millis()),
        Err(e) => error!("🚀 LOD: ❌ Failed to start delivery loop: {} - channel updates will not be sent", e),
    }
}

async fn flush(events: &Arc<EventSystem>, definitions: &Arc<DashMap<String, ObjectDefinition>>, counters: &mut Counters) {
    let Some(gorc_instances) = events.get_gorc_instances() else {
        return;
    };
    let Some(sender) = events.get_client_response_sender() else {
        return;
    };

    let keys: Vec<StreamKey> = streams().iter().map(|entry| entry.key().clone()).collect();
    if keys.is_empty() {
        return;
    }

    let now = Instant::now();
    // Half a tick of tolerance. Without it a rate is rounded up to the next
    // multiple of TICK: a 30 Hz target (33.3 ms) on a 10 ms grid would only ever
    // fire at 40 ms, i.e. 25 Hz. With it the send lands on the nearest tick.
    let slack = TICK / 2;

    for key in keys {
        let (gorc_id, channel, event_name) = &key;
        let (gorc_id, channel) = (*gorc_id, *channel);

        // Snapshot what we need; the map is never held across an await.
        let (version, bytes, bytes_sticky, sticky_version, object_type) = {
            let Some(stream) = streams().get(&key) else {
                continue;
            };
            if !stream.pending {
                continue;
            }
            if stream.next_due.map_or(false, |due| now + slack < due) {
                continue;
            }
            (
                stream.version,
                Arc::clone(&stream.bytes),
                stream.bytes_sticky.clone(),
                stream.sticky_version,
                stream.object_type.clone(),
            )
        };

        let Some(tiers) = resolve_tiers(definitions, &object_type, channel) else {
            // Unreachable by construction: `object_type` is the definition's own
            // name and channels come from the same definition. Dropping the
            // stream is still the right answer over guessing a radius — an
            // unthrottled fallback would have no zone to respect and would go to
            // every player on the server.
            error!("🚀 LOD: ❌ No definition for {} channel {} - dropping payload", object_type, channel);
            streams().remove(&key);
            continue;
        };

        let Some(object_position) = gorc_instances.get_object_position(gorc_id).await else {
            // Object unregistered (deleted, or a player that disconnected).
            streams().remove(&key);
            continue;
        };

        // The outermost tier is the whole audience. Anything past it is out of
        // the channel's zone and gets nothing, exactly as before.
        let outer = tiers.last().copied().unwrap_or(LodTier { distance: 0.0, frequency: 0.0 });
        let recipients = gorc_instances.find_players_in_radius(object_position, outer.distance).await;
        counters.evaluated += 1;
        counters.radius_scans += 1;
        counters.recipients += recipients.len() as u64;
        counters.max_recipients = counters.max_recipients.max(recipients.len() as u64);

        // Innermost tier containing a recipient decides its rate.
        let mut rates: HashMap<PlayerId, f64> = HashMap::with_capacity(recipients.len());
        let mut unassigned = recipients;
        for tier in &tiers[..tiers.len().saturating_sub(1)] {
            if unassigned.is_empty() {
                break;
            }
            counters.radius_scans += 1;
            let inside: HashSet<PlayerId> = gorc_instances
                .find_players_in_radius(object_position, tier.distance)
                .await
                .into_iter()
                .collect();
            unassigned.retain(|player_id| {
                if inside.contains(player_id) {
                    rates.insert(*player_id, tier.frequency);
                    false
                } else {
                    true
                }
            });
        }
        for player_id in unassigned {
            rates.insert(player_id, outer.frequency);
        }

        // Recipients to serve now, and whether each is still owed the sticky
        // keys: never served, or served nothing since their last change.
        let mut due: Vec<(PlayerId, bool)> = Vec::new();
        {
            let Some(mut stream) = streams().get_mut(&key) else {
                continue;
            };
            // Forget players that left the zone. GORC's `gorc_zone_exited` core
            // event is commented out upstream, so there is no exit hook to hang
            // this on — pruning against the live audience is the cleanup.
            stream.sent.retain(|player_id, _| rates.contains_key(player_id));

            for (player_id, frequency) in &rates {
                let (send, owed_sticky) = match stream.sent.get(player_id) {
                    // Never served: a player that just entered the zone gets the
                    // current state immediately, on top of the `gorc_zone_enter`
                    // snapshot Horizon already sent it.
                    None => (true, true),
                    Some((sent_version, sent_at)) => (
                        *sent_version < version
                            && now.saturating_duration_since(*sent_at) + slack >= interval(*frequency),
                        *sent_version < sticky_version,
                    ),
                };
                if send {
                    due.push((*player_id, owed_sticky));
                }
            }

            // Marked before the send: a failed send is not worth retrying at a
            // higher rate than the channel allows, and the next version will
            // carry the state anyway.
            for (player_id, _) in &due {
                stream.sent.insert(*player_id, (version, now));
            }

            stream.next_due = rates
                .iter()
                .map(|(player_id, frequency)| match stream.sent.get(player_id) {
                    Some((_, sent_at)) => *sent_at + interval(*frequency),
                    None => now,
                })
                .min();

            // Settled once every current recipient holds this version — which
            // includes the "nobody is in range" case. A concurrent queue() that
            // bumped the version keeps the stream awake.
            if stream.version == version
                && rates
                    .keys()
                    .all(|player_id| stream.sent.get(player_id).map_or(false, |(v, _)| *v >= version))
            {
                stream.pending = false;
            }
        }

        *counters.sent.entry(event_name.clone()).or_default() += due.len() as u64;
        for (player_id, owed_sticky) in due {
            let payload = match (&bytes_sticky, owed_sticky) {
                (Some(sticky), true) => sticky,
                _ => &bytes,
            };
            counters.bytes += payload.len() as u64;
            if let Err(e) = sender.send_to_client(player_id, (**payload).clone()).await {
                debug!("🚀 LOD: send to player {} for object {} ch{} failed: {}", player_id, gorc_id, channel, e);
            }
        }
    }
}

fn interval(frequency: f64) -> Duration {
    if frequency.is_finite() && frequency > 0.0 {
        Duration::from_secs_f64(1.0 / frequency)
    } else {
        Duration::ZERO
    }
}

/// Rate ladder declared for a channel, innermost tier first.
///
/// Cloned per call rather than borrowed: the ladder is one or two elements and
/// holding a `DashMap` guard across the awaits that follow would put the
/// delivery loop in the way of every handler queueing a payload.
fn resolve_tiers(
    definitions: &Arc<DashMap<String, ObjectDefinition>>,
    object_type: &str,
    channel: u8,
) -> Option<Vec<LodTier>> {
    let definition = definitions.get(object_type)?;
    let channel = definition.channel(channel)?;
    Some(channel.lod.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_of(gorc_id: GorcObjectId) -> (u64, Arc<Vec<u8>>, Option<Arc<Vec<u8>>>, u64) {
        let stream = streams().get(&(gorc_id, 0, "move".to_string())).expect("stream queued");
        (stream.version, Arc::clone(&stream.bytes), stream.bytes_sticky.clone(), stream.sticky_version)
    }

    fn data_of(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["data"].clone()
    }

    #[test]
    fn sticky_key_survives_being_overwritten_before_delivery() {
        let id = GorcObjectId::new();
        // The one packet of a reparent...
        queue_sticky(id, 0, "player", "move", &json!({"position": 1, "parent_id": "planet"}), &["parent_id"]);
        let (v1, bytes1, sticky1, sv1) = stream_of(id);
        assert_eq!((v1, sv1), (1, 1));
        // ...already carries the value: both variants are the same buffer.
        assert!(Arc::ptr_eq(&bytes1, sticky1.as_ref().unwrap()));

        // ...overwritten by the next position, which does not.
        queue_sticky(id, 0, "player", "move", &json!({"position": 2}), &["parent_id"]);
        let (v2, bytes2, sticky2, sv2) = stream_of(id);
        assert_eq!((v2, sv2), (2, 1), "an unchanged sticky value does not bump sticky_version");
        assert_eq!(data_of(&bytes2), json!({"position": 2}));
        assert_eq!(data_of(sticky2.as_ref().unwrap()), json!({"position": 2, "parent_id": "planet"}));

        // A new frame bumps the version recipients are measured against.
        queue_sticky(id, 0, "player", "move", &json!({"position": 3, "parent_id": "city"}), &["parent_id"]);
        let (_, _, _, sv3) = stream_of(id);
        assert_eq!(sv3, 3);
        queue_sticky(id, 0, "player", "move", &json!({"position": 4}), &["parent_id"]);
        let (_, _, sticky4, _) = stream_of(id);
        assert_eq!(data_of(sticky4.as_ref().unwrap())["parent_id"], json!("city"));
        streams().remove(&(id, 0, "move".to_string()));
    }

    #[test]
    fn plain_queue_has_no_sticky_variant() {
        let id = GorcObjectId::new();
        queue(id, 0, "player", "move", &json!({"position": 1, "parent_id": "planet"}));
        queue(id, 0, "player", "move", &json!({"position": 2}));
        let (_, bytes, sticky, sv) = stream_of(id);
        assert!(sticky.is_none());
        assert_eq!(sv, 0);
        assert_eq!(data_of(&bytes), json!({"position": 2}));
        streams().remove(&(id, 0, "move".to_string()));
    }
}

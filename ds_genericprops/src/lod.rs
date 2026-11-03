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
//! the key goes out once per recipient and per declaration, exactly as if the
//! original packet had been delivered.
//!
//! A payload carrying [`RECEIVED_STAMP_KEY`] (the time Horizon read the update
//! from the game server) is also stamped with [`SENT_STAMP_KEY`] when it goes
//! out, so a client can tell the time spent here from the time spent on the
//! wire. Such a stream keeps its payload and is serialized again on each tick it
//! sends on — once for every recipient of that tick, not once per recipient.
//!
//! A stream queued with an owner ([`queue_sticky_owned`]: the player's own
//! `move`) serves that owner at the channel's base `frequency`, whatever the
//! ladder says: the ladder is for watching someone else, and the owner, at
//! distance 0, otherwise fell into its innermost rung — 15 Hz for its own
//! position (MovementProbe, 2026-10-22).
//!
//! Recipients are computed here rather than read from
//! `ObjectInstance::subscribers` on purpose: `GorcInstanceManager::get_object`
//! deep-clones the whole instance (boxed object, zone manager, subscriber sets)
//! and doing that on every tick for every moving object is far more expensive
//! than re-evaluating the radius predicate. The test is the one
//! `find_players_in_radius` applies (distance² <= radius²) against the same
//! `player_positions` map GORC's own subscription sweep uses, so the two agree —
//! but on one copy of that map per flush, bucketed in a [`PlayerGrid`]: one full
//! scan of every player per stream and per tier was the whole cost of the loop at
//! 1248 players (preprod 2026-10-04, the single delivery thread at 100 %).
//!
//! **Bitcode recipients** (`ds_protocol::props`) get each payload as two GORC
//! frames instead of the JSON envelope: the position and rotation as
//! `<event>/transform`, unreliable (datagrams on WebTransport), and the other keys
//! as `<event>`, reliable, only when there are any. The clocks of the trip ride with the transform, and `player_id` is left out
//! when it is the object's own id (the client has it from the zone entry). The
//! frames are encoded once per version, on the first bitcode recipient. Since a
//! datagram can be lost, a bitcode recipient served the last version gets its
//! transform [`TRANSFORM_REPEATS`] more times at the channel's rate: the "last
//! version always goes out eventually" guarantee, without a reliable send. Every
//! frame carries the object's [`epoch`], bumped when a sticky key (`parent_id`) is
//! declared, so the client never applies a transform under the wrong parent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use horizon_event_system::protocol::{bitcode, encode_server, EventKey, ServerFrame};
use horizon_event_system::{current_timestamp, ClientResponseSender, EventSystem, Format, GorcObjectId, Outbound, PlayerId, Vec3};
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

/// Payload key holding the time (unix ms) Horizon received the update from the
/// game server. Its presence asks for [`SENT_STAMP_KEY`] on delivery.
pub const RECEIVED_STAMP_KEY: &str = "horizon_received_ms";

/// Payload key set to the time (unix ms) the payload was handed to the client
/// connection, on streams whose payload carries [`RECEIVED_STAMP_KEY`].
pub const SENT_STAMP_KEY: &str = "horizon_sent_ms";

/// Time between two `[lod]` report lines. Measured in time, not in ticks: a loop
/// running late is exactly when the line is needed, and 1000 slow ticks once
/// kept it silent for minutes.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Side of a [`PlayerGrid`] cell: the outer radius of the player channel, so a
/// player's audience is the 27 cells around it.
const GRID_CELL: f64 = 200.0;

/// The players of one flush bucketed by position, so a radius query only looks at
/// the cells it overlaps instead of every player.
struct PlayerGrid {
    cells: HashMap<(i64, i64, i64), Vec<(PlayerId, Vec3)>>,
}

impl PlayerGrid {
    fn new(players: Vec<(PlayerId, Vec3)>) -> Self {
        let mut cells: HashMap<(i64, i64, i64), Vec<(PlayerId, Vec3)>> = HashMap::new();
        for (player_id, position) in players {
            cells.entry(Self::cell_of(position)).or_default().push((player_id, position));
        }
        Self { cells }
    }

    fn cell_of(position: Vec3) -> (i64, i64, i64) {
        (
            (position.x / GRID_CELL).floor() as i64,
            (position.y / GRID_CELL).floor() as i64,
            (position.z / GRID_CELL).floor() as i64,
        )
    }

    /// Calls `found` with each player within `radius` of `center` (inclusive, like
    /// `find_players_in_radius`) and its squared distance.
    fn within(&self, center: Vec3, radius: f64, mut found: impl FnMut(PlayerId, f64)) {
        let radius_sq = radius * radius;
        let mut visit = |players: &Vec<(PlayerId, Vec3)>| {
            for (player_id, position) in players {
                let distance_sq = position.distance_squared(center);
                if distance_sq <= radius_sq {
                    found(*player_id, distance_sq);
                }
            }
        };
        let span = (radius / GRID_CELL).ceil() as i64;
        let side = 2 * span + 1;
        // A huge radius (a 10 km village zone) spans more cells than are occupied:
        // walking the occupied ones is then the cheaper of the two.
        if side.saturating_mul(side).saturating_mul(side) > self.cells.len() as i64 {
            self.cells.values().for_each(|players| visit(players));
            return;
        }
        let (cx, cy, cz) = Self::cell_of(center);
        for x in cx - span..=cx + span {
            for y in cy - span..=cy + span {
                for z in cz - span..=cz + span {
                    if let Some(players) = self.cells.get(&(x, y, z)) {
                        visit(players);
                    }
                }
            }
        }
    }
}

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

/// One `[lod]` line every `REPORT_EVERY`: the numbers to compare a load
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
    // Movement intake ahead of the queue: `coalesced` above zero means the
    // handlers are slower than the game servers and packets are being replaced
    // before they are applied — the early sign of the overload that used to
    // end in an OOM (see player_movement::coalesce).
    let (moves_in, moves_coalesced, draining) = crate::handlers::player_movement::intake_report();
    info!(
        "[lod] moves in={:.0}/s coalesced={:.0}/s draining={} | queued [{}] | streams={} evaluated={:.0}/s radius_scans={:.0}/s | sent [{}] total={:.0}/s {:.1} KB/s | recipients/eval avg={:.1} max={}",
        moves_in as f64 / secs,
        moves_coalesced as f64 / secs,
        draining,
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
    /// Version at which a sticky key was last declared. A recipient whose delivered
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
    /// The player this object is (its own `move` stream), served at the
    /// channel's base frequency instead of the ladder's.
    owner: Option<PlayerId>,
    /// The payloads to stamp with [`SENT_STAMP_KEY`] on delivery, or `None`
    /// when the payload asks for no stamp and `bytes` go out as they are.
    stamped: Option<Stamped>,
    /// The payload, for the bitcode frames.
    data: Arc<serde_json::Value>,
    /// The payload with the sticky keys merged, when it differs from `data`.
    data_sticky: Option<Arc<serde_json::Value>>,
    /// The object's epoch when this version was queued.
    epoch: u32,
    /// Bitcode frames of `version`, encoded on the first bitcode recipient.
    bitcode: Option<(u64, BitcodeFrames)>,
    /// Transforms still to resend to bitcode recipients served the last version.
    repeats: HashMap<PlayerId, u8>,
}

/// What a stamped stream needs to serialize its envelope again at send time.
#[derive(Clone)]
struct Stamped {
    data: serde_json::Value,
    /// The payload with the sticky keys merged, when it differs from `data`.
    data_sticky: Option<serde_json::Value>,
    timestamp: u64,
}

/// Wall-clock time in unix milliseconds.
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `data` with [`SENT_STAMP_KEY`] set to `sent_ms` (unchanged if not an object).
fn with_sent_stamp(data: &serde_json::Value, sent_ms: u64) -> serde_json::Value {
    let mut stamped = data.clone();
    if let Some(map) = stamped.as_object_mut() {
        map.insert(SENT_STAMP_KEY.to_string(), json!(sent_ms));
    }
    stamped
}

/// Transforms resent, unreliably, to a bitcode recipient already served the last
/// version (see the module docs).
pub const TRANSFORM_REPEATS: u8 = 3;

/// Keys that travel with the transform of a bitcode frame instead of the reliable
/// keys: they change with every payload and only describe it.
const TRANSFORM_EXTRA_KEYS: [&str; 3] = ["servergodot_ms", RECEIVED_STAMP_KEY, SENT_STAMP_KEY];

fn epochs() -> &'static DashMap<GorcObjectId, u32> {
    static EPOCHS: OnceLock<DashMap<GorcObjectId, u32>> = OnceLock::new();
    EPOCHS.get_or_init(DashMap::new)
}

/// The object's epoch: how many times a sticky key (`parent_id`) was declared on
/// one of its streams. Carried by its bitcode frames and zone entries.
pub fn epoch(gorc_id: GorcObjectId) -> u32 {
    epochs().get(&gorc_id).map_or(0, |epoch| *epoch)
}

/// The frames of one version of a stream, for bitcode recipients.
#[derive(Clone, Default)]
struct BitcodeFrames {
    /// Position and rotation, unreliable.
    transform: Option<bytes::Bytes>,
    /// The other keys, reliable.
    update: Option<bytes::Bytes>,
    /// The other keys with the sticky ones merged, for the recipients owed them.
    update_sticky: Option<bytes::Bytes>,
}

/// What a stream needs to build its bitcode frames.
struct FrameInput<'a> {
    handle: u32,
    channel: u8,
    seq: u16,
    event: EventKey,
    transform_event: EventKey,
    epoch: u32,
    object_id: &'a str,
    data: &'a serde_json::Value,
    /// The payload with the sticky keys merged, when the stream has sticky keys.
    data_sticky: Option<&'a serde_json::Value>,
}

/// Splits a payload into its transform and its other keys (`None` when there are
/// none left). A payload that is not a JSON object goes whole into the other keys.
fn split_payload(data: &serde_json::Value, object_id: &str, epoch: u32) -> (ds_protocol::TransformUpdate, Option<serde_json::Value>) {
    let Some(map) = data.as_object() else {
        return (ds_protocol::TransformUpdate { epoch, ..Default::default() }, Some(data.clone()));
    };
    let (mut transform, mut rest) = ds_protocol::props::split(map);
    transform.epoch = epoch;
    if !transform.is_empty() {
        let extra: serde_json::Map<String, serde_json::Value> =
            TRANSFORM_EXTRA_KEYS.iter().filter_map(|key| rest.remove_entry(*key)).collect();
        if !extra.is_empty() {
            transform.extra = ds_protocol::DynValue::from(&serde_json::Value::Object(extra));
        }
        if rest.get("player_id").and_then(|v| v.as_str()) == Some(object_id) {
            rest.remove("player_id");
        }
    }
    (transform, (!rest.is_empty()).then(|| serde_json::Value::Object(rest)))
}

fn bitcode_frames(input: &FrameInput) -> BitcodeFrames {
    let frame = |event: &EventKey, payload: Vec<u8>| {
        encode_server(&ServerFrame::GorcEvent {
            handle: input.handle,
            channel: input.channel,
            seq: input.seq,
            event: event.clone(),
            payload,
        })
    };
    let update = |rest: Option<serde_json::Value>| {
        rest.map(|rest| {
            frame(&input.event, bitcode::encode(&ds_protocol::PropUpdate {
                epoch: input.epoch,
                data: ds_protocol::DynValue::from(&rest),
            }))
        })
    };
    let (transform, rest) = split_payload(input.data, input.object_id, input.epoch);
    let transform = (!transform.is_empty()).then(|| frame(&input.transform_event, bitcode::encode(&transform)));
    let update_plain = update(rest);
    let update_sticky = match input.data_sticky {
        None => None,
        Some(sticky) if std::ptr::eq(sticky, input.data) => update_plain.clone(),
        Some(sticky) => update(split_payload(sticky, input.object_id, input.epoch).1),
    };
    BitcodeFrames { transform, update: update_plain, update_sticky }
}

/// The listener format of `player_id`, looked up once per flush.
async fn format_of(
    sender: &(dyn ClientResponseSender + Send + Sync),
    formats: &mut HashMap<PlayerId, Format>,
    player_id: PlayerId,
) -> Format {
    if let Some(format) = formats.get(&player_id) {
        return *format;
    }
    let format = sender.connection_profile(player_id).await.map(|profile| profile.format).unwrap_or_default();
    formats.insert(player_id, format);
    format
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

/// Drop every stream of `gorc_id`. Call when the object is (re)registered: a
/// player reconnecting within [`IDLE_RETENTION`] gets the same id, and the
/// stream of its previous session would otherwise carry over its sticky values
/// (the old `parent_id`) and its per-recipient versions into the new one.
pub fn forget_object(gorc_id: GorcObjectId) {
    streams().retain(|(id, _, _), _| *id != gorc_id);
    epochs().remove(&gorc_id);
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
    enqueue(gorc_id, channel, object_type, event_name, data, sticky_keys, None);
}

/// [`queue_sticky`] for the stream of a player's own object: `owner` is served
/// at the channel's base frequency, everyone else by the ladder.
pub fn queue_sticky_owned(
    gorc_id: GorcObjectId,
    channel: u8,
    object_type: &str,
    event_name: &str,
    data: &serde_json::Value,
    sticky_keys: &[&str],
    owner: PlayerId,
) {
    enqueue(gorc_id, channel, object_type, event_name, data, sticky_keys, Some(owner));
}

fn enqueue(
    gorc_id: GorcObjectId,
    channel: u8,
    object_type: &str,
    event_name: &str,
    data: &serde_json::Value,
    sticky_keys: &[&str],
    owner: Option<PlayerId>,
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
        owner,
        stamped: None,
        data: Arc::new(serde_json::Value::Null),
        data_sticky: None,
        epoch: 0,
        bitcode: None,
        repeats: HashMap::new(),
    });

    stream.version = stream.version.wrapping_add(1);
    stream.bytes = bytes;
    stream.pending = true;
    stream.last_queued = now;
    stream.owner = owner;
    let stamp = data.get(RECEIVED_STAMP_KEY).is_some();
    let mut data_sticky: Option<serde_json::Value> = None;
    stream.data = Arc::new(data.clone());
    stream.data_sticky = None;
    stream.bitcode = None;
    if data.as_object().is_some_and(|payload| sticky_keys.iter().any(|key| payload.contains_key(*key))) {
        // A new parent: transforms of the previous epoch must not be applied under it.
        let mut epoch = epochs().entry(gorc_id).or_insert(0);
        *epoch = epoch.wrapping_add(1);
    }
    stream.epoch = epoch(gorc_id);

    if let Some(payload) = data.as_object() {
        for sticky_key in sticky_keys {
            // A sticky key is a DECLARATION, sent once, not a value to diff: the
            // same value again is still news to whoever has not been served it.
            // Comparing with the remembered value lost a reparent after a quick
            // reconnect (2026-10-13): the stream of the previous session still held
            // the same planet as `parent_id`, the version was not bumped, the
            // packet was overwritten before delivery, and the client applied
            // planet-local coordinates under its spawn apartment — in space.
            if let Some(value) = payload.get(*sticky_key) {
                stream.sticky.insert(sticky_key.to_string(), value.clone());
                stream.sticky_version = stream.version;
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
            let merged = serde_json::Value::Object(merged);
            stream.bytes_sticky = serialize_envelope(gorc_id, channel, object_type, event_name, &merged, timestamp);
            stream.data_sticky = Some(Arc::new(merged.clone()));
            if stamp {
                data_sticky = Some(merged);
            }
        }
    }
    stream.stamped = stamp.then(|| Stamped { data: data.clone(), data_sticky, timestamp });
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
                    if last_report.elapsed() >= REPORT_EVERY {
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

    // One copy of every player position for the whole flush, built on the first
    // stream that is due (most ticks have none).
    let mut grid: Option<PlayerGrid> = None;
    // The listener format of each recipient, looked up once per flush.
    let mut formats: HashMap<PlayerId, Format> = HashMap::new();

    let now = Instant::now();
    // Half a tick of tolerance. Without it a rate is rounded up to the next
    // multiple of TICK: a 30 Hz target (33.3 ms) on a 10 ms grid would only ever
    // fire at 40 ms, i.e. 25 Hz. With it the send lands on the nearest tick.
    let slack = TICK / 2;

    for key in keys {
        let (gorc_id, channel, event_name) = &key;
        let (gorc_id, channel) = (*gorc_id, *channel);

        // Snapshot what we need; the map is never held across an await.
        let (version, bytes, bytes_sticky, sticky_version, object_type, owner, stamped, data, data_sticky, epoch, cached_frames) = {
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
                stream.owner,
                stream.stamped.clone(),
                Arc::clone(&stream.data),
                stream.data_sticky.clone(),
                stream.epoch,
                stream.bitcode.as_ref().filter(|(v, _)| *v == stream.version).map(|(_, frames)| frames.clone()),
            )
        };

        let Some((tiers, base_frequency)) = resolve_tiers(definitions, &object_type, channel) else {
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
        // the channel's zone and gets nothing, exactly as before. The innermost
        // tier containing a recipient decides its rate.
        let outer = tiers.last().copied().unwrap_or(LodTier { distance: 0.0, frequency: 0.0 });
        let grid = grid.get_or_insert_with(|| PlayerGrid::new(gorc_instances.player_positions_snapshot()));
        let mut rates: HashMap<PlayerId, f64> = HashMap::new();
        grid.within(object_position, outer.distance, |player_id, distance_sq| {
            let frequency = tiers
                .iter()
                .find(|tier| distance_sq <= tier.distance * tier.distance)
                .map_or(outer.frequency, |tier| tier.frequency);
            rates.insert(player_id, owner_rate(owner, player_id, frequency, base_frequency));
        });
        counters.evaluated += 1;
        counters.radius_scans += 1;
        counters.recipients += rates.len() as u64;
        counters.max_recipients = counters.max_recipients.max(rates.len() as u64);

        // Recipients to serve now, and whether each is still owed the sticky
        // keys: never served, or served nothing since their last change.
        let mut due: Vec<(PlayerId, bool)> = Vec::new();
        // Bitcode recipients already served this version, owed a repeat of its transform.
        let mut repeat_due: Vec<PlayerId> = Vec::new();
        {
            let Some(mut stream) = streams().get_mut(&key) else {
                continue;
            };
            // Forget players that left the zone. GORC's `gorc_zone_exited` core
            // event is commented out upstream, so there is no exit hook to hang
            // this on — pruning against the live audience is the cleanup.
            stream.sent.retain(|player_id, _| rates.contains_key(player_id));
            stream.repeats.retain(|player_id, left| *left > 0 && rates.contains_key(player_id));

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
                } else if stream.repeats.contains_key(player_id)
                    && stream.sent.get(player_id).is_some_and(|(_, sent_at)| {
                        now.saturating_duration_since(*sent_at) + slack >= interval(*frequency)
                    })
                {
                    repeat_due.push(*player_id);
                }
            }

            // Marked before the send: a failed send is not worth retrying at a
            // higher rate than the channel allows, and the next version will
            // carry the state anyway.
            for (player_id, _) in &due {
                stream.sent.insert(*player_id, (version, now));
                // Served a new version: its repeats start over once it is sent.
                stream.repeats.remove(player_id);
            }
            for player_id in &repeat_due {
                if let Some((_, sent_at)) = stream.sent.get_mut(player_id) {
                    *sent_at = now;
                }
                if let Some(left) = stream.repeats.get_mut(player_id) {
                    *left -= 1;
                }
            }
            stream.repeats.retain(|_, left| *left > 0);

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
                && stream.repeats.is_empty()
                && rates
                    .keys()
                    .all(|player_id| stream.sent.get(player_id).map_or(false, |(v, _)| *v >= version))
            {
                stream.pending = false;
            }
        }

        *counters.sent.entry(event_name.clone()).or_default() += due.len() as u64;
        if due.is_empty() && repeat_due.is_empty() {
            continue;
        }
        let mut json_due: Vec<(PlayerId, bool)> = Vec::with_capacity(due.len());
        let mut bitcode_due: Vec<(PlayerId, bool)> = Vec::new();
        for (player_id, owed_sticky) in due {
            match format_of(&*sender, &mut formats, player_id).await {
                Format::Json => json_due.push((player_id, owed_sticky)),
                Format::Bitcode => bitcode_due.push((player_id, owed_sticky)),
            }
        }
        let sent_ms = unix_ms();
        // A stamped stream is serialized again now, with the time it leaves.
        let (bytes, bytes_sticky) = match &stamped {
            Some(stamped) if !json_due.is_empty() => {
                let envelope = |data: &serde_json::Value| {
                    serialize_envelope(gorc_id, channel, &object_type, event_name, &with_sent_stamp(data, sent_ms),
                        stamped.timestamp)
                };
                let Some(fresh) = envelope(&stamped.data) else { continue };
                let fresh_sticky = match (&bytes_sticky, &stamped.data_sticky) {
                    (None, _) => None,
                    (Some(_), Some(merged)) => envelope(merged),
                    (Some(_), None) => Some(Arc::clone(&fresh)),
                };
                (fresh, fresh_sticky)
            }
            _ => (bytes, bytes_sticky),
        };
        for (player_id, owed_sticky) in json_due {
            let payload = match (&bytes_sticky, owed_sticky) {
                (Some(sticky), true) => sticky,
                _ => &bytes,
            };
            counters.bytes += payload.len() as u64;
            if let Err(e) = sender.send_to_client(player_id, (**payload).clone()).await {
                debug!("🚀 LOD: send to player {} for object {} ch{} failed: {}", player_id, gorc_id, channel, e);
            }
        }

        if bitcode_due.is_empty() && repeat_due.is_empty() {
            continue;
        }
        let frames = match cached_frames {
            Some(frames) if stamped.is_none() => frames,
            _ => {
                let Some(handle) = gorc_instances.object_handle(gorc_id) else {
                    debug!("🚀 LOD: object {} has no wire handle, bitcode recipients skipped", gorc_id);
                    continue;
                };
                // A stamped payload leaves with the time it leaves, as for JSON.
                let (data, data_sticky) = if stamped.is_some() {
                    (Arc::new(with_sent_stamp(&data, sent_ms)), data_sticky.map(|d| Arc::new(with_sent_stamp(&d, sent_ms))))
                } else {
                    (data, data_sticky)
                };
                let object_id = gorc_id.to_string();
                let frames = bitcode_frames(&FrameInput {
                    handle,
                    channel,
                    seq: gorc_instances.next_sequence(gorc_id, channel),
                    event: events.event_table().key(event_name),
                    transform_event: events.event_table().key(&ds_protocol::transform_event(event_name)),
                    epoch,
                    object_id: &object_id,
                    data: &data,
                    data_sticky: match (&bytes_sticky, &data_sticky) {
                        (None, _) => None,
                        (Some(_), Some(merged)) => Some(merged),
                        (Some(_), None) => Some(&data),
                    },
                });
                if stamped.is_none() {
                    if let Some(mut stream) = streams().get_mut(&key) {
                        if stream.version == version {
                            stream.bitcode = Some((version, frames.clone()));
                        }
                    }
                }
                frames
            }
        };
        for (player_id, owed_sticky) in &bitcode_due {
            let update = if *owed_sticky { frames.update_sticky.as_ref().or(frames.update.as_ref()) } else { frames.update.as_ref() };
            if let Some(update) = update {
                counters.bytes += update.len() as u64;
                if let Err(e) = sender.send_frame(*player_id, Outbound::reliable(update.clone())).await {
                    debug!("🚀 LOD: send to player {} for object {} ch{} failed: {}", player_id, gorc_id, channel, e);
                }
            }
            if let Some(transform) = &frames.transform {
                counters.bytes += transform.len() as u64;
                let _ = sender.send_frame(*player_id, Outbound::unreliable(transform.clone())).await;
            }
        }
        if let Some(transform) = &frames.transform {
            for player_id in &repeat_due {
                counters.bytes += transform.len() as u64;
                let _ = sender.send_frame(*player_id, Outbound::unreliable(transform.clone())).await;
            }
            if !bitcode_due.is_empty() {
                if let Some(mut stream) = streams().get_mut(&key) {
                    for (player_id, _) in &bitcode_due {
                        stream.repeats.insert(*player_id, TRANSFORM_REPEATS);
                    }
                    stream.pending = true;
                }
            }
        }
    }
}

/// The rate `player_id` is served at: the ladder's `frequency`, or for the
/// object's `owner` the channel's `base_frequency` when that is higher.
fn owner_rate(owner: Option<PlayerId>, player_id: PlayerId, frequency: f64, base_frequency: f64) -> f64 {
    if owner == Some(player_id) {
        frequency.max(base_frequency)
    } else {
        frequency
    }
}

fn interval(frequency: f64) -> Duration {
    if frequency.is_finite() && frequency > 0.0 {
        Duration::from_secs_f64(1.0 / frequency)
    } else {
        Duration::ZERO
    }
}

/// Rate ladder declared for a channel, innermost tier first, and the channel's
/// base frequency.
///
/// Cloned per call rather than borrowed: the ladder is one or two elements and
/// holding a `DashMap` guard across the awaits that follow would put the
/// delivery loop in the way of every handler queueing a payload.
fn resolve_tiers(
    definitions: &Arc<DashMap<String, ObjectDefinition>>,
    object_type: &str,
    channel: u8,
) -> Option<(Vec<LodTier>, f64)> {
    let definition = definitions.get(object_type)?;
    let channel = definition.channel(channel)?;
    Some((channel.lod.clone(), channel.frequency))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_grid_finds_what_a_full_scan_finds() {
        // Players spread over a few km, some on cell edges and in negative cells.
        let mut players = Vec::new();
        let mut seed: u64 = 7;
        for _ in 0..500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let coord = |shift: u32| ((seed >> shift) % 3000) as f64 - 1500.0;
            players.push((PlayerId::new(), Vec3::new(coord(1), coord(21), coord(41))));
        }
        players.push((PlayerId::new(), Vec3::new(200.0, -200.0, 0.0)));
        let grid = PlayerGrid::new(players.clone());

        for (center, radius) in [
            (Vec3::new(0.0, 0.0, 0.0), 30.0),
            (Vec3::new(10.0, -390.0, 5.0), 200.0),
            (Vec3::new(0.0, 0.0, 0.0), 282.9), // reaches the edge player exactly
            (Vec3::new(-1400.0, 1400.0, 0.0), 450.0),
            (Vec3::new(0.0, 0.0, 0.0), 10_000.0), // falls back to the occupied cells
        ] {
            let mut expected: Vec<PlayerId> = players
                .iter()
                .filter(|(_, p)| p.distance_squared(center) <= radius * radius)
                .map(|(id, _)| *id)
                .collect();
            let mut found = Vec::new();
            grid.within(center, radius, |id, _| found.push(id));
            expected.sort_by_key(|id| id.to_string());
            found.sort_by_key(|id| id.to_string());
            assert_eq!(found, expected, "radius {} around {:?}", radius, center);
        }
    }

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
    fn a_repeated_sticky_value_is_declared_again() {
        let id = GorcObjectId::new();
        // Previous session: reparented onto the planet, delivered.
        queue_sticky(id, 0, "player", "move", &json!({"position": 1, "parent_id": "planet"}), &["parent_id"]);
        queue_sticky(id, 0, "player", "move", &json!({"position": 2}), &["parent_id"]);
        // New session: leaves the apartment for the same planet.
        queue_sticky(id, 0, "player", "move", &json!({"position": 3, "parent_id": "planet"}), &["parent_id"]);
        let (v3, _, _, sv3) = stream_of(id);
        assert_eq!(sv3, v3, "a recipient served version 2 must be owed the new declaration");
        streams().remove(&(id, 0, "move".to_string()));
    }

    #[test]
    fn forget_object_drops_every_stream_of_that_object_only() {
        let (id, other) = (GorcObjectId::new(), GorcObjectId::new());
        queue_sticky(id, 0, "player", "move", &json!({"position": 1, "parent_id": "planet"}), &["parent_id"]);
        queue(id, 1, "player", "update_property", &json!({"head": 0.5}));
        queue(other, 0, "player", "move", &json!({"position": 1}));
        forget_object(id);
        assert!(!streams().iter().any(|entry| entry.key().0 == id));
        assert!(streams().contains_key(&(other, 0, "move".to_string())));
        streams().remove(&(other, 0, "move".to_string()));
    }

    #[test]
    fn the_owner_is_served_at_the_base_rate_others_by_the_ladder() {
        let (owner, other) = (PlayerId::new(), PlayerId::new());
        assert_eq!(owner_rate(Some(owner), owner, 15.0, 30.0), 30.0);
        assert_eq!(owner_rate(Some(owner), other, 15.0, 30.0), 15.0);
        assert_eq!(owner_rate(None, owner, 15.0, 30.0), 15.0);
        assert_eq!(owner_rate(Some(owner), owner, 60.0, 30.0), 60.0, "never slower than the ladder");
    }

    #[test]
    fn a_received_stamp_keeps_the_payload_to_stamp_on_send() {
        let id = GorcObjectId::new();
        let owner = PlayerId::new();
        queue_sticky_owned(id, 0, "player", "move",
            &json!({"position": 1, "parent_id": "planet", RECEIVED_STAMP_KEY: 10}), &["parent_id"], owner);
        queue_sticky_owned(id, 0, "player", "move", &json!({"position": 2, RECEIVED_STAMP_KEY: 20}), &["parent_id"], owner);
        {
            let stream = streams().get(&(id, 0, "move".to_string())).expect("stream queued");
            assert_eq!(stream.owner, Some(owner));
            let stamped = stream.stamped.as_ref().expect("a received stamp asks for a sent one");
            assert_eq!(stamped.data[RECEIVED_STAMP_KEY], json!(20));
            assert_eq!(stamped.data_sticky.as_ref().unwrap()["parent_id"], json!("planet"));
        }
        assert_eq!(with_sent_stamp(&json!({"position": 2}), 30), json!({"position": 2, SENT_STAMP_KEY: 30}));
        queue(id, 0, "player", "move", &json!({"position": 3}));
        assert!(streams().get(&(id, 0, "move".to_string())).unwrap().stamped.is_none());
        streams().remove(&(id, 0, "move".to_string()));
    }

    fn decode_gorc(bytes: &bytes::Bytes) -> (u32, u16, EventKey, Vec<u8>) {
        match horizon_event_system::protocol::decode_server(bytes, usize::MAX).unwrap() {
            ServerFrame::GorcEvent { handle, seq, event, payload, .. } => (handle, seq, event, payload),
            other => panic!("not a GORC event: {:?}", other),
        }
    }

    fn input<'a>(data: &'a serde_json::Value, data_sticky: Option<&'a serde_json::Value>, object_id: &'a str) -> FrameInput<'a> {
        FrameInput {
            handle: 7,
            channel: 0,
            seq: 42,
            event: EventKey::name("move"),
            transform_event: EventKey::name(ds_protocol::transform_event("move")),
            epoch: 3,
            object_id,
            data,
            data_sticky,
        }
    }

    #[test]
    fn a_move_is_one_transform_frame_with_its_clocks() {
        let object_id = GorcObjectId::new().to_string();
        let data = json!({
            "player_id": object_id,
            "position": {"x": 1.0, "y": 2.0, "z": 3.0},
            "rotation": {"x": 0.0, "y": 0.5, "z": 0.0},
            "servergodot_ms": 12.5,
            RECEIVED_STAMP_KEY: 100,
            SENT_STAMP_KEY: 101,
        });
        let frames = bitcode_frames(&input(&data, None, &object_id));
        assert!(frames.update.is_none(), "nothing reliable to send: the clocks ride with the transform");
        let (handle, seq, event, payload) = decode_gorc(frames.transform.as_ref().unwrap());
        assert_eq!((handle, seq, event), (7, 42, EventKey::name("move/transform")));
        let transform: ds_protocol::TransformUpdate = bitcode::decode(&payload).unwrap();
        assert_eq!(transform.epoch, 3);
        assert_eq!(transform.position, Some([1.0, 2.0, 3.0]));
        assert_eq!(transform.extra.to_value().unwrap(),
            json!({"servergodot_ms": 12.5, RECEIVED_STAMP_KEY: 100, SENT_STAMP_KEY: 101}));
    }

    #[test]
    fn other_keys_are_a_reliable_update_and_sticky_ones_a_variant() {
        let object_id = GorcObjectId::new().to_string();
        let data = json!({"position": {"x": 1.0, "y": 2.0, "z": 3.0}, "action": "jump"});
        let sticky = json!({"position": {"x": 1.0, "y": 2.0, "z": 3.0}, "action": "jump", "parent_id": "city"});
        let frames = bitcode_frames(&input(&data, Some(&sticky), &object_id));
        assert!(frames.transform.is_some());
        let update = |bytes: &bytes::Bytes| {
            let (_, _, event, payload) = decode_gorc(bytes);
            assert_eq!(event, EventKey::name("move"));
            let update: ds_protocol::PropUpdate = bitcode::decode(&payload).unwrap();
            assert_eq!(update.epoch, 3);
            update.data.to_value().unwrap()
        };
        assert_eq!(update(frames.update.as_ref().unwrap()), json!({"action": "jump"}));
        assert_eq!(update(frames.update_sticky.as_ref().unwrap()), json!({"action": "jump", "parent_id": "city"}));
    }

    #[test]
    fn a_player_id_other_than_the_object_stays() {
        let object_id = GorcObjectId::new().to_string();
        let data = json!({"position": {"x": 1.0, "y": 2.0, "z": 3.0}, "player_id": "someone-else"});
        let (_, rest) = split_payload(&data, &object_id, 0);
        assert_eq!(rest, Some(json!({"player_id": "someone-else"})));
    }

    #[test]
    fn a_payload_without_transform_is_all_reliable() {
        let object_id = GorcObjectId::new().to_string();
        let data = json!({"led_state": 2, "servergodot_ms": 1});
        let frames = bitcode_frames(&input(&data, None, &object_id));
        assert!(frames.transform.is_none());
        let (_, _, _, payload) = decode_gorc(frames.update.as_ref().unwrap());
        let update: ds_protocol::PropUpdate = bitcode::decode(&payload).unwrap();
        assert_eq!(update.data.to_value().unwrap(), data, "without a transform, nothing moves to extra");
    }

    #[test]
    fn declaring_a_sticky_key_bumps_the_epoch() {
        let id = GorcObjectId::new();
        assert_eq!(epoch(id), 0);
        queue_sticky(id, 0, "player", "move", &json!({"position": 1, "parent_id": "planet"}), &["parent_id"]);
        queue_sticky(id, 0, "player", "move", &json!({"position": 2}), &["parent_id"]);
        assert_eq!(epoch(id), 1);
        queue_sticky(id, 0, "player", "move", &json!({"position": 3, "parent_id": "city"}), &["parent_id"]);
        assert_eq!(epoch(id), 2);
        assert_eq!(streams().get(&(id, 0, "move".to_string())).unwrap().epoch, 2);
        forget_object(id);
        assert_eq!(epoch(id), 0);
    }

    use horizon_event_system::{AuthenticationStatus, Delivery, GorcInstanceManager, ListenerProfile, Transport};
    use std::sync::Mutex;

    /// Records what the delivery loop sends, and answers each player's format.
    #[derive(Debug, Default)]
    struct Recorder {
        formats: HashMap<PlayerId, Format>,
        frames: Mutex<Vec<(PlayerId, Delivery, Vec<u8>)>>,
    }

    impl Recorder {
        fn sent_to(&self, player_id: PlayerId) -> Vec<(Delivery, Vec<u8>)> {
            let frames = self.frames.lock().unwrap();
            frames.iter().filter(|(p, _, _)| *p == player_id).map(|(_, d, b)| (*d, b.clone())).collect()
        }
    }

    type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

    impl ClientResponseSender for Recorder {
        fn send_to_client(&self, player_id: PlayerId, data: Vec<u8>) -> Fut<'_, Result<(), String>> {
            self.frames.lock().unwrap().push((player_id, Delivery::Reliable, data));
            Box::pin(async { Ok(()) })
        }
        fn is_connection_active(&self, _: PlayerId) -> Fut<'_, bool> {
            Box::pin(async { true })
        }
        fn get_auth_status(&self, _: PlayerId) -> Fut<'_, Option<AuthenticationStatus>> {
            Box::pin(async { None })
        }
        fn kick(&self, _: PlayerId, _: Option<String>) -> Fut<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn send_frame(&self, player_id: PlayerId, frame: Outbound) -> Fut<'_, Result<(), String>> {
            self.frames.lock().unwrap().push((player_id, frame.delivery, frame.bytes.to_vec()));
            Box::pin(async { Ok(()) })
        }
        fn connection_profile(&self, player_id: PlayerId) -> Fut<'_, Option<ListenerProfile>> {
            let format = self.formats.get(&player_id).copied().unwrap_or_default();
            Box::pin(async move { Some(ListenerProfile::new(Transport::WebTransport).with_format(format)) })
        }
    }

    #[tokio::test]
    async fn bitcode_recipients_get_unreliable_transforms_then_repeats() {
        let (json_player, bitcode_player) = (PlayerId::new(), PlayerId::new());
        let recorder = Arc::new(Recorder {
            formats: HashMap::from([(json_player, Format::Json), (bitcode_player, Format::Bitcode)]),
            ..Default::default()
        });
        let gorc = Arc::new(GorcInstanceManager::new());
        let mut events = EventSystem::new();
        events.set_gorc_instances(gorc.clone());
        events.set_client_response_sender(recorder.clone());
        let events = Arc::new(events);

        let definition = ObjectDefinition::new("lod_test_box".to_string(), json!({"channels": [
            {"zone": 0, "distance": 50.0, "frequency": 100.0, "properties": ["position", "rotation", "led"]}
        ]}));
        let definitions = Arc::new(DashMap::new());
        definitions.insert("lod_test_box".to_string(), definition.clone());
        let gorc_id = GorcObjectId::new();
        let object = crate::genericprops::GenericProps::new(
            definition,
            json!({"position": {"x": 0.0, "y": 0.0, "z": 0.0}, "rotation": {"x": 0.0, "y": 0.0, "z": 0.0}, "led": 1}),
            gorc_id.to_string(),
        );
        gorc.register_object_with_uuid(object, Vec3::new(0.0, 0.0, 0.0), Some(gorc_id)).await;
        gorc.add_player(json_player, Vec3::new(1.0, 0.0, 0.0)).await;
        gorc.add_player(bitcode_player, Vec3::new(2.0, 0.0, 0.0)).await;

        let position = json!({"position": {"x": 1.0, "y": 0.0, "z": 0.0}, "rotation": {"x": 0.0, "y": 1.0, "z": 0.0}});
        queue(gorc_id, 0, "lod_test_box", "move", &position);
        let mut counters = Counters::default();
        flush(&events, &definitions, &mut counters).await;

        let json_frames = recorder.sent_to(json_player);
        assert_eq!(json_frames.len(), 1);
        assert_eq!(data_of(&json_frames[0].1), position, "JSON recipients get the envelope as before");

        let bitcode_frames = recorder.sent_to(bitcode_player);
        assert_eq!(bitcode_frames.len(), 1, "a move without other keys is a transform frame only");
        assert_eq!(bitcode_frames[0].0, Delivery::Unreliable);
        let (_, first_seq, _, _) = decode_gorc(&bytes::Bytes::from(bitcode_frames[0].1.clone()));

        // No new version: the transform is repeated at the channel's rate, then the stream settles.
        for _ in 0..TRANSFORM_REPEATS + 2 {
            tokio::time::sleep(Duration::from_millis(15)).await;
            flush(&events, &definitions, &mut counters).await;
        }
        let bitcode_frames = recorder.sent_to(bitcode_player);
        assert_eq!(bitcode_frames.len(), 1 + TRANSFORM_REPEATS as usize);
        assert!(bitcode_frames.iter().all(|(delivery, bytes)| {
            *delivery == Delivery::Unreliable && decode_gorc(&bytes::Bytes::from(bytes.clone())).1 == first_seq
        }));
        assert_eq!(recorder.sent_to(json_player).len(), 1, "JSON recipients get no repeat");
        assert!(!streams().get(&(gorc_id, 0, "move".to_string())).unwrap().pending);

        // A change of another key is a reliable update, with the new transform.
        queue(gorc_id, 0, "lod_test_box", "move", &json!({"position": {"x": 2.0, "y": 0.0, "z": 0.0}, "led": 0}));
        flush(&events, &definitions, &mut counters).await;
        let new_frames: Vec<_> = recorder.sent_to(bitcode_player).into_iter().skip(1 + TRANSFORM_REPEATS as usize).collect();
        assert_eq!(new_frames.iter().map(|(d, _)| *d).collect::<Vec<_>>(), vec![Delivery::Reliable, Delivery::Unreliable]);
        forget_object(gorc_id);
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

//! Bulk object transfer between Horizon and one Godot server, used when a server
//! starts (initial world dump), when zones are split and when they are merged.
//!
//! Every item carries its world (space / planet + local position), resolved by
//! ds_genericprops; membership is `zones_contain(zones, world)`. Its properties stay
//! JSON text, written once by ds_genericprops and copied into the message to Godot.
//! Planets and stars are worlds, not zone content: always sent, never frozen.
//! Only the items inside the zones are sent: the Godot server keeps no copy of what
//! another server simulates (its PropRegistry forgets out-of-zone items).

use ds_common::events::{GenericPropsRequest, SnapshotItem};
use ds_common::world::ObjectWorld;
use ds_common::zone::{is_world_object, zones_contain, zones_label, Zone};
use horizon_event_system::EventError;
use serde::Serialize;
use serde_json::value::{to_raw_value, RawValue};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use super::{send_ws, send_ws_text};
use crate::server::Server;

pub type ManagedObjects = Arc<Mutex<HashSet<String>>>;
pub type ManagedPlayers = Arc<Mutex<Vec<String>>>;

/// An object of a live message as Godot must see it: without the Horizon-internal keys.
pub fn item_on_wire(item: &GenericPropsRequest) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    ObjectWorld::strip_internal_keys(&mut value["object_data"]);
    value
}

#[derive(Serialize)]
struct WireItem<'a> {
    object_type: &'a str,
    object_uuid: &'a str,
    object_data: &'a RawValue,
    /// Omitted unless true: Godot reads it with a default, which a present `null` bypasses.
    #[serde(skip_serializing_if = "Option::is_none")]
    dormant: Option<bool>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    namespace: &'a str,
    event: &'a str,
    data: WireItem<'a>,
}

/// The message Godot gets for one object of a bulk transfer, its properties copied as they are.
fn wire_message(event: &str, item: &SnapshotItem, object_data: &RawValue, dormant: bool) -> Result<String, EventError> {
    let data = WireItem {
        object_type: &item.object_type,
        object_uuid: &item.object_uuid,
        object_data,
        dormant: dormant.then_some(true),
    };
    serde_json::to_string(&WireMessage { namespace: "server", event, data }).map_err(EventError::Serialization)
}

/// The properties Godot must see to create the item: as they are for a prop; for a
/// player, with what Horizon keeps of its init (move_clocks, roles), as spawn_player
/// gives the first spawn and each zone transfer. A player handed over by a split or a
/// merge is created from this: without it the new server lost the player's clocks and
/// roles. Parsed for the players only: a few hundred, not the whole world.
fn spawn_properties(item: &SnapshotItem) -> Cow<'_, RawValue> {
    if item.object_type != "player" {
        return Cow::Borrowed(&item.object_data);
    }
    let marked = serde_json::from_str::<Value>(item.object_data.get()).and_then(|mut data| {
        super::move_clocks::mark(&mut data, &item.object_uuid);
        super::player_roles::mark(&mut data, &item.object_uuid);
        to_raw_value(&data)
    });
    match marked {
        Ok(marked) => Cow::Owned(marked),
        Err(e) => {
            warn!("[initial_object] player {} sent without its clocks and roles: {}", item.object_uuid, e);
            Cow::Borrowed(&item.object_data)
        }
    }
}

fn is_member(item: &SnapshotItem, zones: &[Zone]) -> bool {
    zones_contain(zones, &item.world)
}

fn track(item: &SnapshotItem, server: &Server) {
    server.managed_objects.lock().unwrap().insert(item.object_uuid.clone());
    if item.object_type == "player" {
        // A hand-over decided by the ServerManager: this server owns the player now.
        crate::ownership::take(&item.object_uuid, &server.uuid, &server.managed_players);
        let parent = item.parent_id.clone().unwrap_or_default();
        server.player_parents.lock().unwrap().insert(item.object_uuid.clone(), parent);
        // Sent right after the player itself (same socket, same order): a driver keeps
        // the throttle they hold instead of having the engine brake stop them, a
        // sprinting player keeps running.
        let _ = super::player_action::replay_input(&item.object_uuid, &server.websocket_sender);
    }
}

fn untrack(item: &SnapshotItem, server: &Server) {
    server.managed_objects.lock().unwrap().remove(&item.object_uuid);
    if item.object_type == "player" {
        {
            let mut players = server.managed_players.lock().unwrap();
            if let Some(pos) = players.iter().position(|x| x == &item.object_uuid) {
                players.remove(pos);
            }
        }
        crate::ownership::release(&item.object_uuid, &server.uuid);
        server.player_parents.lock().unwrap().remove(&item.object_uuid);
    }
}

/// Sends the world objects and the items inside `zones` to the server as
/// `initial_object` (those items become managed); items outside `zones` are not sent
/// at all. Ends with `initial_object_end`.
///
/// Outside items used to be sent and then frozen, so Godot held a copy for
/// collisions. Godot now drops them on arrival (PropRegistry), so sending them only
/// cost bandwidth and JSON parsing, twice per item (initial_object + freeze_object),
/// for most of the world on every server.
pub fn handle_initial_object(
    items: &HashMap<String, SnapshotItem>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    info!("[initial_object] sending {} items, zones=[{}]", items.len(), zones_label(zones));
    let (mut sent, mut skipped) = (0usize, 0usize);
    for item in items.values() {
        if send_initial(item, server, zones)? { sent += 1 } else { skipped += 1 }
    }
    end_initial(server, sent, skipped)
}

/// How fast `handle_initial_object_paced` sends: `batch` objects, then `pause`.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub batch: usize,
    pub pause: std::time::Duration,
}

/// `handle_initial_object` spread over time, for a server that keeps simulating players
/// meanwhile: sent in one go, the 11235 objects of a merge froze the survivor 33 s with a
/// player on it, who fell through the ground (preprod, 2026-10-17).
pub async fn handle_initial_object_paced(
    items: &HashMap<String, SnapshotItem>,
    server: &Server,
    zones: &[Zone],
    pace: Pace,
) -> Result<(), EventError> {
    info!(
        "[initial_object] sending {} items, zones=[{}], {} every {:?}",
        items.len(), zones_label(zones), pace.batch, pace.pause
    );
    let (mut sent, mut skipped) = (0usize, 0usize);
    for item in items.values() {
        if send_initial(item, server, zones)? {
            sent += 1;
            if sent % pace.batch.max(1) == 0 {
                tokio::time::sleep(pace.pause).await;
            }
        } else {
            skipped += 1;
        }
    }
    end_initial(server, sent, skipped)
}

/// Sends one object of an initial load (world objects always, the rest when inside
/// `zones`) and tracks it as managed. False when it is outside the zones.
fn send_initial(item: &SnapshotItem, server: &Server, zones: &[Zone]) -> Result<bool, EventError> {
    let world_object = is_world_object(&item.object_type);
    if !world_object && !is_member(item, zones) {
        debug!("[initial_object] item {} ({}) is outside the zones, not sent", item.object_uuid, item.object_type);
        return Ok(false);
    }
    let message = wire_message("initial_object", item, &spawn_properties(item), false)?;
    send_ws_text(&server.websocket_sender, "initial_object", message)?;
    if !world_object {
        track(item, server);
    }
    Ok(true)
}

/// Closes an initial load (`initial_object_end`).
fn end_initial(server: &Server, sent: usize, skipped: usize) -> Result<(), EventError> {
    send_ws(&server.websocket_sender, "initial_object", &json!({
        "namespace": "server",
        "event": "initial_object_end",
        "data": {},
    }))?;
    info!(
        "[initial_object] done: sent={} outside the zones (not sent)={} players managed now: {:?}",
        sent, skipped, server.managed_players.lock().unwrap()
    );
    Ok(())
}

/// Sends the players and vehicles of a hand-over to `server` ASLEEP, during its
/// warm-up: Godot instantiates and places them (the expensive part: ~2.4 s of main
/// loop for 60 players) but keeps them inert — no messages, no input, not counted
/// — until `handle_activate_objects`. Not tracked: the releasing server still owns
/// and simulates them meanwhile. Items outside `zones` are not sent. Returns the
/// uuids sent.
pub fn handle_dormant_objects(
    items: &HashMap<String, SnapshotItem>,
    server: &Server,
    zones: &[Zone],
) -> Result<HashSet<String>, EventError> {
    let mut sent = HashSet::new();
    for item in items.values() {
        if is_world_object(&item.object_type) || !is_member(item, zones) {
            continue;
        }
        let message = wire_message("initial_object", item, &spawn_properties(item), true)?;
        send_ws_text(&server.websocket_sender, "initial_object", message)?;
        sent.insert(item.object_uuid.clone());
    }
    info!("[initial_object] {} asleep on {}: {} item(s)", zones_label(zones), server.server_name, sent.len());
    Ok(sent)
}

/// The switch of a hand-over: `activate_object` for each item (fresh state) — Godot
/// wakes what it holds asleep, or creates it the normal way — and the server owns
/// them from now on (tracked, like `handle_initial_object`). Items outside `zones`
/// are not sent.
pub fn handle_activate_objects(
    items: &HashMap<String, SnapshotItem>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    let mut sent = 0usize;
    for item in items.values() {
        if is_world_object(&item.object_type) || !is_member(item, zones) {
            continue;
        }
        let message = wire_message("activate_object", item, &spawn_properties(item), false)?;
        send_ws_text(&server.websocket_sender, "activate_object", message)?;
        track(item, server);
        sent += 1;
    }
    info!("[activate_object] {} woken on {}", sent, server.server_name);
    Ok(())
}

/// Drops on `server` the players of `uuids` it holds asleep (they left the zone or
/// the game during the warm-up): Godot's `freeze_object` frees an asleep copy.
pub fn handle_drop_dormant(items: &HashMap<String, SnapshotItem>, uuids: &[String], server: &Server) -> Result<(), EventError> {
    for uuid in uuids {
        match items.get(uuid) {
            Some(item) => send_ws_text(&server.websocket_sender, "freeze_object", wire_message("freeze_object", item, &item.object_data, false)?)?,
            None => send_ws(&server.websocket_sender, "freeze_object", &json!({
                "namespace": "server",
                "event": "freeze_object",
                "data": { "object_type": "player", "object_uuid": uuid, "object_data": {} },
            }))?,
        }
    }
    if !uuids.is_empty() {
        info!("[freeze_object] {} asleep player(s) dropped on {}", uuids.len(), server.server_name);
    }
    Ok(())
}

/// Sends only the world objects (planets, stars) of `items`: what an idle server of
/// the pool preloads so that being handed zones later is not a cold start.
pub fn handle_world_objects(items: &HashMap<String, SnapshotItem>, server: &Server) -> Result<(), EventError> {
    let mut sent = 0usize;
    for item in items.values().filter(|i| is_world_object(&i.object_type)) {
        send_ws_text(&server.websocket_sender, "initial_object", wire_message("initial_object", item, &item.object_data, false)?)?;
        sent += 1;
    }
    info!("[initial_object] {} preloaded {} world object(s) (planets/stars)", server.server_name, sent);
    Ok(())
}

/// Freezes on the server every non-world item that is NOT inside `zones` (pass an
/// empty slice to freeze everything) and forgets it from the managed sets.
pub fn handle_freeze_object(
    items: &HashMap<String, SnapshotItem>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    let websocket = server.websocket_sender.clone();
    info!("[freeze_object] checking {} items against zones=[{}]", items.len(), zones_label(zones));

    // Players first, then what seated players ride, then the rest: a split freezes
    // ~20k objects here, in no order, and the server erased a handed-over player
    // only when its turn came — 3 s later on preprod (2026-10-04), while the new
    // server already simulated it: the client got positions from both, and saw its
    // truck jump back and freeze.
    let ridden: HashSet<&str> = items
        .values()
        .filter(|i| i.object_type == "player")
        .filter(|i| i.seat.as_deref().is_some_and(|seat| !seat.is_empty()))
        .filter_map(|i| i.parent_id.as_deref())
        .collect();
    let rank = |item: &SnapshotItem| -> u8 {
        if item.object_type == "player" {
            0
        } else if ridden.contains(item.object_uuid.as_str()) {
            1
        } else {
            2
        }
    };
    let mut ordered: Vec<&SnapshotItem> = items
        .values()
        .filter(|item| !is_world_object(&item.object_type) && !is_member(item, zones))
        .collect();
    ordered.sort_by_key(|item| rank(item));

    let mut frozen = 0usize;
    for item in ordered {
        untrack(item, server);
        debug!("[freeze_object] item {} ({}) leaves this server", item.object_uuid, item.object_type);
        send_ws_text(&websocket, "freeze_object", wire_message("freeze_object", item, &item.object_data, false)?)?;
        frozen += 1;
    }
    info!(
        "[freeze_object] done: frozen={} players managed now: {:?}",
        frozen, server.managed_players.lock().unwrap()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_common::world::Point3;

    /// The same object in the form live messages use, `_world` inside, and in a snapshot's.
    fn both_forms(object_type: &str, uuid: &str, properties: Value) -> (GenericPropsRequest, SnapshotItem) {
        let world = ObjectWorld::space_at(Point3::new(1.0, 2.0, 3.0));
        let mut object_data = properties.clone();
        object_data[ObjectWorld::KEY] = serde_json::to_value(&world).unwrap();
        object_data[ObjectWorld::GLOBAL_POSITION_KEY] = json!({"x": 1.0, "y": 2.0, "z": 3.0});
        let text = |key: &str| properties.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let live = GenericPropsRequest {
            object_type: object_type.into(),
            object_uuid: uuid.into(),
            object_data,
            broadcast_only: None,
        };
        let item = SnapshotItem {
            object_type: object_type.into(),
            object_uuid: uuid.into(),
            world,
            parent_id: text("parent_id"),
            seat: text("seat"),
            poi_uuid: text("poi_uuid"),
            name: text("name"),
            object_data: to_raw_value(&properties).unwrap(),
        };
        (live, item)
    }

    /// What Godot was sent when a snapshot's items held a `Value`, minus `broadcast_only`
    /// (always null, read nowhere in Godot).
    fn message_before(event: &str, live: &GenericPropsRequest, spawn: bool, dormant: bool) -> Value {
        let mut data = item_on_wire(live);
        if spawn && live.object_type == "player" {
            super::super::move_clocks::mark(&mut data["object_data"], &live.object_uuid);
            super::super::player_roles::mark(&mut data["object_data"], &live.object_uuid);
        }
        if dormant {
            data["dormant"] = Value::Bool(true);
        }
        data.as_object_mut().unwrap().remove("broadcast_only");
        json!({"namespace": "server", "event": event, "data": data})
    }

    fn message_now(event: &str, item: &SnapshotItem, spawn: bool, dormant: bool) -> Value {
        let properties = if spawn { spawn_properties(item) } else { Cow::Borrowed(item.object_data.as_ref()) };
        serde_json::from_str(&wire_message(event, item, &properties, dormant).unwrap()).unwrap()
    }

    #[test]
    fn godot_gets_the_same_object_as_before() {
        let box_properties = json!({
            "position": {"x": 1.5, "y": -2.25, "z": 3.125},
            "parent_id": "hab",
            "opened": false,
            "parcel_number": 12,
            "nested": {"list": [1, 2.5, "three", null]},
        });
        let (live, item) = both_forms("box", "wire-box", box_properties);
        assert_eq!(message_now("initial_object", &item, true, false), message_before("initial_object", &live, true, false));
        assert_eq!(message_now("initial_object", &item, true, true), message_before("initial_object", &live, true, true));
        assert_eq!(message_now("activate_object", &item, true, false), message_before("activate_object", &live, true, false));
        assert_eq!(message_now("freeze_object", &item, false, false), message_before("freeze_object", &live, false, false));

        let (live, item) = both_forms("planet", "wire-planet", json!({"name": "SandBox", "positions": [{"x": 1e9, "y": 0.0, "z": 0.0}]}));
        assert_eq!(message_now("initial_object", &item, false, false), message_before("initial_object", &live, false, false));
    }

    #[test]
    fn a_handed_over_player_keeps_its_clocks_and_roles() {
        let uuid = "wire-player";
        super::super::move_clocks::set(uuid, true);
        super::super::player_roles::set(uuid, vec!["admin".to_string()]);
        // Whatever `roles` the data carries is dropped: only the verified token grants them.
        let properties = json!({"parent_id": "truck", "seat": "SeatDriver", "roles": ["forged"]});
        let (live, item) = both_forms("player", uuid, properties);

        let now = message_now("activate_object", &item, true, false);
        assert_eq!(now, message_before("activate_object", &live, true, false));
        assert_eq!(now["data"]["object_data"]["move_clocks"], true);
        assert_eq!(now["data"]["object_data"]["roles"], json!(["admin"]));
        assert_eq!(now["data"]["object_data"]["seat"], "SeatDriver");

        // Asleep too, and a freeze carries neither: they only matter to a spawn.
        assert_eq!(message_now("initial_object", &item, true, true), message_before("initial_object", &live, true, true));
        let freeze = message_now("freeze_object", &item, false, false);
        assert_eq!(freeze, message_before("freeze_object", &live, false, false));
        assert_eq!(freeze["data"]["object_data"]["roles"], json!(["forged"]));
        assert!(freeze["data"]["object_data"].get("move_clocks").is_none());

        super::super::move_clocks::forget(uuid);
        super::super::player_roles::forget(uuid);
    }

    /// Godot reads `data.get("dormant", false)`: a present `null` would bypass the default.
    #[test]
    fn dormant_is_left_out_unless_true() {
        let (_, item) = both_forms("box", "wire-awake", json!({"opened": false}));
        let awake = wire_message("initial_object", &item, &item.object_data, false).unwrap();
        assert!(!awake.contains("dormant"), "{}", awake);
        let asleep: Value = serde_json::from_str(&wire_message("initial_object", &item, &item.object_data, true).unwrap()).unwrap();
        assert_eq!(asleep["data"]["dormant"], true);
    }

    #[test]
    fn no_internal_key_reaches_godot() {
        let (_, item) = both_forms("box", "wire-internal", json!({"opened": false}));
        let message = wire_message("initial_object", &item, &item.object_data, false).unwrap();
        assert!(!message.contains(ObjectWorld::KEY) && !message.contains(ObjectWorld::GLOBAL_POSITION_KEY), "{}", message);
    }
}

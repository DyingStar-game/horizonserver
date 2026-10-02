//! Bulk object transfer between Horizon and one Godot server, used when a server
//! starts (initial world dump), when zones are split and when they are merged.
//!
//! Every item carries `object_data["_world"]` (space / planet + local position),
//! injected by ds_genericprops; membership is `zones_contain(zones, world)`.
//! Planets and stars are worlds, not zone content: always sent, never frozen.
//! Only the items inside the zones are sent: the Godot server keeps no copy of what
//! another server simulates (its PropRegistry forgets out-of-zone items).

use ds_common::events::GenericPropsRequest;
use ds_common::world::ObjectWorld;
use ds_common::zone::{is_world_object, zones_contain, zones_label, Zone};
use horizon_event_system::EventError;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use super::send_ws;
use crate::server::Server;

pub type ManagedObjects = Arc<Mutex<HashSet<String>>>;
pub type ManagedPlayers = Arc<Mutex<Vec<String>>>;

/// The item as Godot must see it: without the Horizon-internal keys.
pub fn item_on_wire(item: &GenericPropsRequest) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    ObjectWorld::strip_internal_keys(&mut value["object_data"]);
    value
}

fn is_member(item: &GenericPropsRequest, zones: &[Zone]) -> bool {
    match ObjectWorld::from_object_data(&item.object_data) {
        Some(world) => zones_contain(zones, &world),
        None => {
            warn!("[initial_object] item {} has no _world / _global_position, treated as outside", item.object_uuid);
            false
        }
    }
}

fn track(item: &GenericPropsRequest, server: &Server) {
    server.managed_objects.lock().unwrap().insert(item.object_uuid.clone());
    if item.object_type == "player" {
        // A hand-over decided by the ServerManager: this server owns the player now.
        crate::ownership::take(&item.object_uuid, &server.uuid, &server.managed_players);
        let parent = item.object_data.get("parent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        server.player_parents.lock().unwrap().insert(item.object_uuid.clone(), parent);
    }
}

fn untrack(item: &GenericPropsRequest, server: &Server) {
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
    items: &HashMap<String, GenericPropsRequest>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    let websocket = server.websocket_sender.clone();
    info!("[initial_object] sending {} items, zones=[{}]", items.len(), zones_label(zones));

    let mut sent = 0usize;
    let mut skipped = 0usize;
    for item in items.values() {
        let world_object = is_world_object(&item.object_type);
        if !world_object && !is_member(item, zones) {
            debug!("[initial_object] item {} ({}) is outside the zones, not sent", item.object_uuid, item.object_type);
            skipped += 1;
            continue;
        }
        send_ws(&websocket, "initial_object", &json!({
            "namespace": "server",
            "event": "initial_object",
            "data": item_on_wire(item),
        }))?;
        sent += 1;
        if !world_object {
            track(item, server);
        }
    }

    send_ws(&websocket, "initial_object", &json!({
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

/// Sends only the world objects (planets, stars) of `items`: what an idle server of
/// the pool preloads so that being handed zones later is not a cold start.
pub fn handle_world_objects(items: &HashMap<String, GenericPropsRequest>, server: &Server) -> Result<(), EventError> {
    let mut sent = 0usize;
    for item in items.values().filter(|i| is_world_object(&i.object_type)) {
        send_ws(&server.websocket_sender, "initial_object", &json!({
            "namespace": "server",
            "event": "initial_object",
            "data": item_on_wire(item),
        }))?;
        sent += 1;
    }
    info!("[initial_object] {} preloaded {} world object(s) (planets/stars)", server.server_name, sent);
    Ok(())
}

/// Freezes on the server every non-world item that is NOT inside `zones` (pass an
/// empty slice to freeze everything) and forgets it from the managed sets.
pub fn handle_freeze_object(
    items: &HashMap<String, GenericPropsRequest>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    let websocket = server.websocket_sender.clone();
    info!("[freeze_object] checking {} items against zones=[{}]", items.len(), zones_label(zones));

    let mut frozen = 0usize;
    for item in items.values() {
        if is_world_object(&item.object_type) || is_member(item, zones) {
            continue;
        }
        untrack(item, server);
        debug!("[freeze_object] item {} ({}) leaves this server", item.object_uuid, item.object_type);
        send_ws(&websocket, "freeze_object", &json!({
            "namespace": "server",
            "event": "freeze_object",
            "data": item_on_wire(item),
        }))?;
        frozen += 1;
    }
    info!(
        "[freeze_object] done: frozen={} players managed now: {:?}",
        frozen, server.managed_players.lock().unwrap()
    );
    Ok(())
}

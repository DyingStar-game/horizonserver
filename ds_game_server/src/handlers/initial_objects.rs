//! Bulk object transfer between Horizon and one Godot server, used when a server
//! starts (initial world dump), when zones are split and when they are merged.
//!
//! Every item carries `object_data["_world"]` (space / planet + local position),
//! injected by ds_genericprops; membership is `zones_contain(zones, world)`.
//! Planets and stars are worlds, not zone content: always sent, never frozen.

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
        let mut players = server.managed_players.lock().unwrap();
        if !players.contains(&item.object_uuid) {
            players.push(item.object_uuid.clone());
        }
        let parent = item.object_data.get("parent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        server.player_parents.lock().unwrap().insert(item.object_uuid.clone(), parent);
    }
}

fn untrack(item: &GenericPropsRequest, server: &Server) {
    server.managed_objects.lock().unwrap().remove(&item.object_uuid);
    if item.object_type == "player" {
        let mut players = server.managed_players.lock().unwrap();
        if let Some(pos) = players.iter().position(|x| x == &item.object_uuid) {
            players.remove(pos);
        }
        server.player_parents.lock().unwrap().remove(&item.object_uuid);
    }
}

/// Sends every item to the server as `initial_object`; items outside `zones` are
/// frozen right after (Godot needs them for collisions but must not simulate them),
/// items inside become managed. Ends with `initial_object_end`.
pub fn handle_initial_object(
    items: &HashMap<String, GenericPropsRequest>,
    server: &Server,
    zones: &[Zone],
) -> Result<(), EventError> {
    let websocket = server.websocket_sender.clone();
    info!("[initial_object] sending {} items, zones=[{}]", items.len(), zones_label(zones));

    let mut sent = 0usize;
    let mut frozen = 0usize;
    for item in items.values() {
        let wire = item_on_wire(item);
        send_ws(&websocket, "initial_object", &json!({
            "namespace": "server",
            "event": "initial_object",
            "data": wire,
        }))?;
        sent += 1;

        if is_world_object(&item.object_type) {
            continue;
        }
        if is_member(item, zones) {
            track(item, server);
        } else {
            debug!("[initial_object] item {} ({}) is outside the zones, freezing", item.object_uuid, item.object_type);
            send_ws(&websocket, "freeze_object", &json!({
                "namespace": "server",
                "event": "freeze_object",
                "data": wire,
            }))?;
            frozen += 1;
        }
    }

    send_ws(&websocket, "initial_object", &json!({
        "namespace": "server",
        "event": "initial_object_end",
        "data": {},
    }))?;
    info!(
        "[initial_object] done: sent={} frozen={} players managed now: {:?}",
        sent, frozen, server.managed_players.lock().unwrap()
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

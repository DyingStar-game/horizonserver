use ds_common::world::ObjectWorld;
use horizon_event_system::EventError;
use serde_json::json;
use tracing::{debug, info};

use super::{send_ws, WsWriter};

/// Sends `server/add_prop` for a player. Horizon-internal keys (`_world`,
/// `_global_position`) are stripped from the copy that goes on the wire.
pub async fn handle_spawn_player(event: serde_json::Value, websocket: WsWriter) -> Result<(), EventError> {
    let player_uuid = event
        .get("object_uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    info!("[spawn_player] preparing add_prop for player_uuid={} to Godot server", player_uuid);

    let mut data = event;
    ObjectWorld::strip_internal_keys(&mut data["object_data"]);
    let message = json!({
        "namespace": "server",
        "event": "add_prop",
        "data": data,
    });
    debug!("[spawn_player] constructed message: {:?}", message);
    send_ws(&websocket, "spawn_player", &message)?;
    info!("[spawn_player] add_prop sent to Godot server for player_uuid={}", player_uuid);
    Ok(())
}

pub async fn handle_player_quit(event: serde_json::Value, websocket: WsWriter) -> Result<(), EventError> {
    debug!("[player_quit] Player quit event received: {:?}", event);
    let message = json!({
        "namespace": "server",
        "event": "remove_player",
        "data": event,
    });
    debug!("[player_quit] constructed message: {:?}", message);
    send_ws(&websocket, "player_quit", &message)
}

use horizon_event_system::{ClientEventWrapper, EventError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tracing::{debug, info};

use super::{send_ws, WsWriter};

/// Last `update_velocity` payload received from each client, by player uuid.
///
/// The client only sends its velocity when it changes, so a player crossing a
/// zone border while moving would otherwise land on the destination Godot server
/// with no velocity at all and stop dead at the border. Shared by every `Server`
/// instance of the process: the destination server never saw the movement
/// (it was not managing the player when it arrived).
static LAST_VELOCITY: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Keeps the payload of a client `update_velocity`, whoever manages the player.
pub fn remember_velocity(player_uuid: &str, data: &Value) {
    LAST_VELOCITY.lock().unwrap().insert(player_uuid.to_string(), data.clone());
}

/// Drops the cached velocity of a player who left the game.
pub fn forget_velocity(player_uuid: &str) {
    LAST_VELOCITY.lock().unwrap().remove(player_uuid);
}

/// Re-sends the last known velocity of a player to the Godot server that just
/// spawned them after a transfer, so they keep moving across the border.
/// Nothing is sent when no velocity was ever received for that player.
pub fn replay_velocity(player_uuid: &str, websocket: &WsWriter) -> Result<(), EventError> {
    let data = LAST_VELOCITY.lock().unwrap().get(player_uuid).cloned();
    let Some(data) = data else {
        debug!("[player_movement] no cached velocity for player {}, nothing to replay", player_uuid);
        return Ok(());
    };
    info!("[player_movement] replaying last velocity of player {} after transfer", player_uuid);
    send_move(player_uuid, &data, websocket)
}

pub async fn handle_player_movement(
    event: ClientEventWrapper<serde_json::Value>,
    websocket: WsWriter,
) -> Result<(), EventError> {
    send_move(&event.player_id.to_string(), &event.data, &websocket)
}

fn send_move(player_uuid: &str, data: &Value, websocket: &WsWriter) -> Result<(), EventError> {
    let message = json!({
        "namespace": "player",
        "event": "move",
        "player_id": player_uuid,
        "data": data,
    });
    debug!("[player_movement] constructed message: {:?}", message);
    send_ws(websocket, "player_movement", &message)
}

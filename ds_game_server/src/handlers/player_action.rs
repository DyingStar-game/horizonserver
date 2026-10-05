use horizon_event_system::{ClientEventWrapper, EventError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tracing::{debug, info};

use super::{send_ws, WsWriter};

/// Actions the client sends only when they change, and which hold until the next
/// one: the throttle / steer / brake of a driver, sprint held, weightless thrust and
/// brake, the walk speed.
const HELD_ACTIONS: [&str; 5] = ["vehicle_input", "sprint", "thrust_vertical", "eva_stabilize", "walk_speed"];

/// Last payload of each held action of each player, by player uuid then action.
///
/// A player handed over to another Godot server (split, merge, a vehicle crossing a
/// border) was spawned there with none of them: the new server applied the engine
/// brake and the truck stopped dead under its driver (preprod, 2026-10-04), and a
/// sprinting player went on walking. Shared by every `Server`: the new server never
/// saw the input.
static LAST_HELD: LazyLock<Mutex<HashMap<String, HashMap<String, Value>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Keeps the last held input of a player, whoever manages them.
pub fn remember_input(player_uuid: &str, data: &Value) {
    let Some(action) = data.get("action").and_then(|a| a.as_str()) else { return };
    if HELD_ACTIONS.contains(&action) {
        LAST_HELD.lock().unwrap().entry(player_uuid.to_string()).or_default().insert(action.to_string(), data.clone());
    }
}

/// Drops the cached input of a player who left the game.
pub fn forget_input(player_uuid: &str) {
    LAST_HELD.lock().unwrap().remove(player_uuid);
}

/// Re-sends the held input of a player to the Godot server that just took them
/// over. Harmless when it no longer applies: the server only applies a
/// `vehicle_input` to the vehicle the player pilots.
pub fn replay_input(player_uuid: &str, websocket: &WsWriter) -> Result<(), EventError> {
    let held: Vec<Value> = match LAST_HELD.lock().unwrap().get(player_uuid) {
        Some(actions) => actions.values().cloned().collect(),
        None => return Ok(()),
    };
    info!("[player_action] replaying {} held input(s) of player {} after a hand-over", held.len(), player_uuid);
    for data in held {
        send_ws(websocket, "player_action", &json!({
            "namespace": "player",
            "event": "action",
            "player_id": player_uuid,
            "data": data,
        }))?;
    }
    Ok(())
}

/// Forwards a client's action to the Godot server managing the player. Synchronous on purpose:
/// called from the handler itself, actions keep the order the client sent them in.
pub fn handle_player_action(
    event: &ClientEventWrapper<serde_json::Value>,
    websocket: &WsWriter,
) -> Result<(), EventError> {
    let message = json!({
        "namespace": "player",
        "event": "action",
        "player_id": event.player_id.to_string(),
        "data": event.data.clone(),
    });
    debug!("[player_action] constructed message: {:?}", message);
    send_ws(websocket, "player_action", &message)
}

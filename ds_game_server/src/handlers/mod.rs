pub mod spawn_player;
pub mod spawn_prop;
pub mod player_movement;
pub mod player_action;
pub mod initial_objects;
pub mod update_prop;

use horizon_event_system::EventError;
use serde_json::Value;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use tracing::{debug, error};
use websocket::message::OwnedMessage;
use websocket::sender::Writer;

/// The websocket writer to one Godot server, shared by every handler of that server.
pub type WsWriter = Arc<Mutex<Option<Writer<TcpStream>>>>;

/// Sends one JSON message to the Godot server. Fails when the socket is gone.
pub fn send_ws(websocket: &WsWriter, tag: &str, message: &Value) -> Result<(), EventError> {
    let mut ws_guard = websocket.lock().map_err(|e| {
        error!("[{}] websocket lock error: {}", tag, e);
        EventError::HandlerExecution(format!("websocket lock error: {}", e))
    })?;
    let Some(w) = ws_guard.as_mut() else {
        debug!("[{}] No websocket writer available", tag);
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    };
    debug!("[{}] Sending message to websocket", tag);
    w.send_message(&OwnedMessage::Text(message.to_string())).map_err(|e| {
        error!("[{}] ERROR sending message to game server: {}", tag, e);
        EventError::HandlerExecution(format!("Message blocked: {}", e))
    })?;
    debug!("[{}] Message sent successfully", tag);
    Ok(())
}

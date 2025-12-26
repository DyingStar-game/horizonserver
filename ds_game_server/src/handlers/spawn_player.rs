use horizon_event_system::{
    EventError,
};
use tracing::{debug};
use std::sync::{Arc, Mutex};
use std::net::TcpStream;
use websocket::sender::Writer;
use websocket::message::OwnedMessage;
use serde_json::json;

pub async fn handle_spawn_player(
    event: serde_json::Value,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
) -> Result<(), EventError> {
    
    let message = json!({
        "namespace": "server",
        "event": "add_prop",
        "data": event,
    });
    debug!("[spawn_player] constructed message: {:?}", message);
    let mut ws_guard = match websocket.lock() {
        Ok(g) => g,
        Err(e) => {
            debug!("[spawn_player] websocket lock error: {}", e);
            return Err(EventError::HandlerExecution(format!("websocket lock error: {}", e)));
        }
    };
    if ws_guard.is_none() {
        debug!("[spawn_player] No websocket writer available");
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    }
    if let Some(w) = ws_guard.as_mut() {
        debug!("[spawn_player] Sending message to websocket");
        if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
            debug!("[spawn_player] ERROR sending message to game server: {}", e);
            return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
        } else {
            debug!("[spawn_player] Message sent successfully");
        }
    }
    Ok(())
}

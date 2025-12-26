use horizon_event_system::{
    EventError, ClientEventWrapper,
};
use tracing::{debug, error};
use std::sync::{Arc, Mutex};
use std::net::TcpStream;
use websocket::sender::Writer;
use websocket::message::OwnedMessage;
use serde_json::json;

pub async fn handle_player_action(
    event: ClientEventWrapper<serde_json::Value>,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
) -> Result<(), EventError> {
    
    let message = json!({
        "namespace": "player",
        "event": "action",
        "player_id": event.player_id.to_string(),
        "data": event.data.clone(),
    });
    debug!("[message][to][gamesever]: {:?}", message);
    let mut ws_guard = match websocket.lock() {
        Ok(g) => g,
        Err(e) => {
            debug!("[player_action] websocket lock error: {}", e);
            return Err(EventError::HandlerExecution(format!("websocket lock error: {}", e)));
        }
    };
    if ws_guard.is_none() {
        debug!("[player_action] No websocket writer available");
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    }
    if let Some(w) = ws_guard.as_mut() {
        debug!("[player_action] Sending message to websocket");
        if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
            debug!("[player_action] ERROR sending message to game server: {}", e);
            return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
        } else {
            debug!("[player_action] Message sent successfully");
        }
    }
    Ok(())

}

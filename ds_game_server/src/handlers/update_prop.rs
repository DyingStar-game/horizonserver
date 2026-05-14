use horizon_event_system::{
    EventError,
};
use tracing::{debug};
use std::sync::{Arc, Mutex};
use std::net::TcpStream;
use websocket::sender::Writer;
use websocket::message::OwnedMessage;
use serde_json::json;

pub async fn handle_update_prop(
    event: serde_json::Value,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
) -> Result<(), EventError> {
    
    debug!("[update_prop] handler called with event: {:?}", event);
    debug!("[update_prop] websocket Arc ptr: {:p}", &websocket);
    let message = json!({
        "namespace": "server",
        "event": "update_prop",
        "data": event,
    });
    debug!("[update_prop] constructed message: {:?}", message);
    let mut ws_guard = match websocket.lock() {
        Ok(g) => g,
        Err(e) => {
            debug!("[update_prop] websocket lock error: {}", e);
            return Err(EventError::HandlerExecution(format!("websocket lock error: {}", e)));
        }
    };
    if ws_guard.is_none() {
        debug!("[update_prop] No websocket writer available");
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    }
    if let Some(w) = ws_guard.as_mut() {
        debug!("[update_prop] Sending message to websocket");
        if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
            debug!("[update_prop] ERROR sending message to game server: {}", e);
            return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
        } else {
            debug!("[update_prop] Message sent successfully");
        }
    }
    Ok(())
}

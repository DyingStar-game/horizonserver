use horizon_event_system::EventError;
use serde_json::json;
use tracing::debug;

use super::{send_ws, WsWriter};

pub async fn handle_spawn_prop(event: serde_json::Value, websocket: WsWriter) -> Result<(), EventError> {
    debug!("[spawn_prop] handler called with event: {:?}", event);
    let message = json!({
        "namespace": "server",
        "event": "add_prop",
        "data": event,
    });
    send_ws(&websocket, "spawn_prop", &message)
}

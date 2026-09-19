use horizon_event_system::EventError;
use serde_json::json;
use tracing::debug;

use super::{send_ws, WsWriter};

pub async fn handle_update_prop(event: serde_json::Value, websocket: WsWriter) -> Result<(), EventError> {
    debug!("[update_prop] handler called with event: {:?}", event);
    let message = json!({
        "namespace": "server",
        "event": "update_prop",
        "data": event,
    });
    send_ws(&websocket, "update_prop", &message)
}

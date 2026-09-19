use horizon_event_system::{ClientEventWrapper, EventError};
use serde_json::json;
use tracing::debug;

use super::{send_ws, WsWriter};

pub async fn handle_player_action(
    event: ClientEventWrapper<serde_json::Value>,
    websocket: WsWriter,
) -> Result<(), EventError> {
    let message = json!({
        "namespace": "player",
        "event": "action",
        "player_id": event.player_id.to_string(),
        "data": event.data.clone(),
    });
    debug!("[player_action] constructed message: {:?}", message);
    send_ws(&websocket, "player_action", &message)
}

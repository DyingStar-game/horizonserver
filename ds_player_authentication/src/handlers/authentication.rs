//! # Player authentication handlers
//! 
//! Manages the authentication part when client connect to Horizon server.
//! 

use std::sync::Arc;
use horizon_event_system::{
    ClientConnectionRef, EventSystem,PlayerId, Vec3
};
use tracing::{debug, info, error};
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInit {
    pub data: PlayerInitData,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInitData {
    pub login: String,
    pub password: String,
    pub spawn_point: i8,
}

pub async fn handle_player_init(
    event: PlayerInit,
    player_id: PlayerId,
    connection: ClientConnectionRef,
    events: Arc<EventSystem>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {

    if event.data.login == "I am an idiot !" {
        return Err("plugin auth: Rejected player init with invalid login".into());
    }

    // send to client its uuid
    debug!("plugin auth: Emitting init_registered for player_id {:?}", player_id);

    // TODO replace by uuid found in user database
    let player_db_id = PlayerId::new();

    let payload = serde_json::to_vec(&serde_json::json!({
        "player_id": player_db_id,
        "type": "init_ack"
    })).expect("failed to serialize payload");

    if let Err(e) = connection.respond(&payload).await
    {
        error!("Failed to send init_ack to client: {}", e);
    }

    // Update the player_id stored in the connection manager
    // This replaces the temporary connection-level player_id with the database player_id
    if let Err(e) = events.emit_core("update_player_id", &serde_json::json!({
        "old_player_id": player_id,
        "new_player_id": player_db_id,
        "connection_id": player_id,  // The connection_id is currently the old player_id
    })).await
    {
        error!("Failed to emit update_player_id event: {}", e);
    }

    // send to gorcplugin (player plugin) the new player event
    if let Err(e) = events.emit_plugin("propsplugin", "new_player", &serde_json::json!({
        "object_type": "player",
        "object_uuid": player_db_id,
        "object_data": {
            "name": event.data.login,
            "position": Vec3::new(0.0, 0.0, 0.0),
            "rotation": Vec3::new(0.0, 0.0, 0.0),
            "connection_id": player_db_id,  // Use the new player_db_id here
            "spawn_point": event.data.spawn_point,
        }
    })).await
    {
        error!("Failed to emit plugin event to gorcplugin: {}", e);
    }

    Ok(())
}

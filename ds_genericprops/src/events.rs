use serde_json;
use serde::{Deserialize, Serialize};
use horizon_event_system::{PlayerId, Vec3};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericPropsGORCUpdateRequest {
    /// ID of the player requesting the movement
    pub object_uuid: String,
    /// Requested new position in world coordinates  
    pub new_data: serde_json::Value,
    /// Current velocity vector for prediction
    pub channel: u8
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerMoveRequest {
    /// ID of the player requesting the movement
    pub player_id: PlayerId,
    /// Requested new position in world coordinates  
    pub position: Vec3,
    pub rotation: Vec3,
    pub out_of_zone: Option<String>,
    pub parent_id: Option<String>,
    /// Simulation time (ms) of the game server when it produced this position,
    /// forwarded to the clients so they measure speeds against it.
    #[serde(default)]
    pub servergodot_ms: Option<f64>,
    /// When ds_game_server read this update from the game server (unix ms).
    #[serde(default)]
    pub horizon_received_ms: Option<u64>,
}

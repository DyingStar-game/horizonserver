use serde_json;
use serde::{Deserialize, Serialize};

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
pub struct GenericPropsRequest {
    pub object_type: String,
    pub object_uuid: String,
    pub object_data: serde_json::Value
}
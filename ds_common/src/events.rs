use serde_json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericPropsRequest {
    pub object_type: String,
    pub object_uuid: String,
    pub object_data: serde_json::Value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DSErrorMessage {
    pub player_id: String,
    pub type_: String,
    pub code: i32,
    pub message: String
}

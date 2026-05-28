use serde_json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericPropsRequest {
    pub object_type: String,
    pub object_uuid: String,
    pub object_data: serde_json::Value,
    /// When true the handler broadcasts the current GORC state to nearby clients
    /// but does NOT call update_object, because the caller has already committed
    /// the authoritative state via a direct update_object call.
    #[serde(default)]
    pub broadcast_only: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DSErrorMessage {
    pub player_id: String,
    pub type_: String,
    pub code: i32,
    pub message: String
}

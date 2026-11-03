use serde_json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// `gameserver:objects_snapshot`: genericprops' answer to the ServerManager's
/// `genericprops:get_objects_snapshot`, the objects by uuid.
///
/// Typed on both ends, never a `serde_json::Value`: the 52k objects of a first split
/// went through `json!` (a full copy), were parsed into a `Value` on arrival, copied
/// again and converted — 2.8 s of the 2.95 s the return took, where reading 50 MB
/// of JSON straight into this type takes 0.5 s (preprod 2026-11-03: past the 5 s
/// snapshot timeout once). Same JSON on the wire as before.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectsSnapshot {
    pub request_id: String,
    pub items: HashMap<String, GenericPropsRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DSErrorMessage {
    pub player_id: String,
    pub type_: String,
    pub code: i32,
    pub message: String
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_snapshot_is_the_json_it_replaced() {
        let item = GenericPropsRequest {
            object_type: "box".into(),
            object_uuid: "b".into(),
            object_data: json!({"parent_id": "h", "_world": {"world": "planet"}}),
            broadcast_only: None,
        };
        let snapshot = ObjectsSnapshot { request_id: "r".into(), items: HashMap::from([("b".to_string(), item.clone())]) };
        let before = json!({"request_id": "r", "items": {"b": item}});
        assert_eq!(serde_json::to_value(&snapshot).unwrap(), before);
        let back: ObjectsSnapshot = serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
        assert_eq!(back.request_id, "r");
        assert_eq!(back.items["b"].object_data, item.object_data);
    }
}

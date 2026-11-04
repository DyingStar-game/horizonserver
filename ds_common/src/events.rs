use serde_json;
use serde_json::value::RawValue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::world::ObjectWorld;

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

/// One object of a snapshot: what Horizon decides on is typed, the rest stays text.
///
/// A mesh op only reads the fields below. It passes the properties on to Godot, so
/// they are written to JSON once, by genericprops, and copied as they are from then
/// on: as a `Value` holding `_world` too, an object was rebuilt on arrival, copied at
/// each step of a hand-over, and converted again for each zone test and each send.
///
/// No `#[serde(flatten)]` nor tagged enum on the way to `object_data`: serde buffers
/// those, which a `RawValue` does not survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotItem {
    pub object_type: String,
    pub object_uuid: String,
    /// The world the object lies in, for the zone tests. Without its `chain` and
    /// `planet_name`, which no reader of a snapshot uses.
    pub world: ObjectWorld,
    pub parent_id: Option<String>,
    pub seat: Option<String>,
    pub poi_uuid: Option<String>,
    pub name: Option<String>,
    /// The properties, without the Horizon-internal keys: ready for Godot.
    pub object_data: Box<RawValue>,
}

/// `gameserver:objects_snapshot`: genericprops' answer to the ServerManager's
/// `genericprops:get_objects_snapshot`, the objects by uuid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectsSnapshot {
    pub request_id: String,
    pub items: HashMap<String, SnapshotItem>,
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
    use crate::world::Point3;
    use serde_json::value::to_raw_value;
    use serde_json::json;

    #[test]
    fn a_snapshot_keeps_the_properties_as_they_were_written() {
        let text = r#"{"a":1.5,"parent_id":"h","z":{"nested":[1,2]}}"#;
        let item = SnapshotItem {
            object_type: "box".into(),
            object_uuid: "b".into(),
            world: ObjectWorld::space_at(Point3::new(1.0, 2.0, 3.0)),
            parent_id: Some("h".into()),
            seat: None,
            poi_uuid: None,
            name: None,
            object_data: RawValue::from_string(text.to_string()).unwrap(),
        };
        let snapshot = ObjectsSnapshot { request_id: "r".into(), items: HashMap::from([("b".to_string(), item)]) };
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let back: ObjectsSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.request_id, "r");
        let back = &back.items["b"];
        assert_eq!(back.object_data.get(), text, "copied, not rewritten");
        assert_eq!(back.world.local_position, Point3::new(1.0, 2.0, 3.0));
        assert_eq!(back.parent_id.as_deref(), Some("h"));
        assert_eq!(back.seat, None);
        // Not an inert string: it nests as JSON in what goes on to Godot.
        let wire = serde_json::to_value(to_raw_value(&back.object_data).unwrap()).unwrap();
        assert_eq!(wire, json!({"a": 1.5, "parent_id": "h", "z": {"nested": [1, 2]}}));
    }
}

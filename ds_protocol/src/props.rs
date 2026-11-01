//! GORC payloads of props (players included) for bitcode clients.
//!
//! A JSON client gets the channel's data as one JSON object, as before. A bitcode
//! client gets position and rotation apart from the other keys, so they can travel
//! unreliably (datagrams on WebTransport) while the other keys — `parent_id`,
//! `action`, `seat`… — stay reliable:
//!
//! - `<event>/transform` frames ([`transform_event`]) carry a [`TransformUpdate`],
//!   always unreliable;
//! - the event itself (`move`, `update_property`…) carries a [`PropUpdate`] with the
//!   other keys, reliable, and only when there are any;
//! - a zone entry carries a [`PropState`]: both parts, reliable.
//!
//! The client keeps, per object and channel, the transform with the highest GORC
//! `seq` and drops older ones that arrive late.
//!
//! **Epoch.** A transform is in the frame of the object's parent (`parent_id`, a
//! reliable key). When the parent changes, the first transforms in the new frame can
//! arrive before the reliable frame that announces it, and applied under the old
//! parent they would put the object somewhere else entirely. Every frame carries the
//! object's `epoch`, bumped at each change of parent: the client applies a transform
//! only when its epoch is the one of the last parent it applied (from an update or a
//! zone entry), and drops it otherwise — the next ones follow at the channel's rate.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::DynValue;

/// Suffix of the event name of the unreliable transform frames: `move/transform`
/// carries the transform of a `move`.
pub const TRANSFORM_SUFFIX: &str = "/transform";

/// Name of the transform frames of `event`.
pub fn transform_event(event: &str) -> String {
    format!("{event}{TRANSFORM_SUFFIX}")
}

/// The event a transform frame belongs to, or `None` when `name` is not one.
pub fn transform_of(name: &str) -> Option<&str> {
    name.strip_suffix(TRANSFORM_SUFFIX)
}

/// Keys carried by [`TransformUpdate`] instead of the other keys.
pub const POSITION_KEY: &str = "position";
pub const ROTATION_KEY: &str = "rotation";

/// Position (world or parent-local coordinates, `f64`: planet coordinates reach
/// millions of metres, where an `f32` loses half a metre) and Euler rotation in
/// radians.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct TransformUpdate {
    /// The object's epoch (see the module docs).
    pub epoch: u32,
    pub position: Option<[f64; 3]>,
    pub rotation: Option<[f32; 3]>,
    /// Keys that describe this transform rather than the object's state (the clocks
    /// of the trip: `servergodot_ms`, `horizon_received_ms`, `horizon_sent_ms`), as a
    /// JSON object, or null.
    pub extra: DynValue,
}

impl TransformUpdate {
    /// No position and no rotation (`extra` alone is not worth a frame).
    pub fn is_empty(&self) -> bool {
        self.position.is_none() && self.rotation.is_none()
    }
}

/// The keys of a channel other than position and rotation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct PropUpdate {
    /// The object's epoch (see the module docs).
    pub epoch: u32,
    /// A JSON object.
    pub data: DynValue,
}

/// Full state of a channel, sent on zone entry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct PropState {
    /// Carries the object's epoch.
    pub transform: TransformUpdate,
    /// A JSON object with the other keys.
    pub data: DynValue,
}

/// Splits a channel payload (a JSON object) into its transform and its other keys.
///
/// `position` and `rotation` go to the transform when they are `{x, y, z}` objects of
/// numbers. Anything else (absent, null, another shape) stays with the other keys, so
/// nothing is lost.
pub fn split(data: &Map<String, Value>) -> (TransformUpdate, Map<String, Value>) {
    let mut rest = data.clone();
    let mut transform = TransformUpdate::default();
    if let Some(position) = data.get(POSITION_KEY).and_then(xyz) {
        transform.position = Some(position);
        rest.remove(POSITION_KEY);
    }
    if let Some(rotation) = data.get(ROTATION_KEY).and_then(xyz) {
        transform.rotation = Some(rotation.map(|v| v as f32));
        rest.remove(ROTATION_KEY);
    }
    (transform, rest)
}

/// Puts a transform and the other keys back into one JSON object, as a JSON client
/// would have received it.
pub fn join(transform: &TransformUpdate, rest: &Map<String, Value>) -> Map<String, Value> {
    let mut data = rest.clone();
    if let Ok(Value::Object(extra)) = transform.extra.to_value() {
        data.extend(extra);
    }
    if let Some([x, y, z]) = transform.position {
        data.insert(POSITION_KEY.to_string(), serde_json::json!({"x": x, "y": y, "z": z}));
    }
    if let Some([x, y, z]) = transform.rotation {
        data.insert(ROTATION_KEY.to_string(), serde_json::json!({"x": x, "y": y, "z": z}));
    }
    data
}

fn xyz(value: &Value) -> Option<[f64; 3]> {
    let object = value.as_object()?;
    if object.len() != 3 {
        return None;
    }
    Some([object.get("x")?.as_f64()?, object.get("y")?.as_f64()?, object.get("z")?.as_f64()?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn move_payload() -> Map<String, Value> {
        json!({
            "player_id": "0b6f0a43-58a4-4a48-9a8f-0d6e1f8f2c3a",
            "position": {"x": 4450602.9, "y": -12.25, "z": 3.0},
            "rotation": {"x": 0.0, "y": 1.5, "z": -0.25},
            "parent_id": "65345350-5a40-4f44-a3c1-0ca5641cb97a"
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn position_and_rotation_go_to_the_transform() {
        let (transform, rest) = split(&move_payload());
        assert_eq!(transform.position, Some([4450602.9, -12.25, 3.0]));
        assert_eq!(transform.rotation, Some([0.0, 1.5, -0.25]));
        assert!(!rest.contains_key(POSITION_KEY) && !rest.contains_key(ROTATION_KEY));
        assert_eq!(rest.len(), 2);
    }

    /// Exact for rotations an f32 holds exactly; others come back within f32 precision.
    #[test]
    fn join_gives_back_the_payload() {
        let payload = move_payload();
        let (transform, rest) = split(&payload);
        assert_eq!(join(&transform, &rest), payload);
    }

    #[test]
    fn other_shapes_stay_with_the_other_keys() {
        let payload = json!({"position": [1, 2, 3], "rotation": {"x": 1, "y": "a", "z": 0}})
            .as_object()
            .unwrap()
            .clone();
        let (transform, rest) = split(&payload);
        assert!(transform.is_empty());
        assert_eq!(rest, payload);
    }

    #[test]
    fn transform_event_names_round_trip() {
        assert_eq!(transform_event("move"), "move/transform");
        assert_eq!(transform_of("move/transform"), Some("move"));
        assert_eq!(transform_of("move"), None);
    }

    #[test]
    fn a_transform_frame_is_small() {
        let (transform, _) = split(&move_payload());
        // The epoch, two option tags, 3 f64, 3 f32 and a null extra.
        assert!(bitcode::encode(&transform).len() <= 48);
    }

    #[test]
    fn prop_state_round_trips_through_bitcode() {
        let (transform, rest) = split(&move_payload());
        let state = PropState { transform, data: DynValue::from(&Value::Object(rest.clone())) };
        let decoded: PropState = bitcode::decode(&bitcode::encode(&state)).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(join(&decoded.transform, decoded.data.to_value().unwrap().as_object().unwrap()), move_payload());
    }
}

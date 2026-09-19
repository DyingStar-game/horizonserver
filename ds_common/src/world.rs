//! The **world** an object lives in: `space` or one planet, derived from its
//! `parent_id` chain (player > vehicle > planet > space). Computed by
//! ds_genericprops (the owner of `parent_id`) and injected as `object_data["_world"]`
//! in every payload ds_game_server consumes, so the game-server plugin can decide
//! which Godot server owns an object without any GORC lookup of its own.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Plain point, kept independent from `horizon_event_system::Vec3` so ds_common
/// stays free of the event-system dependency. Serialises as `{x, y, z}` like Vec3.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct Point3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Point3 {
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub fn from_value(value: &Value) -> Option<Point3> {
        Some(Point3 {
            x: value.get("x")?.as_f64()?,
            y: value.get("y")?.as_f64()?,
            z: value.get("z")?.as_f64()?,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorldKind {
    Space,
    Planet,
}

/// One ancestor in the parent chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorldChainEntry {
    pub uuid: String,
    pub object_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectWorld {
    /// Parents from the direct parent upward (self excluded). Stops at the first
    /// `planet`, which is the world the object lives in.
    pub chain: Vec<WorldChainEntry>,
    pub world: WorldKind,
    pub planet_uuid: Option<String>,
    pub planet_name: Option<String>,
    /// Position relative to the world origin: the planet centre for a planet world
    /// (== `global_position` inside that planet's own Godot World3D), the universe
    /// origin for space.
    pub local_position: Point3,
}

impl ObjectWorld {
    /// Key under which the world is injected in `object_data`.
    pub const KEY: &'static str = "_world";
    /// Key of the absolute position ds_genericprops also injects; both are internal
    /// to Horizon and stripped before anything goes to a Godot server.
    pub const GLOBAL_POSITION_KEY: &'static str = "_global_position";

    pub fn space_at(local_position: Point3) -> Self {
        Self {
            chain: Vec::new(),
            world: WorldKind::Space,
            planet_uuid: None,
            planet_name: None,
            local_position,
        }
    }

    pub fn is_planet(&self) -> bool {
        self.world == WorldKind::Planet
    }

    /// Reads `object_data["_world"]`. When absent, falls back to `_global_position`
    /// read as a space world (the old contract) so a payload from a not-yet-migrated
    /// emitter still resolves to something; `None` only when neither key is usable.
    pub fn from_object_data(object_data: &Value) -> Option<ObjectWorld> {
        if let Some(world) = object_data.get(Self::KEY) {
            match serde_json::from_value::<ObjectWorld>(world.clone()) {
                Ok(world) => return Some(world),
                Err(e) => {
                    tracing::warn!("[world] invalid _world payload ({}), falling back to _global_position", e);
                }
            }
        }
        let global = object_data.get(Self::GLOBAL_POSITION_KEY)?;
        let point = Point3::from_value(global)?;
        tracing::warn!("[world] no _world in object_data, assuming space at {:?}", point);
        Some(ObjectWorld::space_at(point))
    }

    /// Removes the Horizon-internal keys before the object goes on the Godot wire.
    pub fn strip_internal_keys(object_data: &mut Value) {
        if let Some(map) = object_data.as_object_mut() {
            map.remove(Self::KEY);
            map.remove(Self::GLOBAL_POSITION_KEY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn world_round_trips_through_json() {
        let world = ObjectWorld {
            chain: vec![
                WorldChainEntry { uuid: "v".into(), object_type: "vehicle".into() },
                WorldChainEntry { uuid: "p".into(), object_type: "planet".into() },
            ],
            world: WorldKind::Planet,
            planet_uuid: Some("p".into()),
            planet_name: Some("tarsis_3".into()),
            local_position: Point3::new(1.0, 2.0, 3.0),
        };
        let value = serde_json::to_value(&world).unwrap();
        assert_eq!(value["world"], "planet");
        assert_eq!(value["local_position"]["y"], 2.0);
        let back: ObjectWorld = serde_json::from_value(value).unwrap();
        assert_eq!(back, world);
    }

    #[test]
    fn falls_back_to_global_position_as_space() {
        let data = json!({"_global_position": {"x": 5.0, "y": 6.0, "z": 7.0}});
        let world = ObjectWorld::from_object_data(&data).unwrap();
        assert_eq!(world.world, WorldKind::Space);
        assert_eq!(world.local_position, Point3::new(5.0, 6.0, 7.0));
        assert!(ObjectWorld::from_object_data(&json!({"name": "x"})).is_none());
    }

    #[test]
    fn strip_removes_internal_keys_only() {
        let mut data = json!({"name": "a", "_world": {}, "_global_position": {}});
        ObjectWorld::strip_internal_keys(&mut data);
        assert_eq!(data, json!({"name": "a"}));
    }
}

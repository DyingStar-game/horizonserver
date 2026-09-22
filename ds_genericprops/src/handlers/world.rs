//! Resolves the **world** an object lives in (space or one planet) by walking its
//! `parent_id` chain: player > vehicle > planet > space. genericprops is the only
//! crate that can read `parent_id` (it lives inside `GenericProps.data`), so the
//! result is serialised into `object_data["_world"]` for ds_game_server.

use dashmap::DashMap;
use ds_common::events::GenericPropsRequest;
use ds_common::world::{ObjectWorld, Point3, WorldChainEntry, WorldKind};
use horizon_event_system::{GorcInstanceManager, GorcObjectId, Vec3};
use serde_json::{Map, Value};
use std::sync::Arc;
use tracing::warn;

use crate::genericprops::GenericProps;

/// Longest parent chain we follow before giving up (guards against cycles too).
const MAX_CHAIN_DEPTH: usize = 8;

/// What the resolver needs from one ancestor.
#[derive(Debug, Clone)]
pub struct ChainNode {
    pub uuid: String,
    pub object_type: String,
    pub name: Option<String>,
    pub parent_id: Option<String>,
    /// Position local to ITS parent (for a planet: `positions[0]`).
    pub local_position: Vec3,
}

/// All channels of an object flattened into one property map.
pub fn flatten_props(gp: &GenericProps) -> Map<String, Value> {
    let mut out = Map::new();
    for properties in gp.data.values() {
        if let Value::Object(map) = properties {
            for (key, value) in map.iter() {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    out
}

/// First non-empty `parent_id` across channels (the idiom used everywhere in genericprops).
pub fn read_parent_id(gp: &GenericProps) -> Option<String> {
    gp.parent_id()
}

/// Own local position + parent, as stored on the instance.
pub fn read_own_local_and_parent(gp: &GenericProps) -> (Vec3, Option<String>) {
    let flat = Value::Object(flatten_props(gp));
    (gp.object_def.get_position(&flat), read_parent_id(gp))
}

pub fn chain_node_of(gp: &GenericProps) -> ChainNode {
    let flat = flatten_props(gp);
    let name = flat.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let local_position = gp.object_def.get_position(&Value::Object(flat));
    ChainNode {
        uuid: gp.uuid.clone(),
        object_type: gp.object_def.name.clone(),
        name,
        parent_id: read_parent_id(gp),
        local_position,
    }
}

/// Pure fold: walks `chain` (direct parent first) accumulating local positions
/// until the first planet, which becomes the world. Anything past the planet is
/// ignored (a moon's own parent planet is not the moon-dweller's world).
pub fn fold_world(own_local: Vec3, chain: &[ChainNode]) -> ObjectWorld {
    let mut local = own_local;
    let mut entries = Vec::with_capacity(chain.len());
    for node in chain {
        entries.push(WorldChainEntry { uuid: node.uuid.clone(), object_type: node.object_type.clone() });
        if node.object_type == "planet" {
            return ObjectWorld {
                chain: entries,
                world: WorldKind::Planet,
                planet_uuid: Some(node.uuid.clone()),
                planet_name: node.name.clone(),
                local_position: Point3::new(local.x, local.y, local.z),
            };
        }
        local = Vec3::new(
            local.x + node.local_position.x,
            local.y + node.local_position.y,
            local.z + node.local_position.z,
        );
    }
    ObjectWorld {
        chain: entries,
        world: WorldKind::Space,
        planet_uuid: None,
        planet_name: None,
        local_position: Point3::new(local.x, local.y, local.z),
    }
}

/// Loads the ancestors of an object from GORC and folds them into its world.
/// `own_local` is the object's position relative to `parent_id`.
pub async fn resolve_world(
    gorc: &Arc<GorcInstanceManager>,
    own_local: Vec3,
    parent_id: Option<String>,
) -> ObjectWorld {
    let mut chain: Vec<ChainNode> = Vec::new();
    let mut next = parent_id;
    while let Some(pid) = next.take() {
        if chain.len() >= MAX_CHAIN_DEPTH || chain.iter().any(|n| n.uuid == pid) {
            warn!("[world] parent chain too deep or cyclic at {}, stopping", pid);
            break;
        }
        let Ok(gorc_id) = GorcObjectId::from_str(&pid) else {
            warn!("[world] invalid parent uuid {}, stopping chain", pid);
            break;
        };
        let node = gorc
            .with_object_mut(gorc_id, |instance| instance.get_object::<GenericProps>().map(chain_node_of))
            .await
            .flatten();
        match node {
            Some(node) => {
                next = node.parent_id.clone();
                let is_planet = node.object_type == "planet";
                chain.push(node);
                if is_planet {
                    break;
                }
            }
            None => {
                warn!("[world] parent {} not found in GORC, chain truncated", pid);
                break;
            }
        }
    }
    fold_world(own_local, &chain)
}

/// The full item of an object as ds_game_server expects it in a snapshot: every
/// property flattened, plus `_global_position` and `_world`.
pub async fn full_item(gorc: &Arc<GorcInstanceManager>, uuid: &str) -> Option<GenericPropsRequest> {
    let gorc_id = GorcObjectId::from_str(uuid).ok()?;
    let (object_type, props, global_position, (own_local, parent_id)) = gorc
        .with_object_mut(gorc_id, |instance| {
            instance.get_object::<GenericProps>().map(|gp| (
                gp.object_def.name.clone(),
                flatten_props(gp),
                gp.global_position,
                read_own_local_and_parent(gp),
            ))
        })
        .await
        .flatten()?;
    let object_world = resolve_world(gorc, own_local, parent_id).await;
    let mut object_data = Value::Object(props);
    inject_global_position(&mut object_data, global_position);
    inject_world(&mut object_data, &object_world);
    Some(GenericPropsRequest { object_type, object_uuid: uuid.to_string(), object_data, broadcast_only: None })
}

/// Every object whose parent chain reaches `root_uuid` (a vehicle's cargo, its
/// seated players, the crate on the shelf in the truck...), parents before their
/// children so a receiver can create them in order.
pub async fn descendants_of(
    _props: &DashMap<String, GorcObjectId>,
    gorc: &Arc<GorcInstanceManager>,
    root_uuid: &str,
) -> Vec<GenericPropsRequest> {
    let mut items = Vec::new();
    for (_, uuid) in crate::children::descendants_of(root_uuid, MAX_CHAIN_DEPTH) {
        if let Some(item) = full_item(gorc, &uuid).await {
            items.push(item);
        }
    }
    items
}

pub fn inject_world(object_data: &mut Value, world: &ObjectWorld) {
    if let Some(map) = object_data.as_object_mut() {
        map.insert(
            ObjectWorld::KEY.to_string(),
            serde_json::to_value(world).unwrap_or(Value::Null),
        );
    }
}

pub fn inject_global_position(object_data: &mut Value, global_position: Vec3) {
    if let Some(map) = object_data.as_object_mut() {
        map.insert(
            ObjectWorld::GLOBAL_POSITION_KEY.to_string(),
            serde_json::json!({"x": global_position.x, "y": global_position.y, "z": global_position.z}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(uuid: &str, ty: &str, parent: Option<&str>, pos: (f64, f64, f64)) -> ChainNode {
        ChainNode {
            uuid: uuid.into(),
            object_type: ty.into(),
            name: Some(format!("{}_name", uuid)),
            parent_id: parent.map(|s| s.to_string()),
            local_position: Vec3::new(pos.0, pos.1, pos.2),
        }
    }

    #[test]
    fn player_in_vehicle_on_planet() {
        let chain = vec![
            node("v", "vehicle", Some("p"), (10.0, 0.0, 0.0)),
            node("p", "planet", None, (1e9, 0.0, 0.0)),
        ];
        let w = fold_world(Vec3::new(1.0, 2.0, 3.0), &chain);
        assert_eq!(w.world, WorldKind::Planet);
        assert_eq!(w.planet_uuid.as_deref(), Some("p"));
        assert_eq!(w.planet_name.as_deref(), Some("p_name"));
        assert_eq!(w.chain.len(), 2);
        // vehicle-local + vehicle position on the planet; the planet's orbit is NOT added
        assert_eq!(w.local_position, Point3::new(11.0, 2.0, 3.0));
    }

    #[test]
    fn moon_is_its_own_world() {
        let chain = vec![
            node("moon", "planet", Some("p"), (5e5, 0.0, 0.0)),
            node("p", "planet", None, (1e9, 0.0, 0.0)),
        ];
        let w = fold_world(Vec3::new(1.0, 0.0, 0.0), &chain);
        assert_eq!(w.planet_uuid.as_deref(), Some("moon"));
        assert_eq!(w.chain.len(), 1);
        assert_eq!(w.local_position, Point3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn ship_in_space() {
        let chain = vec![node("ship", "vehicle", None, (100.0, 0.0, 0.0))];
        let w = fold_world(Vec3::new(1.0, 0.0, 0.0), &chain);
        assert_eq!(w.world, WorldKind::Space);
        assert_eq!(w.local_position, Point3::new(101.0, 0.0, 0.0));
        assert_eq!(w.chain[0].object_type, "vehicle");
    }

    #[test]
    fn no_parent_is_space_at_own_position() {
        let w = fold_world(Vec3::new(7.0, 8.0, 9.0), &[]);
        assert_eq!(w.world, WorldKind::Space);
        assert!(w.chain.is_empty());
        assert_eq!(w.local_position, Point3::new(7.0, 8.0, 9.0));
    }
}

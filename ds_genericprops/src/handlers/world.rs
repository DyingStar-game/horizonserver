//! Resolves the **world** an object lives in (space or one planet) by walking its
//! `parent_id` chain: player > vehicle > planet > space. genericprops is the only
//! crate that can read `parent_id` (it lives inside `GenericProps.data`), so the
//! result is serialised into `object_data["_world"]` for ds_game_server.

use dashmap::DashMap;
use ds_common::events::GenericPropsRequest;
use ds_common::world::{ObjectWorld, Point3, WorldChainEntry, WorldKind};
use ds_common::zone::{is_world_object, zones_contain, Zone};
use horizon_event_system::{GorcInstanceManager, GorcObjectId, Vec3};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::genericprops::GenericProps;

/// Longest parent chain we follow before giving up (guards against cycles too).
pub(crate) const MAX_CHAIN_DEPTH: usize = 8;

/// What the resolver needs from one ancestor.
#[derive(Debug, Clone)]
pub struct ChainNode {
    pub uuid: String,
    pub object_type: String,
    pub name: Option<String>,
    pub parent_id: Option<String>,
    /// Position local to ITS parent (for a planet: `positions[0]`).
    pub local_position: Vec3,
    /// Orientation local to ITS parent: what turns the positions of its children (crate::frame).
    pub local_rotation: crate::frame::Basis,
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

/// The value `flatten_props` gives `key` (the last channel holding it), without
/// copying the other properties.
fn flat_value<'a>(gp: &'a GenericProps, key: &str) -> Option<&'a Value> {
    gp.data.values().filter_map(|properties| properties.as_object()?.get(key)).last()
}

/// `object_def.get_position` of the flattened properties, reading only the first
/// of `positions` and `position`: a planet carries big property maps, and a world
/// snapshot read them for every object standing on it.
fn local_position_of(gp: &GenericProps) -> Vec3 {
    let mut probe = Map::new();
    if let Some(first) = flat_value(gp, "positions").and_then(|v| v.as_array()).and_then(|a| a.first()) {
        probe.insert("positions".to_string(), Value::Array(vec![first.clone()]));
    }
    if let Some(position) = flat_value(gp, "position") {
        probe.insert("position".to_string(), position.clone());
    }
    gp.object_def.get_position(&Value::Object(probe))
}

/// Own local position + parent, as stored on the instance.
pub fn read_own_local_and_parent(gp: &GenericProps) -> (Vec3, Option<String>) {
    (local_position_of(gp), read_parent_id(gp))
}

pub fn chain_node_of(gp: &GenericProps) -> ChainNode {
    ChainNode {
        uuid: gp.uuid.clone(),
        object_type: gp.object_def.name.clone(),
        name: flat_value(gp, "name").and_then(|v| v.as_str()).map(|s| s.to_string()),
        parent_id: read_parent_id(gp),
        local_position: local_position_of(gp),
        local_rotation: crate::frame::local_basis(gp),
    }
}

/// Ancestors already read while resolving several worlds in one go (a snapshot):
/// the objects of a village share its buildings and its planet, which were read
/// again from GORC for each of them. `None`: not in GORC.
#[derive(Default)]
pub struct ChainCache(std::collections::HashMap<String, Option<ChainNode>>);

impl ChainCache {
    async fn node(&mut self, gorc: &Arc<GorcInstanceManager>, uuid: &str, gorc_id: GorcObjectId) -> Option<ChainNode> {
        if let Some(node) = self.0.get(uuid) {
            return node.clone();
        }
        let node = gorc
            .with_object_mut(gorc_id, |instance| instance.get_object::<GenericProps>().map(chain_node_of))
            .await
            .flatten();
        self.0.insert(uuid.to_string(), node.clone());
        node
    }
}

/// Pure fold: walks `chain` (direct parent first) accumulating local positions,
/// each turned by its parent's orientation, until the first planet, which becomes the world. Anything past the planet is
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
        local = crate::frame::compose(node.local_position, &node.local_rotation, local);
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
    resolve_world_cached(gorc, &mut ChainCache::default(), own_local, parent_id).await
}

/// `resolve_world`, reading the ancestors through `cache`.
pub async fn resolve_world_cached(
    gorc: &Arc<GorcInstanceManager>,
    cache: &mut ChainCache,
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
        match cache.node(gorc, &pid, gorc_id).await {
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

/// Parents a player belongs with on the mesh: a player in its apartment is on the server
/// of its hab, wherever the apartment lies. Godot already moves a player on a prop with the
/// prop; ruled by its own position, a player whose apartment was 3.9 m across a zone border
/// from its hab's origin landed on a server without the hab ("parent_id not found",
/// preprod 2026-10-21).
const CARRIERS: &[&str] = &["spawnbuilding"];

/// The world of a player for the mesh: `resolve_world`, except that a player whose direct
/// parent is a carrier (CARRIERS) takes the carrier's world (its position, the carrier first
/// in the chain), so every server decision keeps it with its hab.
pub async fn resolve_player_world(
    gorc: &Arc<GorcInstanceManager>,
    own_local: Vec3,
    parent_id: Option<String>,
) -> ObjectWorld {
    resolve_player_world_cached(gorc, &mut ChainCache::default(), own_local, parent_id).await
}

/// `resolve_player_world`, reading the ancestors through `cache`.
pub async fn resolve_player_world_cached(
    gorc: &Arc<GorcInstanceManager>,
    cache: &mut ChainCache,
    own_local: Vec3,
    parent_id: Option<String>,
) -> ObjectWorld {
    let carrier = match parent_id.as_deref().and_then(|pid| Some((pid, GorcObjectId::from_str(pid).ok()?))) {
        Some((pid, gorc_id)) => cache
            .node(gorc, pid, gorc_id)
            .await
            .filter(|node| CARRIERS.contains(&node.object_type.as_str())),
        None => None,
    };
    match carrier {
        Some(node) => {
            let mut world = resolve_world_cached(gorc, cache, node.local_position, node.parent_id.clone()).await;
            world.chain.insert(0, WorldChainEntry { uuid: node.uuid, object_type: node.object_type });
            world
        }
        None => resolve_world_cached(gorc, cache, own_local, parent_id).await,
    }
}

/// `resolve_player_world` for a player, `resolve_world` for anything else.
pub async fn resolve_world_of(
    gorc: &Arc<GorcInstanceManager>,
    object_type: &str,
    own_local: Vec3,
    parent_id: Option<String>,
) -> ObjectWorld {
    resolve_world_of_cached(gorc, &mut ChainCache::default(), object_type, own_local, parent_id).await
}

/// `resolve_world_of`, reading the ancestors through `cache`.
pub async fn resolve_world_of_cached(
    gorc: &Arc<GorcInstanceManager>,
    cache: &mut ChainCache,
    object_type: &str,
    own_local: Vec3,
    parent_id: Option<String>,
) -> ObjectWorld {
    if object_type == "player" {
        resolve_player_world_cached(gorc, cache, own_local, parent_id).await
    } else {
        resolve_world_cached(gorc, cache, own_local, parent_id).await
    }
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
    let object_world = resolve_world_of(gorc, &object_type, own_local, parent_id).await;
    let mut object_data = Value::Object(props);
    inject_global_position(&mut object_data, global_position);
    inject_world(&mut object_data, &object_world);
    Some(GenericPropsRequest { object_type, object_uuid: uuid.to_string(), object_data, broadcast_only: None })
}

/// What a snapshot keeps: the objects lying in `zones`, the ones listed in `also`,
/// and the world objects (planets, stars), which every server needs.
pub struct SnapshotScope {
    pub zones: Vec<Zone>,
    pub also: HashSet<String>,
}

impl SnapshotScope {
    fn keeps(&self, uuid: &str, object_type: &str, world: &ObjectWorld) -> bool {
        is_world_object(object_type) || zones_contain(&self.zones, world) || self.also.contains(uuid)
    }
}

/// The full items of `uuids` (see `full_item`), only those `scope` keeps when given.
/// The worlds share one `ChainCache`, and an object out of scope is never flattened:
/// a split asked for the whole world (52k objects, preprod 2026-11-03), which took
/// 2.6 s to build and as long again to reach ds_game_server, past its 5 s timeout.
pub async fn snapshot(
    gorc: &Arc<GorcInstanceManager>,
    uuids: Vec<String>,
    scope: Option<&SnapshotScope>,
) -> HashMap<String, GenericPropsRequest> {
    let mut items = HashMap::new();
    let mut chains = ChainCache::default();
    for uuid in uuids {
        let Ok(gorc_id) = GorcObjectId::from_str(&uuid) else { continue };
        // Read under the lock, resolve the world after (it awaits on GORC for each ancestor).
        let head = gorc
            .with_object_mut(gorc_id, |instance| {
                instance.get_object::<GenericProps>().map(|gp| {
                    (gp.object_def.name.clone(), gp.global_position, read_own_local_and_parent(gp))
                })
            })
            .await
            .flatten();
        let Some((object_type, global_position, (own_local, parent_id))) = head else {
            debug!("[world] snapshot: {} not in GORC, skipped", uuid);
            continue;
        };
        let object_world = resolve_world_of_cached(gorc, &mut chains, &object_type, own_local, parent_id).await;
        if scope.is_some_and(|scope| !scope.keeps(&uuid, &object_type, &object_world)) {
            continue;
        }
        let Some(properties) = gorc
            .with_object_mut(gorc_id, |instance| instance.get_object::<GenericProps>().map(flatten_props))
            .await
            .flatten()
        else {
            continue;
        };
        let mut object_data = Value::Object(properties);
        inject_global_position(&mut object_data, global_position);
        inject_world(&mut object_data, &object_world);
        items.insert(uuid.clone(), GenericPropsRequest { object_type, object_uuid: uuid, object_data, broadcast_only: None });
    }
    items
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
            local_rotation: crate::frame::Basis::IDENTITY,
        }
    }

    #[test]
    fn rotated_vehicle_turns_its_passenger() {
        // A vehicle turned 90° about Y: its local +X is the planet's -Z.
        let mut vehicle = node("v", "vehicle", Some("p"), (10.0, 0.0, 0.0));
        vehicle.local_rotation =
            crate::frame::Basis::from_euler_yxz(Vec3::new(0.0, std::f64::consts::FRAC_PI_2, 0.0));
        let chain = vec![vehicle, node("p", "planet", None, (1e9, 0.0, 0.0))];
        let w = fold_world(Vec3::new(2.0, 0.0, 0.0), &chain);
        assert!((w.local_position.x - 10.0).abs() < 1e-9);
        assert!((w.local_position.z + 2.0).abs() < 1e-9);
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

    use crate::objectdefinition::ObjectDefinition;
    use serde_json::json;

    /// Registers an object of type `ty` (every property on channel 0) and returns its uuid.
    async fn register(gorc: &Arc<GorcInstanceManager>, ty: &str, data: Value) -> String {
        let definition = ObjectDefinition::new(ty.to_string(), json!({"channels": [
            {"zone": 0, "distance": 100.0, "frequency": 10.0,
             "properties": ["name", "position", "positions", "rotation", "parent_id", "terrain"]}
        ]}));
        let id = GorcObjectId::new();
        let object = GenericProps::new(definition, data, id.to_string());
        gorc.register_object_with_uuid(object, Vec3::zero(), Some(id)).await;
        id.to_string()
    }

    /// Two planets, each with a hab and props in it; `props_per_planet` props each.
    async fn two_planets(props_per_planet: usize) -> (Arc<GorcInstanceManager>, [String; 2], Vec<String>) {
        let gorc = Arc::new(GorcInstanceManager::new());
        // A planet carries big properties: the snapshot must not copy them per object.
        let terrain: Vec<Value> = (0..2000).map(|i| json!({"x": i, "y": i, "z": i})).collect();
        let mut planets = Vec::new();
        let mut uuids = Vec::new();
        for (i, name) in ["SandBox", "Tarsis 1"].iter().enumerate() {
            let planet = register(&gorc, "planet", json!({
                "name": name, "positions": [{"x": 1e9 * (i as f64 + 1.0), "y": 0.0, "z": 0.0}], "terrain": terrain,
            })).await;
            let hab = register(&gorc, "spawnbuilding", json!({
                "position": {"x": 100.0, "y": 0.0, "z": 0.0}, "parent_id": planet,
            })).await;
            uuids.extend([planet.clone(), hab.clone()]);
            for p in 0..props_per_planet {
                uuids.push(register(&gorc, "crate", json!({
                    "position": {"x": p as f64, "y": 0.0, "z": 0.0}, "parent_id": hab,
                })).await);
            }
            planets.push(planet);
        }
        (gorc, [planets[0].clone(), planets[1].clone()], uuids)
    }

    #[tokio::test]
    async fn a_scoped_snapshot_keeps_its_zones_the_worlds_and_also() {
        let (gorc, [sandbox, tarsis], uuids) = two_planets(3).await;
        let elsewhere = uuids[uuids.len() - 1].clone(); // a crate on Tarsis 1
        let scope = SnapshotScope { zones: vec![Zone::planet(&sandbox, "SandBox")], also: HashSet::from([elsewhere.clone()]) };

        let items = snapshot(&gorc, uuids.clone(), Some(&scope)).await;

        let worlds = |ty: &str| items.values().filter(|i| i.object_type == ty).count();
        assert_eq!(worlds("planet"), 2, "both planets: every server needs its reference frames");
        assert!(items.contains_key(&elsewhere), "listed in `also`");
        let on_tarsis = items.values().filter(|i| {
            i.object_type != "planet" && ObjectWorld::from_object_data(&i.object_data).unwrap().planet_uuid.as_deref() == Some(tarsis.as_str())
        });
        assert_eq!(on_tarsis.count(), 1, "nothing else of Tarsis 1");
        assert_eq!(items.len(), 2 + 1 + 3 + 1, "planets, SandBox's hab and crates, the `also` crate");

        // Same items as one full_item each (no cache), for what is kept.
        for (uuid, item) in &items {
            let alone = full_item(&gorc, uuid).await.unwrap();
            assert_eq!(item.object_data, alone.object_data, "{} ({})", uuid, item.object_type);
        }
        // Unscoped, the whole world.
        assert_eq!(snapshot(&gorc, uuids.clone(), None).await.len(), uuids.len());
    }

    /// Timing only: `cargo test -p dyingstar_generic_props snapshot_cost -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn snapshot_cost() {
        let (gorc, [sandbox, _], uuids) = two_planets(10_000).await;
        // The code before 2026-11-03: every property flattened to read one position,
        // for the object and again for each of its ancestors.
        let flat_position = |gp: &GenericProps| gp.object_def.get_position(&Value::Object(flatten_props(gp)));
        let started = std::time::Instant::now();
        let mut old = HashMap::new();
        for uuid in &uuids {
            let id = GorcObjectId::from_str(uuid).unwrap();
            let Some((ty, props, global, local, mut next)) = gorc
                .with_object_mut(id, |i| i.get_object::<GenericProps>().map(|gp| {
                    (gp.object_def.name.clone(), flatten_props(gp), gp.global_position, flat_position(gp), gp.parent_id())
                }))
                .await
                .flatten() else { continue };
            let mut chain = Vec::new();
            while let Some(pid) = next.take() {
                let node = gorc
                    .with_object_mut(GorcObjectId::from_str(&pid).unwrap(), |i| i.get_object::<GenericProps>().map(|gp| {
                        let flat = flatten_props(gp);
                        ChainNode {
                            uuid: gp.uuid.clone(),
                            object_type: gp.object_def.name.clone(),
                            name: flat.get("name").and_then(|v| v.as_str()).map(str::to_string),
                            parent_id: gp.parent_id(),
                            local_position: gp.object_def.get_position(&Value::Object(flat)),
                            local_rotation: crate::frame::local_basis(gp),
                        }
                    }))
                    .await
                    .flatten()
                    .unwrap();
                next = if node.object_type == "planet" { None } else { node.parent_id.clone() };
                chain.push(node);
            }
            let mut object_data = Value::Object(props);
            inject_global_position(&mut object_data, global);
            inject_world(&mut object_data, &fold_world(local, &chain));
            old.insert(uuid.clone(), GenericPropsRequest { object_type: ty, object_uuid: uuid.clone(), object_data, broadcast_only: None });
        }
        let old_cost = started.elapsed();
        let started = std::time::Instant::now();
        let whole = snapshot(&gorc, uuids.clone(), None).await;
        let whole_cost = started.elapsed();
        let scope = SnapshotScope { zones: vec![Zone::planet(&sandbox, "SandBox")], also: HashSet::new() };
        let started = std::time::Instant::now();
        let scoped = snapshot(&gorc, uuids.clone(), Some(&scope)).await;
        let scoped_cost = started.elapsed();
        let json_len = |items: &HashMap<String, GenericPropsRequest>| serde_json::to_vec(items).unwrap().len();
        println!(
            "{} objects: before {:?} ({} B JSON) | now, whole world {:?} | scoped to one planet {:?} ({} items, {} B JSON)",
            uuids.len(), old_cost, json_len(&old), whole_cost, scoped_cost, scoped.len(), json_len(&scoped)
        );
        assert_eq!(whole.len(), old.len());
    }
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use futures::executor::block_on;
use horizon_event_system::{EventSystem, GorcInstanceManager, GorcObjectId, PlayerId, Vec3};
use tokio::sync::Mutex;
use tracing::{debug, error, warn, info};

use crate::genericprops::{distance_squared, GenericProps};
use crate::handlers::mesh_load::{self, LoadView};
use crate::handlers::world::{read_own_local_and_parent, resolve_world};
use ds_common::world::ObjectWorld;

/// How new players are spread over the mining villages (`[ds_genericprops]` in plugins.toml).
#[derive(Debug, Clone, Copy)]
pub struct VillageConfig {
    /// Players a village takes from solo players, whatever room its buildings physically have.
    pub max_players_per_village: usize,
    /// Places (under the cap) kept ahead of the arrivals: whenever the villages already
    /// spawned or requested offer fewer, the closest unspawned ones are requested to Godot.
    /// Godot needs from ~15 s to minutes (during a mesh hand-over) to create a village's
    /// habs; every player arriving meanwhile with no room gets no apartment.
    pub free_reserve: usize,
}

impl Default for VillageConfig {
    fn default() -> Self {
        Self { max_players_per_village: 50, free_reserve: 100 }
    }
}

static VILLAGE_CONFIG: OnceLock<VillageConfig> = OnceLock::new();

/// Set once from `on_init`; later calls are ignored.
pub fn set_village_config(config: VillageConfig) {
    let _ = VILLAGE_CONFIG.set(config);
}

fn village_config() -> VillageConfig {
    VILLAGE_CONFIG.get().copied().unwrap_or_default()
}

/// Who the apartment is for. Today always the default (a solo player): the matchmaking will
/// later send a player joining friends / a group to their village, past the cap.
#[derive(Debug, Clone, Default)]
pub struct AssignRequest {
    /// uuid of the poi_village the player should join.
    pub preferred_village: Option<String>,
    /// Allow the preferred village past `max_players_per_village`, within its physical room.
    pub allow_over_cap: bool,
}

/// A poi_village and the spawnbuildings linked to it through their `poi_uuid`.
#[derive(Debug, Clone)]
struct VillageState {
    gorc_id: Option<GorcObjectId>,
    uuid: String,
    name: String,
    position: Vec3,
    /// Planet and planet-local position: tells which server's zone holds it.
    world: Option<ObjectWorld>,
    is_spawned: bool,
    spawn_requested: bool,
    /// Apartments already given in its buildings.
    occupancy: usize,
    /// Physical free apartments left in its buildings.
    free: usize,
    buildings: Vec<GorcObjectId>,
}

impl VillageState {
    fn has_room_under_cap(&self, max: usize) -> bool {
        self.occupancy < max && self.free > 0
    }

    /// Godot already created (or was asked to create) its habs.
    fn is_spawn_pending_or_done(&self) -> bool {
        self.is_spawned || self.spawn_requested || !self.buildings.is_empty()
    }

    /// Requested from Godot, its habs not arrived in Horizon yet.
    fn is_spawn_pending(&self) -> bool {
        (self.is_spawned || self.spawn_requested) && self.buildings.is_empty()
    }
}

/// Whether a poi_village of this `type` houses players. Only the mining villages have
/// spawnbuildings: a factory city has none, and counting it would hand the reserve a full cap
/// of places that never arrive (free_reserve counts a requested village with no habs as `max`).
/// A village with no `type` (data from before the field) still counts. One whose `type` reads
/// "poi_village" had it overwritten by a game server update (see update.rs, without_type_echo):
/// its layout tells, a factory city's spawn_scene being ares_city_factory.
fn houses_players(village_type: Option<&str>, spawn_scene: Option<&str>) -> bool {
    match village_type {
        None => true,
        Some("poi_village") => !spawn_scene.is_some_and(|s| s.contains("factory")),
        Some(t) => t == "mining",
    }
}

/// The value of `key` in whichever channel of the prop holds it.
fn prop_value<'a>(prop: &'a GenericProps, key: &str) -> Option<&'a serde_json::Value> {
    prop.data.values().find_map(|v| v.get(key))
}

fn prop_f64(prop: &GenericProps, key: &str, default: f64) -> f64 {
    prop_value(prop, key).and_then(|v| v.as_f64()).unwrap_or(default)
}

fn prop_i64(prop: &GenericProps, key: &str, default: i64) -> i64 {
    prop_value(prop, key).and_then(|v| v.as_i64()).unwrap_or(default)
}

fn prop_bool(prop: &GenericProps, key: &str) -> bool {
    prop_value(prop, key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn apartments(building: &GenericProps) -> Vec<serde_json::Value> {
    prop_value(building, "apartments")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

fn apartment_slot(entry: &serde_json::Value) -> Option<(i64, i64, i64)> {
    let f = entry.get("floor").and_then(|v| v.as_i64())?;
    let r = entry.get("row").and_then(|v| v.as_i64())?;
    let c = entry.get("col").and_then(|v| v.as_i64())?;
    Some((f, r, c))
}

/// Spawn position of an apartment, local to its building.
fn slot_position(building: &GenericProps, (floor, row, col): (i64, i64, i64)) -> Vec3 {
    let x_spacing = prop_f64(building, "x_spacing", 5.0);
    let y_spacing = prop_f64(building, "y_spacing", 5.0);
    let z_spacing = prop_f64(building, "z_spacing", 5.0);
    let spawn_point = prop_value(building, "spawn_point");
    let spawn_point_axis = |axis: &str| spawn_point.and_then(|s| s.get(axis)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let (spawn_point_x, spawn_point_y, spawn_point_z) = (spawn_point_axis("x"), spawn_point_axis("y"), spawn_point_axis("z"));

    let x = if col == 0 { spawn_point_x } else { x_spacing - spawn_point_x };
    let z = if col == 0 { spawn_point_z } else { z_spacing - spawn_point_z };
    Vec3::new(
        x + (row as f64 * x_spacing),
        spawn_point_y + (floor as f64 * y_spacing),
        z + (col as f64 * z_spacing),
    )
}

/// The village a player gets an apartment in, or None when every village is at its cap.
/// Solo players fill the fullest village still under the cap first (ties by name), so the
/// villages fill one after the other, starting with mining_village_01. With the pool load
/// known, that happens within the zones of the least loaded server that takes players:
/// filling the villages in line put every arrival on the server owning the next one.
fn select_village<'a>(
    villages: &'a [VillageState],
    request: &AssignRequest,
    max: usize,
    load: Option<&LoadView>,
) -> Option<&'a VillageState> {
    if let Some(preferred) = &request.preferred_village {
        let preferred = villages.iter().find(|v| &v.uuid == preferred).filter(|v| {
            if request.allow_over_cap { v.free > 0 } else { v.has_room_under_cap(max) }
        });
        if preferred.is_some() {
            return preferred;
        }
    }
    // Never on a frozen server, even as the last resort: better wait for habs elsewhere.
    let with_room = villages
        .iter()
        .filter(|v| v.has_room_under_cap(max))
        .filter(|v| !load.is_some_and(|l| l.owner_unresponsive(v.world.as_ref())));
    match load {
        Some(load) => with_room.min_by(|a, b| {
            load.rank(a.world.as_ref())
                .cmp(&load.rank(b.world.as_ref()))
                .then_with(|| b.occupancy.cmp(&a.occupancy))
                .then_with(|| a.name.cmp(&b.name))
        }),
        None => with_room.max_by(|a, b| a.occupancy.cmp(&b.occupancy).then_with(|| b.name.cmp(&a.name))),
    }
}

/// At most this many villages requested and not spawned yet per server: Godot freezes
/// 16-20 s per village it spawns, and one frozen server was sent 40+ requests in a row
/// (minikube, 2026-10-02) which kept it frozen for good.
const MAX_PENDING_VILLAGES_PER_SERVER: usize = 2;

fn in_accepting_zone(village: &VillageState, load: &LoadView) -> bool {
    village.world.as_ref().and_then(|w| load.owner_of(w)).map_or(false, |s| load.accepts(s))
}

/// The load to place arrivals on, when some server taking players owns a village. A
/// server that takes players but owns none (space, the other planets) cannot take any:
/// counting only its zones kept the reserve at 0 and requested every village of
/// SandBox on the frozen server that owned them (2026-10-02).
fn arrival_load<'a>(villages: &[VillageState], load: Option<&'a LoadView>) -> Option<&'a LoadView> {
    load.filter(|l| villages.iter().any(|v| in_accepting_zone(v, l)))
}

/// Villages that count for new players: with an arrival load, only those in the zones
/// of a server taking players; room elsewhere would never be given.
fn counts_for_arrivals(village: &VillageState, load: Option<&LoadView>) -> bool {
    load.map_or(true, |l| in_accepting_zone(village, l))
}

/// The village the next one is searched around when none has room: the fullest one with
/// buildings, i.e. the last village the players were sent to.
fn last_full_village(villages: &[VillageState]) -> Option<&VillageState> {
    villages
        .iter()
        .filter(|v| !v.buildings.is_empty())
        .max_by(|a, b| a.occupancy.cmp(&b.occupancy).then_with(|| b.name.cmp(&a.name)))
}

/// How long a player with no apartment waits for village habs already requested from Godot
/// (at startup, Godot loads the planets for ~2 min before it spawns the first village);
/// past it, the player gets `server not ready` and is disconnected. Kept under the client's
/// SPAWN_TIMEOUT_MS (45 s from init_ack, client.gd), or the client gives up first with
/// "player never spawned".
const HABS_WAIT: Duration = Duration::from_secs(40);
/// How often the waiting players look for an apartment again when nothing arrived
/// meanwhile; habs or a new player arriving trigger a pass at once.
const HABS_RETRY: Duration = Duration::from_secs(2);
/// Arrivals within this window are placed in one pass: a village's habs come as ~10
/// spawnbuildings within milliseconds, the players in waves.
const COALESCE: Duration = Duration::from_millis(100);
/// Between the `server not ready` error and the close frame: the error goes through the
/// connection's outgoing queue, the close frame does not.
const KICK_DELAY: Duration = Duration::from_millis(500);
/// The `no unspawned poi_village left` warning, at most this often (once per player and
/// per second before: 7000 lines per 30 s on preprod, 2026-10-16).
const RESERVE_WARN_EVERY: Duration = Duration::from_secs(10);
/// What the client gets when no apartment could be prepared in time (same error as
/// player_init on a server not ready yet: the client shows it and closes the connection).
const NOT_READY_CODE: i64 = 1338;
const NOT_READY_MESSAGE: &str = "Server not ready";

fn player_id(player_uuid: &str) -> Option<PlayerId> {
    PlayerId::from_str(player_uuid).ok()
}

/// Whether the player's connection is still open (true when it cannot be told).
async fn player_connected(events: &EventSystem, player_uuid: &str) -> bool {
    match (events.get_client_response_sender(), player_id(player_uuid)) {
        (Some(sender), Some(player_id)) => sender.is_connection_active(player_id).await,
        _ => true,
    }
}

/// Send the `server not ready` error to the player; `kick` closes the connection KICK_DELAY
/// later. False when the player cannot be reached.
async fn send_not_ready(events: &EventSystem, player_uuid: &str) -> bool {
    let (Some(sender), Some(player_id)) = (events.get_client_response_sender(), player_id(player_uuid)) else {
        error!("plugin genericprops (new_player): cannot reach player {} to tell the server is not ready", player_uuid);
        return false;
    };
    let payload = serde_json::json!({ "type": "error", "code": NOT_READY_CODE, "message": NOT_READY_MESSAGE });
    if let Err(e) = sender.send_to_client(player_id, payload.to_string().into_bytes()).await {
        warn!("plugin genericprops (new_player): failed to send server-not-ready to player {}: {}", player_uuid, e);
    }
    true
}

async fn kick(events: &EventSystem, player_uuid: &str) {
    if let (Some(sender), Some(player_id)) = (events.get_client_response_sender(), player_id(player_uuid)) {
        if let Err(e) = sender.kick(player_id, Some(NOT_READY_MESSAGE.to_string())).await {
            debug!("plugin genericprops (new_player): kick of player {}: {}", player_uuid, e);
        }
    }
}

/// The village the first players are sent to: its habs are requested at startup
/// (`spawn_requested` in startup_items.json), no spawnbuilding exists before them.
const FIRST_VILLAGE: &str = "M0001";

/// The village to start from when none has buildings yet: FIRST_VILLAGE, else the first by name.
fn first_village(villages: &[VillageState]) -> Option<&VillageState> {
    villages
        .iter()
        .find(|v| v.name == FIRST_VILLAGE)
        .or_else(|| villages.iter().min_by(|a, b| a.name.cmp(&b.name)))
}

/// Places solo players can still get: the room under the cap of the villages with habs,
/// plus a full cap for each village whose habs were requested and have not arrived yet.
/// Nothing on a frozen server: its habs come back only with it, the reserve is kept elsewhere.
fn free_reserve(villages: &[VillageState], max: usize, load: Option<&LoadView>) -> usize {
    let frozen = |v: &VillageState| load.is_some_and(|l| l.owner_unresponsive(v.world.as_ref()));
    let load = arrival_load(villages, load);
    villages
        .iter()
        .filter(|v| counts_for_arrivals(v, load))
        .filter(|v| !frozen(v))
        .map(|v| {
            if !v.buildings.is_empty() {
                v.free.min(max.saturating_sub(v.occupancy))
            } else if v.is_spawn_pending() {
                max
            } else {
                0
            }
        })
        .sum()
}

/// The unspawned villages to request so the reserve gets back to `target`: in the zones
/// of the least loaded server taking players when the load is known, then closest to
/// `reference`. Several can be requested at once: a wave of arrivals outruns one
/// village at a time (preprod, 2026-10-01: 16 players without apartment in 14 s).
fn villages_to_request<'a>(
    villages: &'a [VillageState],
    reference: &VillageState,
    max: usize,
    target: usize,
    load: Option<&LoadView>,
) -> Vec<&'a VillageState> {
    let mut reserve = free_reserve(villages, max, load);
    if reserve >= target || max == 0 {
        return Vec::new();
    }
    // Not on a server frozen or loading the zones it was just handed: a split gave unde
    // its zone and a village to spawn in the same second, and it froze 2 min with them
    // (preprod, 2026-10-16). Asked again once it reports, on a later arrival.
    let mut candidates: Vec<&VillageState> = villages
        .iter()
        .filter(|v| v.uuid != reference.uuid && !v.is_spawn_pending_or_done())
        .filter(|v| load.map_or(true, |l| l.owner_takes_habs(v.world.as_ref())))
        .collect();
    candidates.sort_by(|a, b| {
        let rank = |v: &VillageState| load.map(|l| l.rank(v.world.as_ref()));
        rank(a).cmp(&rank(b)).then_with(|| {
            distance_squared(a.position, reference.position)
                .total_cmp(&distance_squared(b.position, reference.position))
        })
    });
    // With the load known, no more than MAX_PENDING_VILLAGES_PER_SERVER waiting per server.
    let owner = |v: &VillageState| -> Option<String> {
        load.and_then(|l| v.world.as_ref().and_then(|w| l.owner_of(w)).map(|s| s.uuid.clone()))
    };
    let mut pending: HashMap<Option<String>, usize> = HashMap::new();
    if load.is_some() {
        for v in villages.iter().filter(|v| v.is_spawn_pending()) {
            *pending.entry(owner(v)).or_default() += 1;
        }
    }
    let mut chosen = Vec::new();
    for village in candidates {
        if reserve >= target {
            break;
        }
        if load.is_some() {
            let waiting = pending.entry(owner(village)).or_default();
            if *waiting >= MAX_PENDING_VILLAGES_PER_SERVER {
                continue;
            }
            *waiting += 1;
        }
        reserve += max;
        chosen.push(village);
    }
    chosen
}

/// Every poi_village that houses players (see houses_players), with the occupancy of the
/// spawnbuildings linked to it by `poi_uuid`.
async fn collect_villages(gorc_instances: &Arc<GorcInstanceManager>) -> Vec<VillageState> {
    let mut villages: Vec<VillageState> = Vec::new();
    let mut anchors: Vec<(usize, Vec3, Option<String>)> = Vec::new();
    for gorc_id in gorc_instances.get_objects_by_type("poi_village").await {
        let village = gorc_instances
            .with_object_mut(gorc_id, |instance| {
                instance.get_object::<GenericProps>()
                    .filter(|v| houses_players(
                        prop_value(v, "type").and_then(|t| t.as_str()),
                        prop_value(v, "spawn_scene").and_then(|s| s.as_str()),
                    ))
                    .map(|v| (VillageState {
                    gorc_id: Some(gorc_id),
                    uuid: v.uuid.clone(),
                    name: prop_value(v, "name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
                    // Local to the planet, like every village: comparable between villages.
                    position: prop_value(v, "position")
                        .and_then(|p| serde_json::from_value::<Vec3>(p.clone()).ok())
                        .unwrap_or_else(Vec3::zero),
                    world: None,
                    is_spawned: prop_bool(v, "is_spawned"),
                    spawn_requested: prop_bool(v, "spawn_requested"),
                    occupancy: 0,
                    free: 0,
                    buildings: Vec::new(),
                }, read_own_local_and_parent(v)))
            })
            .await
            .flatten();
        if let Some((village, (own_local, parent_id))) = village {
            anchors.push((villages.len(), own_local, parent_id));
            villages.push(village);
        }
    }

    // Villages never move: their world is resolved once.
    for (i, own_local, parent_id) in anchors {
        let cached = village_worlds().lock().await.get(&villages[i].uuid).cloned();
        let world = match cached {
            Some(world) => world,
            None => {
                let world = resolve_world(gorc_instances, own_local, parent_id).await;
                village_worlds().lock().await.insert(villages[i].uuid.clone(), world.clone());
                world
            }
        };
        villages[i].world = Some(world);
    }

    let index: HashMap<String, usize> = villages.iter().enumerate().map(|(i, v)| (v.uuid.clone(), i)).collect();
    let mut orphans = 0;
    for gorc_id in gorc_instances.get_objects_by_type("spawnbuilding").await {
        let building = gorc_instances
            .with_object_mut(gorc_id, |instance| {
                instance.get_object::<GenericProps>().map(|b| {
                    let poi_uuid = prop_value(b, "poi_uuid").and_then(|p| p.as_str()).unwrap_or_default().to_string();
                    let occupancy = prop_value(b, "apartments").and_then(|a| a.as_array()).map_or(0, |a| a.len());
                    (poi_uuid, occupancy, prop_i64(b, "available", 0).max(0) as usize)
                })
            })
            .await
            .flatten();
        let Some((poi_uuid, occupancy, available)) = building else { continue };
        match index.get(&poi_uuid) {
            Some(&i) => {
                let village = &mut villages[i];
                village.occupancy += occupancy;
                village.free += available;
                village.buildings.push(gorc_id);
            }
            None => orphans += 1,
        }
    }
    if orphans > 0 {
        warn!("plugin genericprops (new_player): {} spawnbuilding(s) not linked to a known poi_village (poi_uuid), never used for new players", orphans);
    }
    villages
}

fn village_worlds() -> &'static Mutex<HashMap<String, ObjectWorld>> {
    static WORLDS: OnceLock<Mutex<HashMap<String, ObjectWorld>>> = OnceLock::new();
    WORLDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keep `free_reserve` places ahead of the arrivals: mark the closest unspawned villages to
/// `reference` as `spawn_requested`. Godot (the server owning each zone) then spawns their
/// habs, which come back as spawnbuildings with the village's poi_uuid.
async fn request_villages(
    gorc_instances: &GorcInstanceManager,
    events: &EventSystem,
    villages: &[VillageState],
    reference: &VillageState,
    load: Option<&LoadView>,
) {
    let config = village_config();
    let targets = villages_to_request(villages, reference, config.max_players_per_village, config.free_reserve, load);
    if targets.is_empty() {
        let reserve = free_reserve(villages, config.max_players_per_village, load);
        if reserve < config.free_reserve && reserve_warning_due() {
            warn!(
                "plugin genericprops (new_player): no unspawned poi_village left to request near {} ({} free places, reserve {})",
                reference.name, reserve, config.free_reserve
            );
        }
        return;
    }
    for target in targets {
        info!(
            "plugin genericprops (new_player): requesting the habs of village {} ({}) near {} on {} (free places: {}, reserve: {})",
            target.name, target.uuid, reference.name, owner_label(target, load),
            free_reserve(villages, config.max_players_per_village, load), config.free_reserve
        );
        request_village(gorc_instances, events, target).await;
    }
}

/// Whether the `no unspawned poi_village left` warning may be logged again (RESERVE_WARN_EVERY).
fn reserve_warning_due() -> bool {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    let due = last.map_or(true, |at| at.elapsed() >= RESERVE_WARN_EVERY);
    if due {
        *last = Some(Instant::now());
    }
    due
}

/// Mark `target` as `spawn_requested`, for Godot to spawn its habs.
async fn request_village(gorc_instances: &GorcInstanceManager, events: &EventSystem, target: &VillageState) {
    // In-memory first, on the placement thread: the next pass sees the request and does
    // not send it again.
    if let Some(gorc_id) = target.gorc_id {
        gorc_instances
            .with_object_mut(gorc_id, |instance| {
                if let Some(village) = instance.get_object_mut::<GenericProps>() {
                    village.update(serde_json::json!({ "spawn_requested": true }));
                }
            })
            .await;
    }

    // Persisted, and forwarded to the Godot server owning the village's zone.
    if let Err(e) = events.emit_plugin(
        "genericprops",
        "update_object_from_external",
        &serde_json::json!({
            "object_type": "poi_village",
            "object_uuid": target.uuid,
            "object_data": { "spawn_requested": true },
        }),
    ).await {
        error!("plugin genericprops (new_player): failed to request village {}: {}", target.name, e);
    }
}

/// `name (players)` of the server owning the village, for the logs.
fn owner_label(village: &VillageState, load: Option<&LoadView>) -> String {
    match (load, village.world.as_ref()) {
        (Some(load), Some(world)) => match load.owner_of(world) {
            Some(server) => format!("{} ({} players{})", server.name, load.players(server), if load.accepts(server) { "" } else { ", refusing" }),
            None => "no server".to_string(),
        },
        _ => "unknown server (no load yet)".to_string(),
    }
}

/// Give the player the first free apartment of a building and commit it to GORC.
/// Returns the apartment position (local to the building) and the building uuid.
async fn try_assign_in_building(
    gorc_instances: &GorcInstanceManager,
    events: &EventSystem,
    gorc_id: GorcObjectId,
    player_uuid: &str,
    player_name: &str,
) -> Result<Option<(Vec3, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(mut instance) = gorc_instances.get_object(gorc_id).await else {
        return Ok(None);
    };
    // Scope the mutable building borrow so we can call update_object
    // on `instance` after the borrow is dropped.
    let commit_data = {
        let Some(building) = instance.get_object_mut::<GenericProps>() else {
            return Ok(None);
        };
        let available = prop_i64(building, "available", 0);
        if available <= 0 {
            debug!("plugin genericprops (new_player): spawnbuilding {} has no available apartments", building.uuid);
            return Ok(None);
        }
        let cols = prop_i64(building, "cols", 1);
        let rows = prop_i64(building, "rows", 1);
        let floors = prop_i64(building, "floors", 1);

        // Collect occupied (floor, row, col) tuples from the apartments list.
        // Each entry is an object: { floor, row, col, player_uuid, player_name }.
        let mut apartments = apartments(building);
        let occupied: Vec<(i64, i64, i64)> = apartments.iter().filter_map(apartment_slot).collect();

        // Find the first free (floor, row, col) slot.
        // All three dimensions are 0-based: floors=2 → indices 0 and 1,
        // rows=2 → indices 0 and 1, cols=10 → indices 0–9.
        let free_slot = (0..floors)
            .flat_map(|f| (0..rows).flat_map(move |r| (0..cols).map(move |c| (f, r, c))))
            .find(|slot| !occupied.contains(slot));
        let Some((floor, row, col)) = free_slot else {
            return Ok(None);
        };
        let slot_spawn_position = slot_position(building, (floor, row, col));
        debug!(
            "plugin genericprops (new_player): assigned slot (floor={}, row={}, col={}) in building {} → position {:?}",
            floor, row, col, building.uuid, slot_spawn_position
        );

        apartments.push(serde_json::json!({
            "floor": floor,
            "row": row,
            "col": col,
            "player_uuid": player_uuid,
            "player_name": player_name,
        }));
        info!(
            "plugin genericprops (new_player): updated apartments for building {}: {:?}",
            building.uuid, apartments
        );

        // Mutate the in-memory instance directly, still under the lock.
        // This makes the updated occupancy visible to the next waiter
        // before the lock is released, regardless of when the
        // update_object_from_external async handler actually runs.
        building.update(serde_json::json!({
            "available": available - 1,
            "apartments": apartments.clone(),
        }));

        // Persist the new occupancy (update_object_from_external also forwards it to
        // the Godot server owning the building's zone).
        events.emit_plugin(
            "genericprops",
            "update_object_from_external",
            &serde_json::json!({
                "object_type": "spawnbuilding",
                "object_uuid": building.uuid,
                "object_data": {
                    "available": available - 1,
                    "apartments": apartments,
                },
            }),
        ).await?;

        (slot_spawn_position, building.uuid.clone())
    }; // building borrow ends here — instance is free to move

    // Commit the mutated instance back to GORC immediately, still under
    // the lock, so the next waiter reads fresh occupancy from get_object().
    gorc_instances.update_object(gorc_id, instance).await;
    Ok(Some(commit_data))
}

// ---- placement thread

/// A new player waiting for an apartment, on the placement thread.
struct Arrival {
    player_uuid: String,
    player_name: String,
    is_npc: bool,
    since: Instant,
    /// Looked for an apartment given in an earlier session already.
    home_checked: bool,
    /// `waits for the requested village habs` logged already.
    waiting_logged: bool,
}

enum Placement {
    Arrival(Arrival),
    /// Spawnbuildings were registered: the waiting players may get an apartment now.
    HabsArrived,
}

static PLACEMENT: OnceLock<Sender<Placement>> = OnceLock::new();

/// Start the placement thread. Call once, at plugin startup.
///
/// Every new player used to look for an apartment in its own luminal task, and to poll
/// every second under one global lock while the habs it waited for were not there.
/// Luminal re-polls a pending task at once (it never parks one), so 400-600 players
/// waiting for village habs kept 22-25 cores busy, Horizon CPU-throttled, with one OS
/// thread spawned per poll (preprod NPC test, 2026-10-16). Here the waiting players are a
/// queue owned by one thread, placed in one pass whenever players or habs arrive, and the
/// `new_player` handler returns at once. Assignments are serialized by the thread itself.
///
/// No tokio runtime on this thread (futures' executor), as on luminal: the plugin
/// handlers it emits to run the way they do from there.
pub fn start_placement(events: Arc<EventSystem>) {
    let (tx, rx) = mpsc::channel();
    if PLACEMENT.set(tx).is_err() {
        warn!("plugin genericprops (new_player): placement thread already started");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("genericprops-placement".to_string())
        .spawn(move || run(events, rx));
    match spawned {
        Ok(_) => info!("plugin genericprops (new_player): placement thread started"),
        Err(e) => error!("plugin genericprops (new_player): ❌ failed to start the placement thread: {} - new players will never spawn", e),
    }
}

/// The `new_player` event: queue the player for an apartment, return at once.
pub fn enqueue_new_player(event: &serde_json::Value) {
    let arrival = Arrival {
        player_uuid: event["object_uuid"].as_str().unwrap_or_default().to_string(),
        player_name: event["object_data"]["name"].as_str().unwrap_or_default().to_string(),
        is_npc: event["object_data"]["is_npc"].as_bool().unwrap_or(false),
        since: Instant::now(),
        home_checked: false,
        waiting_logged: false,
    };
    info!(
        "plugin genericprops (new_player): received new_player event for player_uuid={} player_name={}",
        arrival.player_uuid, arrival.player_name
    );
    let player_uuid = arrival.player_uuid.clone();
    match PLACEMENT.get() {
        Some(tx) => {
            if tx.send(Placement::Arrival(arrival)).is_err() {
                error!("plugin genericprops (new_player): placement thread is gone, player {} never spawns", player_uuid);
            }
        }
        None => error!("plugin genericprops (new_player): placement thread not started, player {} never spawns", player_uuid),
    }
}

/// A spawnbuilding was registered: the players waiting for habs look again.
pub fn habs_arrived() {
    if let Some(tx) = PLACEMENT.get() {
        let _ = tx.send(Placement::HabsArrived);
    }
}

fn run(events: Arc<EventSystem>, rx: Receiver<Placement>) {
    let mut waiting: VecDeque<Arrival> = VecDeque::new();
    let mut kicks: Vec<(Instant, String)> = Vec::new();
    let mut retry_at: Option<Instant> = None;
    loop {
        let next = [retry_at, kicks.iter().map(|(at, _)| *at).min()].into_iter().flatten().min();
        let first = match next {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let mut pass = false;
        match first {
            Ok(message) => {
                // Take the whole burst it starts in the same pass.
                let until = Instant::now() + COALESCE;
                let mut message = Some(message);
                while let Some(m) = message.take() {
                    if let Placement::Arrival(arrival) = m {
                        waiting.push_back(arrival);
                    }
                    pass = true;
                    message = rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) = kicks.drain(..).partition(|(at, _)| *at <= now);
        kicks = later;
        for (_, player_uuid) in due {
            block_on(kick(&events, &player_uuid));
        }

        if retry_at.is_some_and(|at| now >= at) {
            pass = true;
        }
        if pass && !waiting.is_empty() {
            block_on(place_waiting(&events, &mut waiting, &mut kicks));
            retry_at = None;
        }
        if waiting.is_empty() {
            retry_at = None;
        } else if retry_at.is_none() {
            retry_at = Some(Instant::now() + HABS_RETRY);
        }
    }
}

/// One pass over the waiting players: their apartment from an earlier session, else a new
/// one; the rest wait for habs, are rejected past HABS_WAIT, or spawn at the origin when no
/// habs are coming at all.
async fn place_waiting(events: &Arc<EventSystem>, waiting: &mut VecDeque<Arrival>, kicks: &mut Vec<(Instant, String)>) {
    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available, {} new player(s) dropped", waiting.len());
        waiting.clear();
        return;
    };

    // Back from an earlier session: the apartment they already have.
    let unchecked: HashSet<String> = waiting.iter().filter(|a| !a.home_checked).map(|a| a.player_uuid.clone()).collect();
    let homes = if unchecked.is_empty() { HashMap::new() } else { existing_apartments(&gorc_instances, &unchecked).await };
    let mut homeless: VecDeque<Arrival> = VecDeque::new();
    for mut arrival in waiting.drain(..) {
        arrival.home_checked = true;
        match homes.get(&arrival.player_uuid) {
            Some((slot, position, uuid)) => {
                info!(
                    "plugin genericprops (new_player): player {} already has apartment {:?} in building {} → position {:?}",
                    arrival.player_uuid, slot, uuid, position
                );
                spawn_player(events, &arrival, *position, uuid, false).await;
            }
            None => homeless.push_back(arrival),
        }
    }
    if homeless.is_empty() {
        return;
    }

    let max = village_config().max_players_per_village;
    let mut villages = collect_villages(&gorc_instances).await;
    let mut load = mesh_load::view();
    let mut last_village: Option<usize> = None;
    let mut placed: Vec<(Arrival, Vec3, String)> = Vec::new();
    let mut unplaced: VecDeque<Arrival> = VecDeque::new();
    while let Some(arrival) = homeless.pop_front() {
        if !player_connected(events, &arrival.player_uuid).await {
            info!("plugin genericprops (new_player): player {} left before getting an apartment", arrival.player_uuid);
            continue;
        }
        let selected = select_village(&villages, &AssignRequest::default(), max, load.as_ref())
            .and_then(|chosen| villages.iter().position(|v| v.uuid == chosen.uuid));
        let Some(index) = selected else {
            // No room under the cap anywhere: none for the next ones either.
            unplaced.push_back(arrival);
            unplaced.extend(homeless.drain(..));
            break;
        };
        let mut home = None;
        for gorc_id in villages[index].buildings.clone() {
            match try_assign_in_building(&gorc_instances, events, gorc_id, &arrival.player_uuid, &arrival.player_name).await {
                Ok(Some(found)) => {
                    home = Some(found);
                    break;
                }
                Ok(None) => {}
                Err(e) => error!("plugin genericprops (new_player): apartment in building {} for player {}: {}", gorc_id, arrival.player_uuid, e),
            }
        }
        let village = &mut villages[index];
        match home {
            Some((position, uuid)) => {
                info!(
                    "plugin genericprops (new_player): player {} gets an apartment in village {} ({}/{} players) on {}",
                    arrival.player_uuid, village.name, village.occupancy + 1, max, owner_label(village, load.as_ref())
                );
                if let Some(server) = load.as_ref().zip(village.world.as_ref()).and_then(|(l, w)| l.owner_of(w)) {
                    mesh_load::note_placed(&server.uuid);
                    // The next player of this pass sees it, as the next waiter used to.
                    load = mesh_load::view();
                }
                village.occupancy += 1;
                village.free = village.free.saturating_sub(1);
                last_village = Some(index);
                placed.push((arrival, position, uuid));
            }
            None => {
                // Its buildings are full after all: `free` came from a stale `available`.
                village.free = 0;
                homeless.push_front(arrival);
            }
        }
    }

    // Keep the reserve of places ahead of the next arrivals, counting the apartments just given.
    keep_reserve(&gorc_instances, events, &villages, last_village.map(|i| &villages[i]), load.as_ref()).await;

    if !unplaced.is_empty() {
        // Habs requested and not arrived yet (Godot still loading their planet, spawning
        // them): wait for them rather than spawning the players at the origin.
        let habs_coming = collect_villages(&gorc_instances).await.iter().any(VillageState::is_spawn_pending);
        for mut arrival in unplaced {
            if !habs_coming {
                warn!(
                    "plugin genericprops (new_player): every village is at its cap of {} players, player {} gets no apartment (raise village_free_reserve?)",
                    max, arrival.player_uuid
                );
                warn!("plugin genericprops (new_player): no free spawnbuilding slot found for player {}, spawning at origin", arrival.player_uuid);
                spawn_player(events, &arrival, Vec3::zero(), "", false).await;
            } else if arrival.since.elapsed() >= HABS_WAIT {
                warn!(
                    "plugin genericprops (new_player): the village habs did not arrive within {:?}, player {} disconnected (server not ready)",
                    HABS_WAIT, arrival.player_uuid
                );
                if send_not_ready(events, &arrival.player_uuid).await {
                    kicks.push((Instant::now() + KICK_DELAY, arrival.player_uuid));
                }
            } else if !player_connected(events, &arrival.player_uuid).await {
                info!("plugin genericprops (new_player): player {} left while waiting for the village habs", arrival.player_uuid);
            } else {
                if !arrival.waiting_logged {
                    info!(
                        "plugin genericprops (new_player): player {} waits for the requested village habs (up to {:?})",
                        arrival.player_uuid, HABS_WAIT
                    );
                    arrival.waiting_logged = true;
                }
                waiting.push_back(arrival);
            }
        }
    }

    for (arrival, position, building_uuid) in placed {
        spawn_player(events, &arrival, position, &building_uuid, true).await;
    }
}

/// Request habs so the reserve of places stays ahead of the arrivals, around `last` (the
/// village players were just sent to), else the last full one, else the first village.
async fn keep_reserve(
    gorc_instances: &GorcInstanceManager,
    events: &EventSystem,
    villages: &[VillageState],
    last: Option<&VillageState>,
    load: Option<&LoadView>,
) {
    if let Some(reference) = last.or_else(|| last_full_village(villages)) {
        request_villages(gorc_instances, events, villages, reference, load).await;
        return;
    }
    // No habs anywhere yet: start with the first village, in case its startup request was
    // lost (startup items imported before it existed).
    let Some(first) = first_village(villages) else {
        warn!("plugin genericprops (new_player): no poi_village to spawn players in");
        return;
    };
    let mut after = villages.to_vec();
    if !first.is_spawn_pending_or_done() {
        info!(
            "plugin genericprops (new_player): no village has habs yet, requesting the first one {} ({})",
            first.name, first.uuid
        );
        request_village(gorc_instances, events, first).await;
        if let Some(v) = after.iter_mut().find(|v| v.uuid == first.uuid) {
            v.spawn_requested = true;
        }
    }
    request_villages(gorc_instances, events, &after, first, load).await;
}

/// The apartment each of `players` was given in an earlier session: slot, position local to
/// the building and building uuid, by player uuid. One scan of the buildings per pass.
async fn existing_apartments(
    gorc_instances: &GorcInstanceManager,
    players: &HashSet<String>,
) -> HashMap<String, ((i64, i64, i64), Vec3, String)> {
    let mut homes = HashMap::new();
    for gorc_id in gorc_instances.get_objects_by_type("spawnbuilding").await {
        let found = gorc_instances
            .with_object_mut(gorc_id, |instance| {
                let building = instance.get_object::<GenericProps>()?;
                let found: Vec<_> = apartments(building)
                    .iter()
                    .filter_map(|entry| {
                        let uuid = entry.get("player_uuid").and_then(|v| v.as_str()).filter(|u| players.contains(*u))?;
                        let slot = apartment_slot(entry)?;
                        Some((uuid.to_string(), (slot, slot_position(building, slot), building.uuid.clone())))
                    })
                    .collect();
                Some(found)
            })
            .await
            .flatten()
            .unwrap_or_default();
        for (uuid, home) in found {
            homes.entry(uuid).or_insert(home);
        }
        if homes.len() == players.len() {
            break;
        }
    }
    homes
}

/// Create the player object in GORC, in its apartment (`broadcast`: a new one, whose
/// building state goes to the nearby clients first).
async fn spawn_player(events: &EventSystem, arrival: &Arrival, spawn_position: Vec3, building_uuid: &str, broadcast: bool) {
    if broadcast {
        // Broadcast the current (authoritative) building state to nearby clients.
        // broadcast_only=true tells handle_object_update to read the fresh GORC
        // state and send it to clients WITHOUT calling update_object again —
        // that would overwrite our committed data with a stale snapshot.
        if let Err(e) = events.emit_plugin(
            "genericprops",
            "update_object_from_external",
            &serde_json::json!({
                "object_type": "spawnbuilding",
                "object_uuid": building_uuid,
                "object_data": {},
                "broadcast_only": true,
            }),
        ).await {
            error!("plugin genericprops (new_player): failed to broadcast building update: {}", e);
        }
    }
    if let Err(e) = events.emit_plugin(
        "genericprops",
        "create_object",
        &serde_json::json!({
            "object_type": "player",
            "object_uuid": arrival.player_uuid,
            "object_data": {
                "name": arrival.player_name,
                "position": { "x": spawn_position.x, "y": spawn_position.y, "z": spawn_position.z },
                "rotation": { "x": 0.0, "y": 1.708, "z": 0.0 },
                "parent_id": building_uuid,
                "spawn_appartment_id": building_uuid,
                "is_npc": arrival.is_npc,
            }
        }),
    ).await {
        error!("plugin genericprops (new_player): failed to emit create_object: {}", e);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn village(name: &str, occupancy: usize, free: usize, x: f64) -> VillageState {
        VillageState {
            gorc_id: None,
            uuid: format!("uuid-{name}"),
            name: name.to_string(),
            position: Vec3::new(x, 0.0, 0.0),
            world: None,
            is_spawned: free > 0 || occupancy > 0,
            spawn_requested: false,
            occupancy,
            free,
            buildings: if free > 0 || occupancy > 0 { vec![GorcObjectId::new()] } else { Vec::new() },
        }
    }

    #[test]
    fn only_mining_villages_house_players() {
        assert!(houses_players(Some("mining"), None));
        assert!(!houses_players(Some("factory"), None), "a factory city has no habs");
        assert!(houses_players(None, None), "a village from before the type field still counts");
        let mining = Some("scenes/_universe/structures/urban/villages/ares_village_mining.tscn");
        let factory = Some("scenes/_universe/structures/urban/cities/ares_city_factory.tscn");
        assert!(houses_players(Some("poi_village"), mining), "type overwritten by an update: the layout tells");
        assert!(!houses_players(Some("poi_village"), factory));
    }

    #[test]
    fn first_village_is_m0001_then_first_by_name() {
        let villages = [village("M0002", 0, 0, 0.0), village("M0001", 0, 0, 10.0)];
        assert_eq!(first_village(&villages).unwrap().name, "M0001");
        let villages = [village("M0003", 0, 0, 0.0), village("M0002", 0, 0, 10.0)];
        assert_eq!(first_village(&villages).unwrap().name, "M0002");
        assert!(first_village(&[]).is_none());
    }

    #[test]
    fn starts_with_first_village_by_name() {
        let villages = [village("mining_village_02", 0, 100, 10.0), village("mining_village_01", 0, 100, 0.0)];
        let chosen = select_village(&villages, &AssignRequest::default(), 50, None).unwrap();
        assert_eq!(chosen.name, "mining_village_01");
    }

    #[test]
    fn fills_fullest_village_under_cap_first() {
        let villages = [village("mining_village_01", 10, 100, 0.0), village("mining_village_02", 30, 100, 10.0)];
        let chosen = select_village(&villages, &AssignRequest::default(), 50, None).unwrap();
        assert_eq!(chosen.name, "mining_village_02");
    }

    #[test]
    fn solo_player_never_goes_past_cap() {
        // Plenty of physical room, but the village holds its 50 players.
        let villages = [village("mining_village_01", 50, 400, 0.0)];
        assert!(select_village(&villages, &AssignRequest::default(), 50, None).is_none());
    }

    #[test]
    fn preferred_village_over_cap_within_physical_room() {
        let villages = [village("mining_village_01", 50, 1, 0.0), village("mining_village_02", 0, 100, 10.0)];
        let request = AssignRequest { preferred_village: Some("uuid-mining_village_01".into()), allow_over_cap: true };
        assert_eq!(select_village(&villages, &request, 50, None).unwrap().name, "mining_village_01");

        let full = [village("mining_village_01", 51, 0, 0.0), village("mining_village_02", 0, 100, 10.0)];
        assert_eq!(select_village(&full, &request, 50, None).unwrap().name, "mining_village_02");
    }

    #[test]
    fn requests_closest_unspawned_villages_until_the_reserve_is_met() {
        let villages = [
            village("mining_village_01", 45, 10, 0.0),
            village("mining_village_02", 0, 0, 5.0),
            village("mining_village_03", 0, 0, 20.0),
            village("mining_village_04", 0, 0, 8.0),
        ];
        let reference = last_full_village(&villages).unwrap();
        assert_eq!(reference.name, "mining_village_01");
        assert_eq!(free_reserve(&villages, 50, None), 5);
        // 5 free places, 100 wanted: two villages at once, closest first.
        let names: Vec<&str> = villages_to_request(&villages, reference, 50, 100, None).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_02", "mining_village_04"]);
        // Reserve already met: nothing.
        assert!(villages_to_request(&villages, reference, 50, 5, None).is_empty());
    }

    #[test]
    fn requested_village_counts_as_a_full_cap_until_its_habs_arrive() {
        let mut requested = village("mining_village_02", 0, 0, 5.0);
        requested.spawn_requested = true;
        let villages = [
            village("mining_village_01", 45, 10, 0.0),
            requested,
            village("mining_village_03", 0, 0, 20.0),
            village("mining_village_04", 0, 0, 8.0),
        ];
        assert_eq!(free_reserve(&villages, 50, None), 55);
        let reference = &villages[0];
        // 55 < 100: one more, never the one already requested.
        let names: Vec<&str> = villages_to_request(&villages, reference, 50, 100, None).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_04"]);
        assert!(villages_to_request(&villages, reference, 50, 50, None).is_empty());
    }

    // ---- placement by pool load (servers_load)

    use ds_common::mesh_load::{PoolLoad, ServerLoad};
    use ds_common::world::{Point3, WorldKind};
    use ds_common::zone::{Bounds, Zone};

    /// SandBox cut at x = 100: `west` owns x < 100, `east` the rest.
    fn sandbox_load(west_players: u32, west_accepting: bool, east_players: u32) -> PoolLoad {
        let sandbox = Zone::planet("sandbox", "SandBox");
        let half = |min_x: f64, max_x: f64| sandbox.with_new_bounds(Bounds {
            min_x, max_x, min_y: -Bounds::UNBOUNDED, max_y: Bounds::UNBOUNDED, min_z: -Bounds::UNBOUNDED, max_z: Bounds::UNBOUNDED,
        });
        let server = |name: &str, players, accepting, zone: Zone| ServerLoad {
            uuid: format!("uuid-{name}"), name: name.into(), players, accepting, loading: false, responsive: true, zones: vec![zone],
        };
        PoolLoad {
            servers: vec![
                server("west", west_players, west_accepting, half(-Bounds::UNBOUNDED, 100.0)),
                server("east", east_players, true, half(100.0, Bounds::UNBOUNDED)),
            ],
            capacity: Some(75),
        }
    }

    fn on_sandbox(mut v: VillageState) -> VillageState {
        v.world = Some(ObjectWorld {
            chain: Vec::new(),
            world: WorldKind::Planet,
            planet_uuid: Some("sandbox".into()),
            planet_name: Some("SandBox".into()),
            local_position: Point3::new(v.position.x, 0.0, 0.0),
        });
        v
    }

    #[test]
    fn places_in_the_least_loaded_server_zone() {
        let villages = [
            on_sandbox(village("mining_village_01", 40, 10, 0.0)),   // west, fullest
            on_sandbox(village("mining_village_09", 5, 45, 200.0)),  // east
        ];
        let load = mesh_load::view_of(sandbox_load(60, true, 10));
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");
        // Without the load: the fullest village, as before.
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, None).unwrap().name, "mining_village_01");
    }

    #[test]
    fn a_refusing_server_gets_players_only_when_no_other_can() {
        let villages = [
            on_sandbox(village("mining_village_01", 10, 40, 0.0)),   // west: silent / loading
            on_sandbox(village("mining_village_09", 45, 5, 200.0)),  // east: busier but reporting
        ];
        let load = mesh_load::view_of(sandbox_load(10, false, 70));
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");

        // East full under the cap: the refusing one is better than no apartment.
        let villages = [on_sandbox(village("mining_village_01", 10, 40, 0.0)), on_sandbox(village("mining_village_09", 50, 5, 200.0))];
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_01");
    }

    #[test]
    fn a_server_at_capacity_counts_as_refusing() {
        let villages = [
            on_sandbox(village("mining_village_01", 10, 40, 0.0)),
            on_sandbox(village("mining_village_09", 10, 40, 200.0)),
        ];
        // East has fewer players but is at the split rule (75): west takes them.
        let load = mesh_load::view_of(sandbox_load(74, true, 75));
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_01");
    }

    #[test]
    fn reserve_is_kept_in_the_zones_that_take_players() {
        let villages = [
            on_sandbox(village("mining_village_01", 0, 100, 0.0)),    // west: refusing, 50 places
            on_sandbox(village("mining_village_02", 0, 0, 10.0)),     // west, unspawned, closest
            on_sandbox(village("mining_village_09", 0, 0, 300.0)),    // east, unspawned
            on_sandbox(village("mining_village_10", 0, 0, 200.0)),    // east, unspawned
        ];
        let load = mesh_load::view_of(sandbox_load(70, false, 5));
        // West's 50 free places do not count: nobody will be sent there.
        assert_eq!(free_reserve(&villages, 50, Some(&load)), 0);
        assert_eq!(free_reserve(&villages, 50, None), 50);
        let reference = &villages[0];
        let names: Vec<&str> = villages_to_request(&villages, reference, 50, 100, Some(&load)).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_10", "mining_village_09"]);
    }

    #[test]
    fn an_accepting_server_without_villages_does_not_empty_the_reserve() {
        // Space (no village) takes players, SandBox's owner refuses: the reserve is
        // counted everywhere, as without the load.
        let mut load = sandbox_load(70, false, 70);
        load.servers[1].accepting = false;
        load.servers.push(ServerLoad {
            uuid: "uuid-space".into(), name: "space".into(), players: 0, accepting: true, loading: false, responsive: true, zones: vec![Zone::space()],
        });
        let load = mesh_load::view_of(load);
        let villages = [on_sandbox(village("mining_village_01", 0, 100, 0.0)), on_sandbox(village("mining_village_02", 0, 0, 10.0))];
        assert_eq!(free_reserve(&villages, 50, Some(&load)), 50);
        assert_eq!(villages_to_request(&villages, &villages[0], 50, 50, Some(&load)).len(), 0);
    }

    /// `sandbox_load` with west silent / frozen (or only loading its new zones).
    fn sandbox_load_west(west_players: u32, west_loading: bool, west_responsive: bool, east_players: u32) -> LoadView {
        let mut load = sandbox_load(west_players, false, east_players);
        load.servers[0].loading = west_loading;
        load.servers[0].responsive = west_responsive;
        mesh_load::view_of(load)
    }

    #[test]
    fn a_frozen_server_never_gets_players_even_as_last_resort() {
        // East full under the cap, west has room but is frozen: no apartment rather than
        // one on a server that will not simulate the player (preprod, 2026-10-16).
        let villages = [on_sandbox(village("mining_village_01", 10, 40, 0.0)), on_sandbox(village("mining_village_09", 50, 5, 200.0))];
        let frozen = sandbox_load_west(10, false, false, 70);
        assert!(select_village(&villages, &AssignRequest::default(), 50, Some(&frozen)).is_none());
        // Only loading: still the last resort, as before.
        let loading = sandbox_load_west(10, true, true, 70);
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&loading)).unwrap().name, "mining_village_01");
    }

    #[test]
    fn habs_pending_on_a_frozen_server_do_not_count_in_the_reserve() {
        let mut requested = on_sandbox(village("mining_village_02", 0, 0, 10.0));
        requested.spawn_requested = true;
        let villages = [
            on_sandbox(village("mining_village_01", 50, 0, 0.0)),    // west, full
            requested,                                                // west, habs pending
            on_sandbox(village("mining_village_09", 0, 0, 300.0)),   // east, unspawned
        ];
        let frozen = sandbox_load_west(50, false, false, 70);
        assert_eq!(free_reserve(&villages, 50, Some(&frozen)), 0);
        // So the reserve is requested on east instead.
        let names: Vec<&str> = villages_to_request(&villages, &villages[0], 50, 50, Some(&frozen)).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_09"]);
    }

    #[test]
    fn no_habs_requested_on_a_frozen_or_loading_server() {
        let villages = [
            on_sandbox(village("mining_village_01", 50, 0, 0.0)),    // west, full
            on_sandbox(village("mining_village_02", 0, 0, 10.0)),    // west, unspawned
            on_sandbox(village("mining_village_03", 0, 0, 20.0)),    // west, unspawned
        ];
        for load in [sandbox_load_west(50, false, false, 0), sandbox_load_west(50, true, true, 0)] {
            assert!(villages_to_request(&villages, &villages[0], 50, 100, Some(&load)).is_empty());
        }
        // Reporting again, done loading: requested.
        let back = sandbox_load_west(50, false, true, 0);
        assert_eq!(villages_to_request(&villages, &villages[0], 50, 100, Some(&back)).len(), 2);
    }

    #[test]
    fn at_most_two_villages_waiting_per_server() {
        let mut requested = on_sandbox(village("mining_village_05", 0, 0, 300.0));
        requested.spawn_requested = true;
        let villages = [
            on_sandbox(village("mining_village_01", 50, 0, 0.0)),  // west, full
            on_sandbox(village("mining_village_02", 0, 0, 10.0)),
            on_sandbox(village("mining_village_03", 0, 0, 20.0)),
            on_sandbox(village("mining_village_04", 0, 0, 30.0)),
            requested,                                              // east, already waiting
            on_sandbox(village("mining_village_06", 0, 0, 310.0)),
            on_sandbox(village("mining_village_07", 0, 0, 320.0)),
        ];
        let load = mesh_load::view_of(sandbox_load(50, false, 0));
        let names: Vec<&str> = villages_to_request(&villages, &villages[0], 50, 1000, Some(&load)).iter().map(|v| v.name.as_str()).collect();
        // East (accepting) first, one more there (2 waiting), then two on west.
        assert_eq!(names, ["mining_village_06", "mining_village_02", "mining_village_03"]);
    }
}

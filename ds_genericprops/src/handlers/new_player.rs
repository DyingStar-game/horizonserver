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
use ds_common::mesh_load::ServerLoad;
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
    /// Players a server is given at most per ADMIT_WINDOW: 49 queued players let in at
    /// once took a server from 61 to 14 tps (minikube, 2026-10-20). Zero: no limit.
    pub admit_per_server: usize,
}

impl Default for VillageConfig {
    fn default() -> Self {
        Self { max_players_per_village: 50, free_reserve: 100, admit_per_server: 10 }
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

/// The village a player gets an apartment in, or None when no village has room under its
/// cap. Solo players fill the fullest village still under the cap first (ties by name), so the
/// villages fill one after the other, starting with mining_village_01. With the pool load
/// known, that happens within the zones of the least loaded server that takes players:
/// filling the villages in line put every arrival on the server owning the next one. The cap
/// is that server's (see `server_cap`): at the same number of players, a village under
/// `max` goes before one above it.
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
    let caps = caps_by_server(villages, max, load);
    let cap_of = |v: &VillageState| caps.get(&owner_key(v, load)).copied().unwrap_or(max);
    // Never on a frozen server, even as the last resort: better wait for habs elsewhere.
    let with_room = villages
        .iter()
        .filter(|v| !load.is_some_and(|l| l.owner_unresponsive(v.world.as_ref())))
        .filter(|v| v.has_room_under_cap(cap_of(v)));
    match load {
        Some(load) => with_room.min_by(|a, b| {
            load.rank(a.world.as_ref())
                .cmp(&load.rank(b.world.as_ref()))
                .then_with(|| (cap_of(a) > max).cmp(&(cap_of(b) > max)))
                .then_with(|| b.occupancy.cmp(&a.occupancy))
                .then_with(|| a.name.cmp(&b.name))
        }),
        None => with_room.max_by(|a, b| a.occupancy.cmp(&b.occupancy).then_with(|| b.name.cmp(&a.name))),
    }
}

/// How much a server's cap of solo players per village rises once its villages hold it.
const CAP_STEP: usize = 10;

/// The server owning the village's zone, which its cap is computed per (`None`: no load
/// to tell, every village counts as one server).
fn owner_key(village: &VillageState, load: Option<&LoadView>) -> Option<String> {
    load.and_then(|l| village.world.as_ref().and_then(|w| l.owner_of(w)).map(|s| s.uuid.clone()))
}

/// The cap of solo players per village on one server, from its villages: `base`
/// (max_players_per_village) while one of them has room below it or still waits for its habs,
/// else the first `base + k * CAP_STEP` one of them has room below. Per server, so the players
/// per server stay even: with every village at 50, a server holding 2 villages (100 players)
/// goes to 60 while one holding 3 (150) stays at 50, the least loaded server taking the
/// arrivals (see select_village). A single cap for the world sent the next 100 players to
/// the server that already had the most; and before any cap rose, a full world spawned the
/// arrivals at the origin with apartments still free (preprod NPC tests, 2026-10-16).
fn server_cap(villages: &[&VillageState], base: usize) -> usize {
    if base == 0 || villages.iter().any(|v| v.is_spawn_pending()) || !villages.iter().any(|v| v.free > 0) {
        return base;
    }
    let mut cap = base;
    while !villages.iter().any(|v| v.has_room_under_cap(cap)) {
        cap += CAP_STEP;
    }
    cap
}

/// `server_cap` of every server owning villages. A frozen server takes no players: left out.
fn caps_by_server(villages: &[VillageState], base: usize, load: Option<&LoadView>) -> HashMap<Option<String>, usize> {
    let mut by_server: HashMap<Option<String>, Vec<&VillageState>> = HashMap::new();
    for village in villages.iter().filter(|v| !load.is_some_and(|l| l.owner_unresponsive(v.world.as_ref()))) {
        by_server.entry(owner_key(village, load)).or_default().push(village);
    }
    by_server.into_iter().map(|(server, villages)| (server, server_cap(&villages, base))).collect()
}

/// The cap `village` was just given an apartment under.
fn village_cap(villages: &[VillageState], village: &VillageState, base: usize, load: Option<&LoadView>) -> usize {
    caps_by_server(villages, base, load).get(&owner_key(village, load)).copied().unwrap_or(base)
}

/// Log a server's cap when it changes.
fn note_cap(village: &VillageState, cap: usize, base: usize, load: Option<&LoadView>) {
    static LAST: std::sync::Mutex<Option<HashMap<Option<String>, usize>>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    let previous = last.get_or_insert_with(HashMap::new).insert(owner_key(village, load), cap).unwrap_or(base);
    if previous != cap {
        info!(
            "plugin genericprops (new_player): villages of {} now take up to {} players (was {})",
            owner_label(village, load), cap, previous
        );
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
/// The window of `admit_per_server`: as long as the retry of the waiting players, so the
/// next pass finds the window empty again.
const ADMIT_WINDOW: Duration = HABS_RETRY;
/// A waiting player whose place did not move is told it is still queued this often: each
/// `queued` message gives the client's SPAWN_TIMEOUT_MS (45 s, client.gd) a fresh start.
const QUEUED_NOTIFY: Duration = Duration::from_secs(10);
/// A waiting player is told its new place as soon as it moves, but not more often than
/// this: only every 10 s, a player went from 40 to 30 and then straight to spawning.
const QUEUED_MIN_GAP: Duration = Duration::from_secs(1);

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

/// Tell the waiting player it is queued, `position` (from 1) among the waiting players.
async fn send_queued(events: &EventSystem, player_uuid: &str, wait: Wait, position: usize) {
    let (Some(sender), Some(player_id)) = (events.get_client_response_sender(), player_id(player_uuid)) else {
        return;
    };
    let payload = serde_json::json!({ "type": "queued", "reason": wait.reason(), "position": position });
    if let Err(e) = sender.send_to_client(player_id, payload.to_string().into_bytes()).await {
        debug!("plugin genericprops (new_player): failed to send queued to player {}: {}", player_uuid, e);
    }
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

/// Why a player is still in the placement queue, for the client and the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// No village has room: the requested habs have not arrived yet.
    Habs,
    /// Its village's server takes no new player now (full, under placement_min_tps, in a
    /// split or merge, loading, frozen).
    ServerFull,
}

impl Wait {
    fn reason(self) -> &'static str {
        match self {
            Wait::Habs => "habs",
            Wait::ServerFull => "server_full",
        }
    }
}

/// The apartment a returning player already has, waiting for its server to take it.
#[derive(Debug, Clone)]
struct Home {
    /// Local to the building.
    position: Vec3,
    building_uuid: String,
    world: ObjectWorld,
}

/// A player waiting for an apartment, or for the server owning it, on the placement thread.
struct Arrival {
    player_uuid: String,
    player_name: String,
    is_npc: bool,
    since: Instant,
    /// Looked for an apartment given in an earlier session already.
    home_checked: bool,
    /// `waits for the requested village habs` logged already.
    waiting_logged: bool,
    /// A returning player: its `object_data` from persistence (everything it carries
    /// survives), spawned in its apartment instead of where it was last seen.
    stored: Option<serde_json::Value>,
    home: Option<Home>,
    wait: Option<Wait>,
    notified_at: Option<Instant>,
    /// The place it was last told, from 1.
    notified_place: Option<usize>,
}

impl Arrival {
    fn new(event: &serde_json::Value, stored: Option<serde_json::Value>) -> Arrival {
        Arrival {
            player_uuid: event["object_uuid"].as_str().unwrap_or_default().to_string(),
            player_name: event["object_data"]["name"].as_str().unwrap_or_default().to_string(),
            is_npc: event["object_data"]["is_npc"].as_bool().unwrap_or(false),
            since: Instant::now(),
            home_checked: false,
            waiting_logged: false,
            stored,
            home: None,
            wait: None,
            notified_at: None,
            notified_place: None,
        }
    }

    /// Queued for `wait`: logged once per reason.
    fn queue(&mut self, wait: Wait, why: impl FnOnce() -> String) {
        if self.wait != Some(wait) && wait == Wait::ServerFull {
            info!("plugin genericprops (new_player): player {} queued: {}", self.player_uuid, why());
        }
        self.wait = Some(wait);
    }

    /// Leaves the queue to be spawned.
    fn log_admitted(&self) {
        if self.wait == Some(Wait::ServerFull) {
            info!("plugin genericprops (new_player): player {} leaves the queue after {:?}", self.player_uuid, self.since.elapsed());
        }
    }
}

enum Placement {
    Arrival(Arrival),
    /// Spawnbuildings were registered: the waiting players may get an apartment now.
    HabsArrived,
    /// A new pool load: a server may take players again.
    LoadUpdated,
}

static PLACEMENT: OnceLock<Sender<Placement>> = OnceLock::new();

/// Players the placement thread let through to `create_object`: their next one is created,
/// any other is queued (see `enqueue_returning`).
fn admitted() -> &'static std::sync::Mutex<HashSet<String>> {
    static ADMITTED: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    ADMITTED.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// True once per admission: the `create_object` of this player may create it.
pub fn take_admitted(player_uuid: &str) -> bool {
    admitted().lock().unwrap().remove(player_uuid)
}

/// Whether a player may be spawned in `world` now: its server takes players. However long
/// the player waited: a server that refuses is never given a player (forced in after 120 s,
/// returning NPCs held one server at 238 players and 20 tps, preprod 2026-10-20); the split
/// or the tps coming back lets the queue go. No load known, or no server owning the place:
/// nothing to protect.
fn admits(load: Option<&LoadView>, world: Option<&ObjectWorld>) -> bool {
    refusing_owner(load, world).is_none()
}

/// The server owning `world` when it takes no new player now.
fn refusing_owner<'a>(load: Option<&'a LoadView>, world: Option<&ObjectWorld>) -> Option<&'a ServerLoad> {
    let load = load?;
    let server = world.and_then(|w| load.owner_of(w))?;
    (!load.accepts(server)).then_some(server)
}

/// The players given to each server over the last ADMIT_WINDOW, against `admit_per_server`.
#[derive(Debug, Default)]
struct Throttle {
    limit: usize,
    by_server: HashMap<String, VecDeque<Instant>>,
}

impl Throttle {
    fn prune(&mut self, now: Instant) {
        for admitted in self.by_server.values_mut() {
            while admitted.front().is_some_and(|at| now.duration_since(*at) >= ADMIT_WINDOW) {
                admitted.pop_front();
            }
        }
        self.by_server.retain(|_, admitted| !admitted.is_empty());
    }

    fn allows(&self, server_uuid: &str) -> bool {
        self.limit == 0 || self.by_server.get(server_uuid).map_or(0, VecDeque::len) < self.limit
    }

    fn record(&mut self, server_uuid: &str, now: Instant) {
        if self.limit > 0 {
            self.by_server.entry(server_uuid.to_string()).or_default().push_back(now);
        }
    }

    /// The pool load, the servers that had their share of arrivals refusing in it: the
    /// placement prefers the others, the queue holds the rest.
    fn view(&self) -> Option<LoadView> {
        mesh_load::view().map(|mut load| {
            for server in self.by_server.keys().filter(|s| !self.allows(s)) {
                load.refuse(server);
            }
            load
        })
    }
}

/// Whether a waiting player at `place` is told so now: never told yet, its place moved
/// (at most every QUEUED_MIN_GAP), or still there for QUEUED_NOTIFY.
fn queued_notice_due(notified_at: Option<Instant>, notified_place: Option<usize>, place: usize, now: Instant) -> bool {
    let Some(at) = notified_at else { return true };
    let since = now.duration_since(at);
    since >= QUEUED_NOTIFY || (notified_place != Some(place) && since >= QUEUED_MIN_GAP)
}

/// Why a server takes no player now, for the queue log.
fn refusal(server: String, throttled: bool) -> String {
    if throttled {
        format!("{} had its share of arrivals for now (queue_admit_per_server)", server)
    } else {
        format!("{} takes no player now", server)
    }
}

/// `name (players, tps)` of a server, for the logs.
fn server_label(load: &LoadView, server: &ServerLoad) -> String {
    format!("{} ({} players, {} tps)", server.name, load.players(server), server.tps)
}

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
    let arrival = Arrival::new(event, None);
    info!(
        "plugin genericprops (new_player): received new_player event for player_uuid={} player_name={}",
        arrival.player_uuid, arrival.player_name
    );
    send_arrival(arrival);
}

/// The `create_object` of a player persistence knows (back from an earlier session): it
/// goes through the queue too, and spawns in its apartment.
pub fn enqueue_returning(event: &serde_json::Value) {
    let arrival = Arrival::new(event, Some(event["object_data"].clone()));
    info!(
        "plugin genericprops (new_player): returning player player_uuid={} player_name={} queued for its apartment",
        arrival.player_uuid, arrival.player_name
    );
    send_arrival(arrival);
}

fn send_arrival(arrival: Arrival) {
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

/// A new `servers_load`: the players waiting for their server look again.
pub fn load_updated() {
    if let Some(tx) = PLACEMENT.get() {
        let _ = tx.send(Placement::LoadUpdated);
    }
}

fn run(events: Arc<EventSystem>, rx: Receiver<Placement>) {
    let mut waiting: VecDeque<Arrival> = VecDeque::new();
    let mut kicks: Vec<(Instant, String)> = Vec::new();
    let mut retry_at: Option<Instant> = None;
    let mut throttle = Throttle::default();
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
                        // Reconnected while still queued: the new session replaces the old.
                        waiting.retain(|a| a.player_uuid != arrival.player_uuid);
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
            block_on(place_waiting(&events, &mut waiting, &mut kicks, &mut throttle));
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
/// habs are coming at all. A player whose village is on a server taking no player now
/// (split under way, tps too low, full) waits for it, however long it takes.
async fn place_waiting(
    events: &Arc<EventSystem>,
    waiting: &mut VecDeque<Arrival>,
    kicks: &mut Vec<(Instant, String)>,
    throttle: &mut Throttle,
) {
    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available, {} new player(s) dropped", waiting.len());
        waiting.clear();
        return;
    };
    let config = village_config();
    throttle.limit = config.admit_per_server;
    throttle.prune(Instant::now());
    let mut load = throttle.view();
    // Waiting for their server, in arrival order with the others at the end of the pass.
    let mut queued: Vec<Arrival> = Vec::new();

    // Back from an earlier session: the apartment they already have.
    let unchecked: HashSet<String> = waiting.iter().filter(|a| !a.home_checked).map(|a| a.player_uuid.clone()).collect();
    let homes = if unchecked.is_empty() { HashMap::new() } else { existing_apartments(&gorc_instances, &unchecked).await };
    let mut homeless: VecDeque<Arrival> = VecDeque::new();
    for mut arrival in waiting.drain(..) {
        if !arrival.home_checked {
            arrival.home_checked = true;
            if let Some((slot, position, uuid)) = homes.get(&arrival.player_uuid) {
                info!(
                    "plugin genericprops (new_player): player {} already has apartment {:?} in building {} → position {:?}",
                    arrival.player_uuid, slot, uuid, position
                );
                let world = resolve_world(&gorc_instances, *position, Some(uuid.clone())).await;
                arrival.home = Some(Home { position: *position, building_uuid: uuid.clone(), world });
            }
        }
        let Some(home) = arrival.home.clone() else {
            homeless.push_back(arrival);
            continue;
        };
        if arrival.wait.is_some() && !player_connected(events, &arrival.player_uuid).await {
            info!("plugin genericprops (new_player): player {} left while queued", arrival.player_uuid);
            continue;
        }
        let owner = load.as_ref().and_then(|l| l.owner_of(&home.world)).map(|s| s.uuid.clone());
        let throttled = owner.as_ref().is_some_and(|uuid| !throttle.allows(uuid));
        if throttled || !admits(load.as_ref(), Some(&home.world)) {
            let why = refusing_owner(load.as_ref(), Some(&home.world)).zip(load.as_ref()).map(|(s, l)| server_label(l, s));
            arrival.queue(Wait::ServerFull, || refusal(why.unwrap_or_default(), throttled));
            queued.push(arrival);
            continue;
        }
        if let Some(uuid) = owner {
            mesh_load::note_placed(&uuid);
            throttle.record(&uuid, Instant::now());
            load = throttle.view();
        }
        arrival.log_admitted();
        spawn_player(events, &arrival, home.position, &home.building_uuid, false).await;
    }

    let max = village_config().max_players_per_village;
    let mut villages = if homeless.is_empty() { Vec::new() } else { collect_villages(&gorc_instances).await };
    let mut last_village: Option<usize> = None;
    let mut placed: Vec<(Arrival, Vec3, String)> = Vec::new();
    let mut unplaced: VecDeque<Arrival> = VecDeque::new();
    while let Some(mut arrival) = homeless.pop_front() {
        if !player_connected(events, &arrival.player_uuid).await {
            info!("plugin genericprops (new_player): player {} left before getting an apartment", arrival.player_uuid);
            continue;
        }
        let selected = select_village(&villages, &AssignRequest::default(), max, load.as_ref())
            .and_then(|chosen| villages.iter().position(|v| v.uuid == chosen.uuid));
        let Some(index) = selected else {
            // No room under the caps anywhere: none for the next ones either.
            unplaced.push_back(arrival);
            unplaced.extend(homeless.drain(..));
            break;
        };
        // The best village is on a server that takes no player now (the accepting ones
        // come first): wait for it rather than overload it. Its apartment is given only
        // then, so a village or a server freed meanwhile takes the player instead.
        let owner = load.as_ref().zip(villages[index].world.as_ref()).and_then(|(l, w)| l.owner_of(w)).map(|s| s.uuid.clone());
        let throttled = owner.as_ref().is_some_and(|uuid| !throttle.allows(uuid));
        if let Some(server) = refusing_owner(load.as_ref(), villages[index].world.as_ref()) {
            let label = server_label(load.as_ref().unwrap(), server);
            arrival.queue(Wait::ServerFull, || refusal(label, throttled));
            queued.push(arrival);
            continue;
        }
        let cap = village_cap(&villages, &villages[index], max, load.as_ref());
        note_cap(&villages[index], cap, max, load.as_ref());
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
                    arrival.player_uuid, village.name, village.occupancy + 1, cap, owner_label(village, load.as_ref())
                );
                if let Some(uuid) = &owner {
                    mesh_load::note_placed(uuid);
                    throttle.record(uuid, Instant::now());
                    // The next player of this pass sees it, as the next waiter used to.
                    load = throttle.view();
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
    if !villages.is_empty() {
        keep_reserve(&gorc_instances, events, &villages, last_village.map(|i| &villages[i]), load.as_ref()).await;
    }

    if !unplaced.is_empty() {
        // Habs requested and not arrived yet (Godot still loading their planet, spawning
        // them): wait for them rather than spawning the players at the origin.
        let habs_coming = collect_villages(&gorc_instances).await.iter().any(VillageState::is_spawn_pending);
        for mut arrival in unplaced {
            if !habs_coming {
                warn!(
                    "plugin genericprops (new_player): no village has an apartment left, player {} gets no apartment (raise village_free_reserve?)",
                    arrival.player_uuid
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
                arrival.queue(Wait::Habs, String::new);
                waiting.push_back(arrival);
            }
        }
    }

    for (arrival, position, building_uuid) in placed {
        arrival.log_admitted();
        spawn_player(events, &arrival, position, &building_uuid, true).await;
    }

    // Everyone still waiting, first arrived first: told where they stand.
    waiting.extend(queued);
    waiting.make_contiguous().sort_by_key(|a| a.since);
    let now = Instant::now();
    for (index, arrival) in waiting.iter_mut().enumerate() {
        let Some(wait) = arrival.wait else { continue };
        let place = index + 1;
        if queued_notice_due(arrival.notified_at, arrival.notified_place, place, now) {
            send_queued(events, &arrival.player_uuid, wait, place).await;
            arrival.notified_at = Some(now);
            arrival.notified_place = Some(place);
        }
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
    let object_data = player_object_data(arrival, spawn_position, building_uuid, broadcast);
    admitted().lock().unwrap().insert(arrival.player_uuid.clone());
    if let Err(e) = events.emit_plugin(
        "genericprops",
        "create_object",
        &serde_json::json!({
            "object_type": "player",
            "object_uuid": arrival.player_uuid,
            "object_data": object_data,
        }),
    ).await {
        admitted().lock().unwrap().remove(&arrival.player_uuid);
        error!("plugin genericprops (new_player): failed to emit create_object: {}", e);
    }
}

/// The player's `object_data`, in its apartment. A returning player keeps everything
/// persistence stored but where it was last seen: it wakes up at home. Its apartment is
/// its own and never changes, so its `spawn_appartment_id` stays, unless it had none or
/// was just given one (`new_home`: no apartment of its own was found).
fn player_object_data(arrival: &Arrival, spawn_position: Vec3, building_uuid: &str, new_home: bool) -> serde_json::Value {
    let mut data = arrival
        .stored
        .as_ref()
        .and_then(|stored| stored.as_object())
        .cloned()
        .unwrap_or_default();
    let has_home = data.get("spawn_appartment_id").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());
    data.insert("name".into(), serde_json::json!(arrival.player_name));
    data.insert("position".into(), serde_json::json!({ "x": spawn_position.x, "y": spawn_position.y, "z": spawn_position.z }));
    data.insert("rotation".into(), serde_json::json!({ "x": 0.0, "y": 1.708, "z": 0.0 }));
    data.insert("parent_id".into(), serde_json::json!(building_uuid));
    if new_home || !has_home {
        data.insert("spawn_appartment_id".into(), serde_json::json!(building_uuid));
    }
    data.insert("is_npc".into(), serde_json::json!(arrival.is_npc));
    serde_json::Value::Object(data)
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
    fn a_village_at_the_cap_takes_more_only_with_room() {
        // Every village holds its 50 players: the cap rises while the buildings have room.
        let villages = [village("mining_village_01", 50, 400, 0.0)];
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, None).unwrap().name, "mining_village_01");
        let villages = [village("mining_village_01", 50, 0, 0.0)];
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
            uuid: format!("uuid-{name}"), name: name.into(), players, accepting, tps: 60, loading: false, responsive: true, zones: vec![zone],
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

        // East at the cap with room left: its cap rises before a refusing server gets any.
        let villages = [on_sandbox(village("mining_village_01", 10, 40, 0.0)), on_sandbox(village("mining_village_09", 50, 5, 200.0))];
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");
        // East's buildings full: the refusing one is better than no apartment.
        let villages = [on_sandbox(village("mining_village_01", 10, 40, 0.0)), on_sandbox(village("mining_village_09", 50, 0, 200.0))];
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
            uuid: "uuid-space".into(), name: "space".into(), players: 0, accepting: true, tps: 60, loading: false, responsive: true, zones: vec![Zone::space()],
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
        // East's buildings full, west has room but is frozen: no apartment rather than
        // one on a server that will not simulate the player (preprod, 2026-10-16).
        let villages = [on_sandbox(village("mining_village_01", 10, 40, 0.0)), on_sandbox(village("mining_village_09", 50, 0, 200.0))];
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
        let back = mesh_load::view_of(sandbox_load(50, true, 0));
        assert_eq!(villages_to_request(&villages, &villages[0], 50, 100, Some(&back)).len(), 2);
    }

    /// `sandbox_load` with no placement capacity: every server under its split rule.
    fn uncapped(west_players: u32, east_players: u32) -> LoadView {
        let mut load = sandbox_load(west_players, true, east_players);
        load.capacity = None;
        mesh_load::view_of(load)
    }

    #[test]
    fn without_load_every_village_rises_by_ten_together() {
        let villages = [village("mining_village_01", 50, 30, 0.0), village("mining_village_02", 50, 30, 10.0)];
        assert_eq!(village_cap(&villages, &villages[0], 50, None), 60);
        // Both fill to 60 before either goes to 70.
        let villages = [village("mining_village_01", 60, 20, 0.0), village("mining_village_02", 55, 25, 10.0)];
        assert_eq!(village_cap(&villages, &villages[0], 50, None), 60);
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, None).unwrap().name, "mining_village_02");
        let villages = [village("mining_village_01", 60, 20, 0.0), village("mining_village_02", 60, 20, 10.0)];
        assert_eq!(village_cap(&villages, &villages[0], 50, None), 70);
    }

    #[test]
    fn the_cap_rises_on_the_least_loaded_server_only() {
        // West: 2 villages at 50 (100 players), east: 3 at 50 (150). West goes to 60, east stays.
        let villages = [
            on_sandbox(village("mining_village_01", 50, 30, 0.0)),
            on_sandbox(village("mining_village_02", 50, 30, 10.0)),
            on_sandbox(village("mining_village_09", 50, 30, 200.0)),
            on_sandbox(village("mining_village_10", 50, 30, 210.0)),
            on_sandbox(village("mining_village_11", 50, 30, 220.0)),
        ];
        let load = uncapped(100, 150);
        let chosen = select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap();
        assert!(chosen.name == "mining_village_01" || chosen.name == "mining_village_02", "west: {}", chosen.name);
        assert_eq!(village_cap(&villages, chosen, 50, Some(&load)), 60);
        // West past east: east takes the arrivals, its villages at 60 now.
        let load = uncapped(160, 150);
        let chosen = select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap();
        assert!(chosen.name.as_str() >= "mining_village_09", "east: {}", chosen.name);
        assert_eq!(village_cap(&villages, chosen, 50, Some(&load)), 60);
    }

    #[test]
    fn arrivals_even_out_the_players_per_server() {
        // West: 2 villages, east: 3, all at 50 (100 vs 150 players). 200 more arrive.
        let mut villages = vec![
            on_sandbox(village("mining_village_01", 50, 100, 0.0)),
            on_sandbox(village("mining_village_02", 50, 100, 10.0)),
            on_sandbox(village("mining_village_09", 50, 100, 200.0)),
            on_sandbox(village("mining_village_10", 50, 100, 210.0)),
            on_sandbox(village("mining_village_11", 50, 100, 220.0)),
        ];
        let (mut west, mut east) = (100u32, 150u32);
        for _ in 0..200 {
            let load = uncapped(west, east);
            let chosen = select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().uuid.clone();
            let v = villages.iter_mut().find(|v| v.uuid == chosen).unwrap();
            v.occupancy += 1;
            v.free -= 1;
            if v.position.x < 100.0 { west += 1 } else { east += 1 }
        }
        assert!(west.abs_diff(east) <= 1, "west {} east {}", west, east);
        // West's two villages carry the difference; east's three stay close to each other.
        let occ: Vec<usize> = villages.iter().map(|v| v.occupancy).collect();
        assert!(occ[0].abs_diff(occ[1]) <= CAP_STEP && occ[2..].iter().max().unwrap() - occ[2..].iter().min().unwrap() <= CAP_STEP, "{:?}", occ);
    }

    #[test]
    fn at_equal_load_a_village_under_the_base_cap_goes_first() {
        let villages = [
            on_sandbox(village("mining_village_01", 50, 30, 0.0)),   // west, at the cap
            on_sandbox(village("mining_village_09", 40, 30, 200.0)), // east, room under 50
        ];
        let load = uncapped(100, 100);
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");
    }

    #[test]
    fn a_server_cap_stays_while_one_of_its_villages_waits_for_habs() {
        let mut requested = on_sandbox(village("mining_village_02", 0, 0, 10.0));
        requested.spawn_requested = true;
        let villages = [
            on_sandbox(village("mining_village_01", 50, 30, 0.0)),   // west, at the cap
            requested,                                               // west, habs coming
            on_sandbox(village("mining_village_09", 45, 30, 200.0)), // east
        ];
        let load = uncapped(50, 120);
        assert_eq!(village_cap(&villages, &villages[0], 50, Some(&load)), 50, "habs coming: they take the next players");
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");
        // No physical room anywhere: nothing to raise.
        let villages = [village("mining_village_01", 50, 0, 0.0)];
        assert_eq!(village_cap(&villages, &villages[0], 50, None), 50);
    }

    #[test]
    fn an_overloaded_server_neither_gets_the_arrivals_nor_the_habs() {
        // East over the placement capacity (75) with a fresh village under 50, west
        // accepting with its villages at 50: west goes to 60 rather than east getting them.
        let villages = [
            on_sandbox(village("mining_village_01", 50, 30, 0.0)),
            on_sandbox(village("mining_village_09", 3, 47, 200.0)),
            on_sandbox(village("mining_village_10", 0, 0, 210.0)),   // east, unspawned
        ];
        let load = mesh_load::view_of(sandbox_load(50, true, 230));
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_01");
        assert!(villages_to_request(&villages, &villages[0], 50, 100, Some(&load)).is_empty());
    }

    #[test]
    fn a_frozen_village_does_not_hold_the_cap_down() {
        let villages = [
            on_sandbox(village("mining_village_01", 10, 40, 0.0)),    // west, frozen
            on_sandbox(village("mining_village_09", 50, 30, 200.0)),  // east, at the cap
        ];
        let frozen = sandbox_load_west(10, false, false, 50);
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&frozen)).unwrap().name, "mining_village_09");
        assert_eq!(village_cap(&villages, &villages[1], 50, Some(&frozen)), 60);
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
        // East (accepting): one more there (2 waiting). West refuses players: none there.
        assert_eq!(names, ["mining_village_06"]);
        // Nobody accepting: the refusing servers are asked too, east first.
        let mut nobody = sandbox_load(50, false, 0);
        nobody.servers[1].accepting = false;
        let load = mesh_load::view_of(nobody);
        let names: Vec<&str> = villages_to_request(&villages, &villages[0], 50, 1000, Some(&load)).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_06", "mining_village_02", "mining_village_03"]);
    }

    // ---- queue

    #[test]
    fn a_player_waits_for_a_refusing_server_however_long_it_takes() {
        let west = on_sandbox(village("mining_village_01", 10, 40, 0.0));
        let load = mesh_load::view_of(sandbox_load(70, false, 10));
        assert!(!admits(Some(&load), west.world.as_ref()), "west refuses: queued, never forced in");
        let east = on_sandbox(village("mining_village_09", 10, 40, 200.0));
        assert!(admits(Some(&load), east.world.as_ref()), "east takes players");
        assert!(admits(None, west.world.as_ref()), "no load: as before the queue");
    }

    #[test]
    fn a_server_at_capacity_queues_until_a_split_makes_room() {
        let west = on_sandbox(village("mining_village_01", 10, 40, 0.0));
        // West at the placement capacity (75): queued.
        let full = mesh_load::view_of(sandbox_load(75, true, 75));
        assert!(refusing_owner(Some(&full), west.world.as_ref()).is_some());
        assert!(!admits(Some(&full), west.world.as_ref()));
        // Split done, 40 players left on west: in.
        let split = mesh_load::view_of(sandbox_load(40, true, 75));
        assert!(admits(Some(&split), west.world.as_ref()));
    }

    fn returning(stored: serde_json::Value) -> Arrival {
        Arrival::new(&serde_json::json!({ "object_uuid": "p1", "object_data": stored.clone() }), Some(stored))
    }

    #[test]
    fn a_returning_player_wakes_up_in_its_apartment_with_everything_it_had() {
        let arrival = returning(serde_json::json!({
            "name": "bob",
            "position": { "x": 900.0, "y": 1.0, "z": 3.0 },
            "rotation": { "x": 0.0, "y": 3.0, "z": 0.0 },
            "parent_id": "truck-1",
            "spawn_appartment_id": "building-1",
            "inventory": ["pickaxe"],
        }));
        let data = player_object_data(&arrival, Vec3::new(1.0, 2.0, 3.0), "building-1", false);
        assert_eq!(data["parent_id"], "building-1");
        assert_eq!(data["position"]["x"], 1.0);
        assert_eq!(data["rotation"]["y"], 1.708);
        assert_eq!(data["spawn_appartment_id"], "building-1");
        assert_eq!(data["inventory"][0], "pickaxe");
        assert_eq!(data["name"], "bob");
    }

    #[test]
    fn the_apartment_of_a_player_never_changes_unless_it_gets_a_new_one() {
        let arrival = returning(serde_json::json!({ "name": "bob", "spawn_appartment_id": "building-1" }));
        // Its own apartment found in another building record: the stored one stays.
        assert_eq!(player_object_data(&arrival, Vec3::zero(), "building-2", false)["spawn_appartment_id"], "building-1");
        // No apartment of its own was found, it was given a new one.
        assert_eq!(player_object_data(&arrival, Vec3::zero(), "building-2", true)["spawn_appartment_id"], "building-2");
        // A player from before the apartments.
        let old = returning(serde_json::json!({ "name": "bob" }));
        assert_eq!(player_object_data(&old, Vec3::zero(), "building-3", false)["spawn_appartment_id"], "building-3");
        // A new player.
        let new = Arrival::new(&serde_json::json!({ "object_uuid": "p2", "object_data": { "name": "ann", "is_npc": true } }), None);
        let data = player_object_data(&new, Vec3::zero(), "building-4", true);
        assert_eq!(data["spawn_appartment_id"], "building-4");
        assert_eq!(data["is_npc"], true);
    }

    #[test]
    fn an_admission_lets_one_create_object_through() {
        admitted().lock().unwrap().insert("p-admitted".to_string());
        assert!(take_admitted("p-admitted"));
        assert!(!take_admitted("p-admitted"), "the next one is queued again");
        assert!(!take_admitted("p-never"));
    }

    #[test]
    fn a_server_gets_at_most_its_share_of_arrivals_per_window() {
        let mut throttle = Throttle { limit: 2, ..Throttle::default() };
        let start = Instant::now();
        throttle.record("west", start);
        assert!(throttle.allows("west"));
        throttle.record("west", start);
        assert!(!throttle.allows("west"), "two in the window: the next one waits");
        assert!(throttle.allows("east"), "per server");
        throttle.prune(start + ADMIT_WINDOW);
        assert!(throttle.allows("west"), "the window is over");
        let unlimited = Throttle::default();
        assert!(unlimited.allows("west"));
    }

    #[test]
    fn a_server_that_had_its_share_sends_the_arrivals_elsewhere() {
        let villages = [
            on_sandbox(village("mining_village_01", 10, 40, 0.0)),   // west, least loaded
            on_sandbox(village("mining_village_09", 10, 40, 200.0)), // east
        ];
        let mut load = mesh_load::view_of(sandbox_load(10, true, 30));
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_01");
        load.refuse("uuid-west");
        assert_eq!(select_village(&villages, &AssignRequest::default(), 50, Some(&load)).unwrap().name, "mining_village_09");
        assert!(refusing_owner(Some(&load), villages[0].world.as_ref()).is_some());
    }

    #[test]
    fn a_waiting_player_hears_of_every_move_of_its_place() {
        let start = Instant::now();
        assert!(queued_notice_due(None, None, 40, start), "first time");
        assert!(!queued_notice_due(Some(start), Some(40), 40, start + Duration::from_secs(2)), "same place");
        assert!(queued_notice_due(Some(start), Some(40), 30, start + Duration::from_secs(2)), "moved");
        assert!(!queued_notice_due(Some(start), Some(40), 30, start + Duration::from_millis(500)), "not twice a second");
        assert!(queued_notice_due(Some(start), Some(40), 40, start + QUEUED_NOTIFY), "keep-alive");
    }
}

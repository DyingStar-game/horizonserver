use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use horizon_event_system::{EventSystem, GorcInstanceManager, GorcObjectId, Vec3};
use tokio::sync::Mutex;
use tracing::{debug, error, warn, info};

use crate::genericprops::{distance_squared, GenericProps};

/// Global lock to serialize apartment assignment across concurrent player-init calls.
/// Held from the start of the free-slot search until the building object has been
/// updated, so the next player always sees the freshly-committed occupancy list.
static APARTMENT_ASSIGN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn apartment_lock() -> &'static Mutex<()> {
    APARTMENT_ASSIGN_LOCK.get_or_init(|| Mutex::new(()))
}

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
/// villages fill one after the other, starting with mining_village_01.
fn select_village<'a>(villages: &'a [VillageState], request: &AssignRequest, max: usize) -> Option<&'a VillageState> {
    if let Some(preferred) = &request.preferred_village {
        let preferred = villages.iter().find(|v| &v.uuid == preferred).filter(|v| {
            if request.allow_over_cap { v.free > 0 } else { v.has_room_under_cap(max) }
        });
        if preferred.is_some() {
            return preferred;
        }
    }
    villages
        .iter()
        .filter(|v| v.has_room_under_cap(max))
        .max_by(|a, b| a.occupancy.cmp(&b.occupancy).then_with(|| b.name.cmp(&a.name)))
}

/// The village the next one is searched around when none has room: the fullest one with
/// buildings, i.e. the last village the players were sent to.
fn last_full_village(villages: &[VillageState]) -> Option<&VillageState> {
    villages
        .iter()
        .filter(|v| !v.buildings.is_empty())
        .max_by(|a, b| a.occupancy.cmp(&b.occupancy).then_with(|| b.name.cmp(&a.name)))
}

/// Places solo players can still get: the room under the cap of the villages with habs,
/// plus a full cap for each village whose habs were requested and have not arrived yet.
fn free_reserve(villages: &[VillageState], max: usize) -> usize {
    villages
        .iter()
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

/// The unspawned villages to request so the reserve gets back to `target`, closest to
/// `reference` first. Several can be requested at once: a wave of arrivals outruns one
/// village at a time (preprod, 2026-10-01: 16 players without apartment in 14 s).
fn villages_to_request<'a>(
    villages: &'a [VillageState],
    reference: &VillageState,
    max: usize,
    target: usize,
) -> Vec<&'a VillageState> {
    let mut reserve = free_reserve(villages, max);
    if reserve >= target || max == 0 {
        return Vec::new();
    }
    let mut candidates: Vec<&VillageState> = villages
        .iter()
        .filter(|v| v.uuid != reference.uuid && !v.is_spawn_pending_or_done())
        .collect();
    candidates.sort_by(|a, b| {
        distance_squared(a.position, reference.position)
            .total_cmp(&distance_squared(b.position, reference.position))
    });
    let mut chosen = Vec::new();
    for village in candidates {
        if reserve >= target {
            break;
        }
        reserve += max;
        chosen.push(village);
    }
    chosen
}

/// Every poi_village with the occupancy of the spawnbuildings linked to it by `poi_uuid`.
async fn collect_villages(gorc_instances: &GorcInstanceManager) -> Vec<VillageState> {
    let mut villages: Vec<VillageState> = Vec::new();
    for gorc_id in gorc_instances.get_objects_by_type("poi_village").await {
        let village = gorc_instances
            .with_object_mut(gorc_id, |instance| {
                instance.get_object::<GenericProps>().map(|v| VillageState {
                    gorc_id: Some(gorc_id),
                    uuid: v.uuid.clone(),
                    name: prop_value(v, "name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
                    // Local to the planet, like every village: comparable between villages.
                    position: prop_value(v, "position")
                        .and_then(|p| serde_json::from_value::<Vec3>(p.clone()).ok())
                        .unwrap_or_else(Vec3::zero),
                    is_spawned: prop_bool(v, "is_spawned"),
                    spawn_requested: prop_bool(v, "spawn_requested"),
                    occupancy: 0,
                    free: 0,
                    buildings: Vec::new(),
                })
            })
            .await
            .flatten();
        villages.extend(village);
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

/// Keep `free_reserve` places ahead of the arrivals: mark the closest unspawned villages to
/// `reference` as `spawn_requested`. Godot (the server owning each zone) then spawns their
/// habs, which come back as spawnbuildings with the village's poi_uuid.
async fn request_villages(
    gorc_instances: &GorcInstanceManager,
    events: &EventSystem,
    villages: &[VillageState],
    reference: &VillageState,
) {
    let config = village_config();
    let targets = villages_to_request(villages, reference, config.max_players_per_village, config.free_reserve);
    if targets.is_empty() {
        let reserve = free_reserve(villages, config.max_players_per_village);
        if reserve < config.free_reserve {
            warn!(
                "plugin genericprops (new_player): no unspawned poi_village left to request near {} ({} free places, reserve {})",
                reference.name, reserve, config.free_reserve
            );
        }
        return;
    }
    for target in targets {
        info!(
            "plugin genericprops (new_player): requesting the habs of village {} ({}) near {} (free places: {}, reserve: {})",
            target.name, target.uuid, reference.name,
            free_reserve(villages, config.max_players_per_village), config.free_reserve
        );

        // In-memory first, still under the assignment lock: the next player sees the request
        // and does not send it again.
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

pub async fn handle_new_player(
    event: serde_json::Value,
    events: Arc<EventSystem>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let player_uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
    let player_name = event["object_data"]["name"].as_str().unwrap_or_default().to_string();
    let is_npc = event["object_data"]["is_npc"].as_bool().unwrap_or(false);
    info!(
        "plugin genericprops (new_player): received new_player event for player_uuid={} player_name={}",
        player_uuid, player_name
    );

    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available");
        return Ok(());
    };

    let mut spawn_position = Vec3::zero();
    let mut found = false;
    let mut building_uuid = String::new();

    // Pass 1 (no lock): check whether this player already has an apartment.
    // This is purely read-only — no concurrent modification risk.
    for gorc_id in gorc_instances.get_objects_by_type("spawnbuilding").await {
        let existing = gorc_instances
            .with_object_mut(gorc_id, |instance| {
                let building = instance.get_object::<GenericProps>()?;
                let slot = apartments(building)
                    .iter()
                    .find(|entry| entry.get("player_uuid").and_then(|v| v.as_str()) == Some(player_uuid.as_str()))
                    .and_then(apartment_slot)?;
                Some((slot, slot_position(building, slot), building.uuid.clone()))
            })
            .await
            .flatten();
        if let Some((slot, position, uuid)) = existing {
            info!(
                "plugin genericprops (new_player): player {} already has apartment {:?} in building {} → position {:?}",
                player_uuid, slot, uuid, position
            );
            spawn_position = position;
            building_uuid = uuid;
            found = true;
            break;
        }
    }

    // Pass 2 (locked): only reached when the player has no existing apartment.
    // The lock serializes concurrent assignments so two players can never claim the same slot.
    if !found {
        let apartment_guard = apartment_lock().lock().await;
        let config = village_config();
        let max = config.max_players_per_village;
        let villages = collect_villages(&gorc_instances).await;

        match select_village(&villages, &AssignRequest::default(), max) {
            Some(village) => {
                for gorc_id in village.buildings.iter().copied() {
                    if let Some((position, uuid)) =
                        try_assign_in_building(&gorc_instances, &events, gorc_id, &player_uuid, &player_name).await?
                    {
                        info!(
                            "plugin genericprops (new_player): player {} gets an apartment in village {} ({}/{} players)",
                            player_uuid, village.name, village.occupancy + 1, max
                        );
                        spawn_position = position;
                        building_uuid = uuid;
                        found = true;
                        break;
                    }
                }

                // Keep the reserve of places ahead of the next arrivals, counting the
                // apartment just given.
                let mut after = villages.clone();
                if found {
                    if let Some(v) = after.iter_mut().find(|v| v.uuid == village.uuid) {
                        v.occupancy += 1;
                        v.free = v.free.saturating_sub(1);
                    }
                }
                request_villages(&gorc_instances, &events, &after, village).await;
            }
            None => {
                // Solo players never go past the cap: the room left in the buildings is
                // kept for players joining their friends / group.
                match last_full_village(&villages) {
                    Some(reference) => request_villages(&gorc_instances, &events, &villages, reference).await,
                    None => warn!("plugin genericprops (new_player): no poi_village with spawnbuildings"),
                }
                warn!(
                    "plugin genericprops (new_player): every village is at its cap of {} players, player {} gets no apartment (raise village_free_reserve?)",
                    max, player_uuid
                );
            }
        }

        // Release the lock before broadcasting. The GORC object is fully
        // committed, so concurrent players waiting on the lock will see the
        // updated occupancy list.
        drop(apartment_guard);

        if found {
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
    }

    if !found {
        warn!("plugin genericprops (new_player): no free spawnbuilding slot found for player {}, spawning at origin", player_uuid);
    }
    // Create the player object in the GORC system.
    if let Err(e) = events.emit_plugin(
        "genericprops",
        "create_object",
        &serde_json::json!({
            "object_type": "player",
            "object_uuid": player_uuid,
            "object_data": {
                "name": player_name,
                "position": { "x": spawn_position.x, "y": spawn_position.y, "z": spawn_position.z },
                "rotation": { "x": 0.0, "y": 1.708, "z": 0.0 },
                "parent_id": building_uuid,
                "spawn_appartment_id": building_uuid,
                "is_npc": is_npc,
            }
        }),
    ).await {
        error!("plugin genericprops (new_player): failed to emit create_object: {}", e);
    }

    Ok(())
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
            is_spawned: free > 0 || occupancy > 0,
            spawn_requested: false,
            occupancy,
            free,
            buildings: if free > 0 || occupancy > 0 { vec![GorcObjectId::new()] } else { Vec::new() },
        }
    }

    #[test]
    fn starts_with_first_village_by_name() {
        let villages = [village("mining_village_02", 0, 100, 10.0), village("mining_village_01", 0, 100, 0.0)];
        let chosen = select_village(&villages, &AssignRequest::default(), 50).unwrap();
        assert_eq!(chosen.name, "mining_village_01");
    }

    #[test]
    fn fills_fullest_village_under_cap_first() {
        let villages = [village("mining_village_01", 10, 100, 0.0), village("mining_village_02", 30, 100, 10.0)];
        let chosen = select_village(&villages, &AssignRequest::default(), 50).unwrap();
        assert_eq!(chosen.name, "mining_village_02");
    }

    #[test]
    fn solo_player_never_goes_past_cap() {
        // Plenty of physical room, but the village holds its 50 players.
        let villages = [village("mining_village_01", 50, 400, 0.0)];
        assert!(select_village(&villages, &AssignRequest::default(), 50).is_none());
    }

    #[test]
    fn preferred_village_over_cap_within_physical_room() {
        let villages = [village("mining_village_01", 50, 1, 0.0), village("mining_village_02", 0, 100, 10.0)];
        let request = AssignRequest { preferred_village: Some("uuid-mining_village_01".into()), allow_over_cap: true };
        assert_eq!(select_village(&villages, &request, 50).unwrap().name, "mining_village_01");

        let full = [village("mining_village_01", 51, 0, 0.0), village("mining_village_02", 0, 100, 10.0)];
        assert_eq!(select_village(&full, &request, 50).unwrap().name, "mining_village_02");
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
        assert_eq!(free_reserve(&villages, 50), 5);
        // 5 free places, 100 wanted: two villages at once, closest first.
        let names: Vec<&str> = villages_to_request(&villages, reference, 50, 100).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_02", "mining_village_04"]);
        // Reserve already met: nothing.
        assert!(villages_to_request(&villages, reference, 50, 5).is_empty());
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
        assert_eq!(free_reserve(&villages, 50), 55);
        let reference = &villages[0];
        // 55 < 100: one more, never the one already requested.
        let names: Vec<&str> = villages_to_request(&villages, reference, 50, 100).iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["mining_village_04"]);
        assert!(villages_to_request(&villages, reference, 50, 50).is_empty());
    }
}

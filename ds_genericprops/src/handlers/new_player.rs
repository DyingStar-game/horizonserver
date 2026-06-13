use std::sync::{Arc, OnceLock};
use horizon_event_system::EventSystem;
use tokio::sync::Mutex;
use tracing::{debug, error, warn, info};

use crate::genericprops::GenericProps;

/// Global lock to serialize apartment assignment across concurrent player-init calls.
/// Held from the start of the free-slot search until the building object has been
/// updated, so the next player always sees the freshly-committed occupancy list.
static APARTMENT_ASSIGN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn apartment_lock() -> &'static Mutex<()> {
    APARTMENT_ASSIGN_LOCK.get_or_init(|| Mutex::new(()))
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
    debug!(
        "plugin genericprops (new_player): Handling new player uuid {:?}",
        player_uuid
    );

    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available");
        return Ok(());
    };

    // Search for a spawnbuilding with at least one available apartment.
    let building_ids = gorc_instances.get_objects_by_type("spawnbuilding").await;

    let mut spawn_position = horizon_event_system::Vec3::zero();
    let mut found = false;
    let mut building_uuid = String::new();

    // Pass 1 (no lock): check whether this player already has an apartment.
    // This is purely read-only — no concurrent modification risk.
    'existing: for gorc_id in building_ids.iter().copied() {
        if let Some(mut instance) = gorc_instances.get_object(gorc_id).await {
            if let Some(building) = instance.get_object_mut::<GenericProps>() {
                let existing_slot = building.data.values()
                    .filter_map(|v| v.get("apartments").and_then(|a| a.as_array()))
                    .flatten()
                    .find(|entry| entry.get("player_uuid").and_then(|v| v.as_str()) == Some(player_uuid.as_str()))
                    .and_then(|entry| {
                        let f = entry.get("floor").and_then(|v| v.as_i64())?;
                        let r = entry.get("row").and_then(|v| v.as_i64())?;
                        let c = entry.get("col").and_then(|v| v.as_i64())?;
                        Some((f, r, c))
                    });

                if let Some((floor, row, col)) = existing_slot {
                    let x_spacing = building.data.values()
                        .filter_map(|v| v.get("x_spacing").and_then(|s| s.as_f64()))
                        .next()
                        .unwrap_or(5.0);
                    let y_spacing = building.data.values()
                        .filter_map(|v| v.get("y_spacing").and_then(|s| s.as_f64()))
                        .next()
                        .unwrap_or(5.0);
                    let z_spacing = building.data.values()
                        .filter_map(|v| v.get("z_spacing").and_then(|s| s.as_f64()))
                        .next()
                        .unwrap_or(5.0);
                    let spawn_point_x = building.data.values()
                        .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("x")).and_then(|x| x.as_f64()))
                        .next()
                        .unwrap_or(0.0);
                    let spawn_point_y = building.data.values()
                        .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("y")).and_then(|y| y.as_f64()))
                        .next()
                        .unwrap_or(0.0);
                    let spawn_point_z = building.data.values()
                        .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("z")).and_then(|z| z.as_f64()))
                        .next()
                        .unwrap_or(0.0);

                    let x = if col == 0 { spawn_point_x } else { x_spacing - spawn_point_x };
                    let z = if col == 0 { spawn_point_z } else { z_spacing - spawn_point_z };
                    spawn_position = horizon_event_system::Vec3::new(
                        x + (row as f64 * x_spacing),
                        spawn_point_y + (floor as f64 * y_spacing),
                        z + (col as f64 * z_spacing),
                    );
                    found = true;
                    building_uuid = building.uuid.clone();
                    info!(
                        "plugin genericprops (new_player): player {} already has apartment (floor={}, row={}, col={}) in building {} → position {:?}",
                        player_uuid, floor, row, col, building_uuid, spawn_position
                    );
                    break 'existing;
                }
            }
        }
    }

    // Pass 2 (locked): only reached when the player has no existing apartment.
    // The lock serializes concurrent assignments so two players can never claim the same slot.
    if !found {
        let _apartment_guard = apartment_lock().lock().await;

        'search: for gorc_id in building_ids {
            if let Some(mut instance) = gorc_instances.get_object(gorc_id).await {
                // Scope the mutable building borrow so we can call update_object
                // on `instance` after the borrow is dropped.
                let commit_data = {
                    if let Some(building) = instance.get_object_mut::<GenericProps>() {
                        // Read grid dimensions and availability from the channel data map.
                        let available = building.data.values()
                            .filter_map(|v| v.get("available").and_then(|a| a.as_i64()))
                            .next()
                            .unwrap_or(0);

                        if available <= 0 {
                            error!("plugin genericprops (new_player): spawnbuilding {} has no available apartments", building.uuid);
                            None
                        } else {
                            let cols = building.data.values()
                                .filter_map(|v| v.get("cols").and_then(|c| c.as_i64()))
                                .next()
                                .unwrap_or(1);
                            let rows = building.data.values()
                                .filter_map(|v| v.get("rows").and_then(|r| r.as_i64()))
                                .next()
                                .unwrap_or(1);
                            let floors = building.data.values()
                                .filter_map(|v| v.get("floors").and_then(|f| f.as_i64()))
                                .next()
                                .unwrap_or(1);

                            let x_spacing = building.data.values()
                                .filter_map(|v| v.get("x_spacing").and_then(|s| s.as_f64()))
                                .next()
                                .unwrap_or(5.0);
                            let y_spacing = building.data.values()
                                .filter_map(|v| v.get("y_spacing").and_then(|s| s.as_f64()))
                                .next()
                                .unwrap_or(5.0);
                            let z_spacing = building.data.values()
                                .filter_map(|v| v.get("z_spacing").and_then(|s| s.as_f64()))
                                .next()
                                .unwrap_or(5.0);

                            let spawn_point_x = building.data.values()
                                .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("x")).and_then(|x| x.as_f64()))
                                .next()
                                .unwrap_or(0.0);
                            let spawn_point_y = building.data.values()
                                .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("y")).and_then(|y| y.as_f64()))
                                .next()
                                .unwrap_or(0.0);
                            let spawn_point_z = building.data.values()
                                .filter_map(|v| v.get("spawn_point").and_then(|s| s.get("z")).and_then(|z| z.as_f64()))
                                .next()
                                .unwrap_or(0.0);

                            // Collect occupied (floor, row, col) tuples from the apartments list.
                            // Each entry is an object: { floor, row, col, player_uuid, player_name }.
                            let occupied: Vec<(i64, i64, i64)> = building.data.values()
                                .filter_map(|v| v.get("apartments").and_then(|a| a.as_array()))
                                .flatten()
                                .filter_map(|entry| {
                                    let f = entry.get("floor").and_then(|v| v.as_i64())?;
                                    let r = entry.get("row").and_then(|v| v.as_i64())?;
                                    let c = entry.get("col").and_then(|v| v.as_i64())?;
                                    Some((f, r, c))
                                })
                                .collect();

                            // Find the first free (floor, row, col) slot.
                            // All three dimensions are 0-based: floors=2 → indices 0 and 1,
                            // rows=2 → indices 0 and 1, cols=10 → indices 0–9.
                            let free_slot = (0..floors)
                                .flat_map(|f| (0..rows).flat_map(move |r| (0..cols).map(move |c| (f, r, c))))
                                .find(|slot| !occupied.contains(slot));
println!("plugin genericprops (new_player): checking slot {:?} against occupied {:?}", free_slot, occupied);
                            if let Some((floor, row, col)) = free_slot {
                                println!("plugin genericprops (new_player): found free slot (floor={}, row={}, col={}) in building {}", floor, row, col, building.uuid);
                                println!("plugin genericprops (new_player): building spawn_point ({}, {}, {})", spawn_point_x, spawn_point_y, spawn_point_z);
                                let x = if col == 0 { spawn_point_x } else { x_spacing - spawn_point_x };
                                let z = if col == 0 { spawn_point_z } else { z_spacing - spawn_point_z };
                                let slot_spawn_position = horizon_event_system::Vec3::new(
                                    x + (row as f64 * x_spacing),
                                    spawn_point_y + (floor as f64 * y_spacing),
                                    z + (col as f64 * z_spacing),
                                );
                                println!("plugin genericprops (new_player): calculated spawn_position {:?} for slot (floor={}, row={}, col={})", slot_spawn_position, floor, row, col);

                                debug!(
                                    "plugin genericprops (new_player): assigned slot (floor={}, row={}, col={}) in building {} → position {:?}",
                                    floor, row, col, building.uuid, slot_spawn_position
                                );
println!("plugin genericprops (new_player): found free slot for player {} in building {}, updating building data", player_uuid, building.uuid);
                                let mut apartments = building.data.values()
                                    .filter_map(|v| v.get("apartments").and_then(|a| a.as_array()))
                                    .flatten()
                                    .cloned()
                                    .collect::<Vec<_>>();
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

                                // Wee need to send to update_object_from_external to broadcast the new occupancy to nearby clients and persistance, not needed to send to server godot
                                events.emit_plugin(
                                    "genericprops",
                                    "update_object_from_external",
                                    &serde_json::json!({
                                        "object_type": "spawnbuilding",
                                        "object_uuid": building.uuid,
                                        "object_data": {
                                            "available": available - 1,
                                            "apartments": apartments.clone(),
                                        },
                                    }),
                                ).await?;

                                Some((slot_spawn_position, building.uuid.clone(), apartments))
                            } else {
                                None
                            }
                        }
                    } else {
                        None
                    }
                }; // building borrow ends here — instance is free to move

                if let Some((slot_spawn_position, slot_building_uuid, _apartments)) = commit_data {
                    spawn_position = slot_spawn_position;
                    building_uuid = slot_building_uuid.clone();
                    found = true;

                    // Commit the mutated instance back to GORC immediately, still under
                    // the lock, so the next waiter reads fresh occupancy from get_object().
                    gorc_instances.update_object(gorc_id, instance).await;

                    // Release the lock before broadcasting.  The GORC object is fully
                    // committed, so concurrent players waiting on the lock will see the
                    // updated occupancy list.
                    drop(_apartment_guard);

                    // Broadcast the current (authoritative) building state to nearby clients.
                    // broadcast_only=true tells handle_object_update to read the fresh GORC
                    // state and send it to clients WITHOUT calling update_object again —
                    // that would overwrite our committed data with a stale snapshot.
                    if let Err(e) = events.emit_plugin(
                        "genericprops",
                        "update_object_from_external",
                        &serde_json::json!({
                            "object_type": "spawnbuilding",
                            "object_uuid": slot_building_uuid,
                            "object_data": {},
                            "broadcast_only": true,
                        }),
                    ).await {
                        error!("plugin genericprops (new_player): failed to broadcast building update: {}", e);
                    }

                    break 'search;
                }
            }
        }
    }

    if !found {
        warn!("plugin genericprops (new_player): no free spawnbuilding slot found for player {}, spawning at origin", player_uuid);
    }
println!("plugin genericprops (new_player): final spawn position for player {} is {:?}", player_uuid, spawn_position);
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

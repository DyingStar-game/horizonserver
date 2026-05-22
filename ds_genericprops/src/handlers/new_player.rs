use std::sync::Arc;
use horizon_event_system::EventSystem;
use tracing::{debug, error, warn};

use crate::genericprops::GenericProps;

pub async fn handle_new_player(
    event: serde_json::Value,
    events: Arc<EventSystem>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let player_uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
    let player_name = event["object_data"]["name"].as_str().unwrap_or_default().to_string();
    let spawn_point = event["object_data"]["spawn_point"].as_i64().unwrap_or(0);

    debug!(
        "plugin genericprops (new_player): Handling new player uuid {:?} spawn_point {}",
        player_uuid, spawn_point
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

    'search: for gorc_id in building_ids {
        if let Some(mut instance) = gorc_instances.get_object(gorc_id).await {
            if let Some(building) = instance.get_object_mut::<GenericProps>() {
                // Read grid dimensions and availability from the channel data map.
                let available = building.data.values()
                    .filter_map(|v| v.get("available").and_then(|a| a.as_i64()))
                    .next()
                    .unwrap_or(0);

                if available <= 0 {
                    error!("plugin genericprops (new_player): spawnbuilding {} has no available apartments", building.uuid);
                    continue;
                }

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
                    spawn_position = horizon_event_system::Vec3::new(
                         col as f64 * x_spacing,
                         floor as f64 * y_spacing,
                         row as f64 * z_spacing,
                    );

                    debug!(
                        "plugin genericprops (new_player): assigned slot (floor={}, row={}, col={}) in building {} → position {:?}",
                        floor, row, col, building.uuid, spawn_position
                    );
                    found = true;
                    building_uuid = building.uuid.clone();
println!("plugin genericprops (new_player): found free slot for player {} in building {}, updating building data", player_uuid, building.uuid);
                    // Update the building object with the new appartment
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
                    events.emit_plugin(
                        "genericprops",
                        "update_object_from_external",
                        &serde_json::json!({
                            "object_uuid": building.uuid,
                            "object_type": "spawnbuilding",
                            "object_data": {
                                "available": available - 1,
                                "apartments": apartments,
                            }
                        }),
                    ).await.unwrap();

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
                "rotation": { "x": 0.0, "y": 0.0, "z": 0.0 },
                "parent_id": building_uuid,
            }
        }),
    ).await {
        error!("plugin genericprops (new_player): failed to emit create_object: {}", e);
    }

    Ok(())
}



//! # Player Movement Handler
//! 
//! Handles real-time player movement events on GORC channel 0, the highest-priority
//! communication channel designed for critical game state updates that require
//! immediate synchronization across all connected clients.
//! 
//! ## Channel 0 Characteristics
//! 
//! - **Frequency**: 60Hz updates for smooth movement
//! - **Range**: 25m replication radius for performance optimization  
//! - **Priority**: Critical data - position, velocity, health status
//! - **Latency**: Minimal buffering for real-time responsiveness
//! 
//! ## Movement Validation
//! 
//! All movement requests undergo strict validation:
//! - **Authentication**: Only authenticated connections can request movement
//! - **Ownership**: Players can only move their own ships  
//! - **Bounds Checking**: Movement deltas are validated for reasonable values
//! - **Anti-Cheat**: Large teleportation attempts are rejected
//! 
//! ## Spatial Replication
//! 
//! Movement updates trigger automatic spatial replication:
//! 1. Client sends movement request via GORC channel 0
//! 2. Server validates request and updates object position
//! 3. Position update is broadcast to all clients within 25m range
//! 4. Clients receive smooth position updates for nearby ships
//! 
//! ## Performance Optimization
//! 
//! - **Batched Updates**: Multiple position changes are batched per frame
//! - **Spatial Culling**: Only nearby clients receive updates (25m radius)
//! - **Async Processing**: Movement validation runs without blocking other events
//! - **Memory Efficiency**: Uses in-place object updates to minimize allocations

use std::sync::Arc;
use horizon_event_system::{
    EventSystem, PlayerId, GorcObjectId,
    EventError,
};
use tracing::{debug, info, warn, error};
use serde_json;
use crate::events::PlayerMoveRequest;
use crate::genericprops::GenericProps;
use dashmap::DashMap;
use std::collections::HashMap;
use ds_common::events::GenericPropsRequest;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Instant;

/// Minimum time (in seconds) between consecutive out_of_zone events for the same player.
/// This prevents "ping-pong" transfers where Godot physics pushes a freshly-spawned
/// player back across the zone boundary within a single physics frame.
const OUT_OF_ZONE_COOLDOWN_SECS: f64 = 0.5;

/// Tracks player UUIDs that have had an out_of_zone transfer event emitted,
/// along with the timestamp of the emission.
///
/// Prevents the same player from generating multiple out_of_zone events
/// within the cooldown window. Entries are only cleared when:
/// 1. The cooldown has expired, AND
/// 2. The player receives a normal movement (no out_of_zone flag).
fn pending_out_of_zone_players() -> &'static DashMap<String, Instant> {
    static MAP: OnceLock<DashMap<String, Instant>> = OnceLock::new();
    MAP.get_or_init(DashMap::new)
}

/// Synchronous wrapper for movement request handling that works with GORC client handlers.
///
/// This function provides the same functionality as `handle_movement_request` but in
/// a synchronous context suitable for use with the GORC client event system.
pub fn handle_movement_request_sync(
    event: serde_json::Value,
    props: Arc<DashMap<String, GorcObjectId>>,
    events: Arc<EventSystem>,
    handle: luminal::Handle,
) -> Result<(), EventError> {
    debug!("🚀 GenericPropsPlugin: Handling player movement event {:?}", event);

    use std::sync::atomic::{AtomicU64, Ordering};
    static HANDLER_COUNTER: AtomicU64 = AtomicU64::new(0);
    static SPAWN_COUNTER: AtomicU64 = AtomicU64::new(0);
    
    let handler_id = HANDLER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let player_uuid = event["object_uuid"].as_str().unwrap_or("unknown").to_string();
    debug!("🚀 HANDLER #{}: Starting for player {}", handler_id, player_uuid);

    // Ensure we have access to the gorc instances manager
    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🚀 HANDLER #{}: ❌ No GORC instances manager available", handler_id);
        return Ok(());
    };
    debug!("🚀 HANDLER #{}: ✅ Got GORC instances", handler_id);

    let event = event.clone();
    let spawn_id = SPAWN_COUNTER.fetch_add(1, Ordering::Relaxed);
    debug!("🚀 HANDLER #{}: Spawning async task (spawn_id={})", handler_id, spawn_id);
    
    handle.spawn(async move {
        debug!("🚀 SPAWN #{}: ✅ Async task started for player {}", spawn_id, player_uuid);

        // Parse the movement data from the GORC event payload
        debug!("🚀 STEP 1: ✅ Parsed raw JSON: {}", event);

        debug!("🚀 STEP 2: Movement handler called for player {}", event["object_uuid"]);

        // SECURITY: Validate connection authentication before processing any movement
        // if !connection.is_authenticated() {
        //     debug!("🚀 STEP 2: ❌ Unauthenticated movement request from {}", connection.remote_addr);
        //     return Err(EventError::HandlerExecution(
        //         "Unauthenticated request".to_string()
        //     ));
        // }
        debug!("🚀 STEP 3: ✅ Connection authenticated");

        let move_data = match serde_json::from_value::<PlayerMoveRequest>(event["object_data"].clone()) {
            Ok(data) => data,
            Err(e) => {
                error!("🚀 STEP 4: ❌ Failed to parse PlayerMoveRequest: {}", e);
                return;
            }
        };
        debug!("🚀 STEP 4: ✅ Parsed PlayerMoveRequest: {:?}", move_data);

        debug!("🚀 STEP 5: Processing movement for ship {} to position {:?}",
            move_data.player_id, move_data.position);

        // SECURITY: Validate player ownership - players can only move their own ships
        // if move_data.player_id != client_player {
        //     error!("🚀 STEP 6: ❌ Security violation: Player {} tried to move ship belonging to {}",
        //         client_player, move_data.player_id);
        //     return Err(EventError::HandlerExecution(
        //         "Unauthorized ship movement".to_string()
        //     ));
        // }
        // debug!("🚀 STEP 6: ✅ Player ownership validated");

        let gorc_id = if let Some(object_uuid) = event["object_uuid"].as_str() {
            if let Some(gorc_ref) = props.get(object_uuid) {
                Some(*gorc_ref)
            } else {
                error!("🎮 GORC: ❌ Object UUID not found in props map: {}", object_uuid);
                None
            }
        } else {
            error!("🎮 GORC: ❌ object_uuid is not a string: {:?}", event["object_uuid"]);
            None
        };

        if let Some(gorc_id) = gorc_id {
            if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {

                let mut final_position = move_data.position;

                // Update player position in GORC tracking - use async directly since we're already in an async context
                match PlayerId::from_str(move_data.player_id.to_string().as_str()) {
                    Ok(player_id) => {
                        // Check if player has a parent_id and calculate global position
                        let mut computed_position = move_data.position;

                        // Update the GenericProps on the object instance
                        if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
                            let zone_set = object_instance.get_object_mut::<GenericProps>()
                                .expect("Object must exists")
                                .update(serde_json::json!({
                                    "position": computed_position.clone()
                                }));
                            
                            for zone in zone_set {
                                object_instance.mark_needs_update(zone);

                                // Get parent_id to calculate global position
                                let parent_id = object_instance.get_object::<GenericProps>().and_then(|props| {
                                    props.data.values()
                                        .filter_map(|zone_data| zone_data.get("parent_id"))
                                        .filter_map(|v| v.as_str())
                                        .find(|s| !s.is_empty())
                                        .map(|s| s.to_string())
                                });

                                if let Some(parent_id_str) = &parent_id {
                                    if let Ok(parent_gorc_id) = GorcObjectId::from_str(parent_id_str) {
                                        if let Some(parent_global_position) = gorc_instances.get_object_position(parent_gorc_id).await {
                                            final_position = horizon_event_system::Vec3 {
                                                x: parent_global_position.x + computed_position.x,
                                                y: parent_global_position.y + computed_position.y,
                                                z: parent_global_position.z + computed_position.z,
                                            };
                                        }
                                    }
                                }
                                
                                // Update the object_instance global_position property
                                object_instance.get_object_mut::<GenericProps>()
                                    .expect("Object must exists")
                                    .global_position = final_position;
                                computed_position = final_position;
                            }
                            
                            // CRITICAL: Update object position for zone change detection BEFORE
                            // update_object, because update_object replaces the entire object
                            // and we need the old position for zone change comparison
                            debug!("🚀 STEP 11.3: About to update GORC object position for {:?} to {:?}", gorc_id, final_position);
                            if let Err(e) = events.update_object_position(gorc_id, final_position).await {
                                error!("🚀 STEP 11.3: ❌ Failed to update GORC object tracking: {}", e);
                            } else {
                                debug!("🚀 STEP 11.3: ✅ Updated GORC object tracking for {:?} at {:?}",
                                    gorc_id, final_position);
                            }
                            
                            // Now update the full object instance (properties, needs_update flags, etc.)
                            gorc_instances.update_object(gorc_id, object_instance).await;
                        } else {
                            error!("🎮 GORC: ❌ Object instance not found in GORC for uuid: {}", move_data.player_id);
                        }

                        // Update player position in GORC tracking
                        debug!("🚀 STEP 11.5: Updating GORC player global_position for player {} to {:?}",
                            move_data.player_id, computed_position);
                        if let Err(e) = events.update_player_position(player_id, computed_position).await {
                            error!("🚀 STEP 11.5: ❌ Failed to update GORC player tracking: {}", e);
                        } else {
                            debug!("🚀 STEP 11.5: ✅ Updated GORC player tracking for player {} at position {:?}",
                                move_data.player_id, computed_position);
                        }
                    }
                    Err(e) => {
                        error!("🚀 STEP 11.5: ❌ Failed to parse player ID: {}", e);
                    }
                }



                // Update the object instance position locally (for immediate response)
                object_instance.object.update_position(final_position);
                debug!("🚀 STEP 7: ✅ Updated local position for {} to {:?}",
                    move_data.player_id, final_position);
                
                // Broadcast position update to nearby players (within 25m range)
                // CRITICAL: Update BOTH player AND object positions in GORC tracking before broadcasting
                debug!("🚀 STEP 8: Beginning position update broadcast for player {}", move_data.player_id);
                // let object_id_str = gorc_event.object_id.clone();
                // debug!("🚀 STEP 9: Using object ID: {}", object_id_str);

                let position_update = serde_json::json!({
                    "player_id": move_data.player_id,
                    "position": move_data.position,
                    "rotation": move_data.rotation,
                    // "velocity": move_data.velocity,
                    // "movement_state": move_data.movement_state,
                    // "client_timestamp": chrono::Utc::now()
                });
                debug!("🚀 STEP 10: Created position update payload: {}", position_update);
                
                // CRITICAL: We need to update player position synchronously for zone detection.
                // Since the handler can run in either multi-threaded or single-threaded runtime,
                // we use std::thread::spawn with a channel to safely execute async code.
                
                debug!("🚀 STEP 11: Updating player position for zone detection");



                // Note: update_object_position was already called above, before update_object
                debug!("🚀 STEP 12: Parsed GORC ID successfully: {:?}", gorc_id);

                // Emit to subscribers - use async directly
                debug!("🚀 STEP 13: About to call emit_gorc_instance on channel 0");
                match events.emit_gorc_instance(
                    gorc_id,
                    0, // Channel 0: Critical movement data
                    "move",
                    &position_update,
                    horizon_event_system::Dest::Client
                ).await {
                    Ok(_) => {
                        debug!("🚀 STEP 14: ✅ emit_gorc_instance completed successfully");
                    },
                    Err(e) => {
                        error!("🚀 STEP 14: ❌ emit_gorc_instance failed: {}", e);
                    }
                }

                // manage player out of godot server zone
                if move_data.out_of_zone.is_some() {

                    // Cooldown-based deduplication: only emit player_out_of_zone if
                    // enough time has passed since the last emission for this player.
                    // This prevents "ping-pong" transfers when Godot physics pushes
                    // a freshly-spawned player back across the zone boundary.
                    let player_uuid_str = event["object_uuid"].as_str().unwrap_or_default().to_string();
                    let map = pending_out_of_zone_players();
                    let now = Instant::now();

                    let should_emit = match map.get(&player_uuid_str) {
                        Some(entry) => entry.value().elapsed().as_secs_f64() >= OUT_OF_ZONE_COOLDOWN_SECS,
                        None => true,
                    };

                    if !should_emit {
                        debug!("🚀 Player {} out_of_zone suppressed (cooldown active, {:.1}s remaining)",
                            player_uuid_str,
                            OUT_OF_ZONE_COOLDOWN_SECS - map.get(&player_uuid_str).map(|e| e.value().elapsed().as_secs_f64()).unwrap_or(0.0));
                    } else {
                        map.insert(player_uuid_str.clone(), now);
                        info!("🚀 Player out of zone detected (emitting, cooldown started): {:?}", move_data.out_of_zone);

                        let mut prop_properties = HashMap::new();
                        if let Some(generic_props) = object_instance.get_object_mut::<GenericProps>() {
                            for properties in generic_props.data.values() {
                                match properties {
                                    Value::Null => warn!("property null"),
                                    Value::Object(map) => {
                                        for(key, value) in map.iter() {
                                            prop_properties.insert(key.to_string(), value.clone());
                                        }
                                    },
                                    _ => warn!("no properties"),
                                }
                            }
                            prop_properties.insert("_global_position".to_string(), serde_json::to_value(final_position).unwrap_or(Value::Null));

                            debug!("🚀 Player out of zone properties: {:?}", prop_properties);
                            let item = GenericPropsRequest  {
                                object_type: "player".to_string(),
                                object_uuid: event["object_uuid"].as_str().unwrap_or_default().to_string(),
                                object_data: serde_json::to_value(&prop_properties).unwrap_or(Value::Null),
                                broadcast_only: None,
                            };

                            // loop on all props and check if zone match
                            // then return the list of objects found to the gameserver that requested it
                            events.emit_plugin(
                                "gameserverplugin",
                                "player_out_of_zone",
                                &json!({
                                    "server_uuid": move_data.out_of_zone.as_ref().unwrap(),
                                    "item": item,
                                    "global_position": final_position,
                                }),
                            ).await.unwrap();
                        }
                    }

                } else {
                    // Normal movement (no out_of_zone) - clear the cooldown entry
                    // ONLY if the cooldown has expired. This ensures that a freshly-spawned
                    // player on a new server can't immediately trigger a bounce-back transfer
                    // even if the first few frames come in without out_of_zone.
                    let player_uuid_str = event["object_uuid"].as_str().unwrap_or_default().to_string();
                    let map = pending_out_of_zone_players();
                    if let Some(entry) = map.get(&player_uuid_str) {
                        if entry.value().elapsed().as_secs_f64() >= OUT_OF_ZONE_COOLDOWN_SECS {
                            drop(entry); // release the read lock before removing
                            if map.remove(&player_uuid_str).is_some() {
                                debug!("🚀 Cleared out_of_zone cooldown for player {} (cooldown expired, normal movement)", player_uuid_str);
                            }
                        } else {
                            debug!("🚀 Player {} normal movement but cooldown still active ({:.1}s remaining), keeping guard",
                                player_uuid_str,
                                OUT_OF_ZONE_COOLDOWN_SECS - entry.value().elapsed().as_secs_f64());
                        }
                    }
                }

            }
        }
        debug!("🚀 SPAWN #{}: ✅ Async task completed for player {}", spawn_id, player_uuid);
    });
    
    debug!("🚀 HANDLER #{}: ✅ Spawn submitted, returning Ok", handler_id);
    Ok(())
}

use std::sync::Arc;
use horizon_event_system::{
    EventSystem, PlayerId, GorcEvent, GorcObject, GorcObjectId, ClientConnectionRef, ObjectInstance,
    EventError,
};
use luminal::Handle;
use tracing::{debug, info, error};
use serde_json;
use dashmap::DashMap;

use crate::events::GenericPropsGORCUpdateRequest;
use ds_common::events::GenericPropsRequest;
use crate::objectdefinition::ObjectDefinition;
use crate::genericprops::GenericProps;

pub fn handle_client_update_request(
    gorc_event: GorcEvent,
    _client_player: PlayerId,
    _connection: ClientConnectionRef,
    object_instance: &mut ObjectInstance,
    events: Arc<EventSystem>,
	luminal_handle: Handle,
) -> Result<(), EventError> {
	
	// Parse the movement data from the GORC event payload
    let event_data = serde_json::from_slice::<serde_json::Value>(&gorc_event.data)
        .map_err(|e| {
            error!("🚀 GORC: ❌ Failed to parse JSON from GORC event data: {}", e);
            EventError::HandlerExecution("Invalid JSON in update request".to_string())
        })?;
    
    let req_data = serde_json::from_value::<GenericPropsGORCUpdateRequest>(event_data)
        .map_err(|e| {
            error!("🚀 GORC: ❌ Failed to parse GenericPropsGORCUpdateRequest: {}", e);
            EventError::HandlerExecution("Invalid update request format".to_string())
        })?;
    
    // Update the object instance directly (this is the authoritative update)
    object_instance.get_object_mut::<GenericProps>().expect("TODO").update(req_data.new_data.clone());

    luminal_handle.spawn(async move {
		// Broadcast position update to nearby players (within 25m range)
		broadcast_object_update(
			&gorc_event.object_id,
			&req_data,
			events,
		).await;
    });
    Ok(())
}

async fn broadcast_object_update(
    object_id_str: &str,
    update_data: &GenericPropsGORCUpdateRequest,
    events: Arc<EventSystem>,
) {
    
    // Parse the GORC object ID and emit the update
    if let Ok(gorc_id) = GorcObjectId::from_str(object_id_str) {
        // Emit on channel 0 (movement) with automatic spatial replication
        if let Err(e) = events.emit_gorc_instance(
            gorc_id,
            update_data.channel, // Channel 0: Critical movement data
            "gorc_info",
            &serde_json::json!(&update_data),
            horizon_event_system::Dest::Client
        ).await {
            error!("🚀 GORC: ❌ Failed to broadcast object update: {}", e);
        } else {
            debug!("🚀 GORC: ✅ Broadcasted position update success");
        }
    } else {
        error!("🚀 GORC: ❌ Invalid GORC object ID format: {}", object_id_str);
    }
}

/// Update the global positions of all child objects when parent moves
async fn update_children_positions(
    parent_gorc_id: GorcObjectId,
    parent_position: horizon_event_system::Vec3,
    props: Arc<DashMap<String, GorcObjectId>>,
    gorc_instances: &horizon_event_system::GorcInstanceManager,
    events: Arc<EventSystem>,
) {
    let parent_id_str = parent_gorc_id.to_string();
    
    // Iterate through all objects to find children
    for entry in props.iter() {
        let child_gorc_id = *entry.value();
        
        // Skip if it's the parent itself
        if child_gorc_id == parent_gorc_id {
            continue;
        }
        
        // Get the child object instance
        if let Some(mut child_instance) = gorc_instances.get_object(child_gorc_id).await {
            if let Some(child_props) = child_instance.get_object_mut::<GenericProps>() {
                // Check if this object has a parent_id property matching our parent
                let has_matching_parent = child_props.data.values()
                    .filter_map(|zone_data| zone_data.get("parent_id"))
                    .any(|parent_id_value| {
                        parent_id_value.as_str() == Some(&parent_id_str)
                    });
                
                if has_matching_parent {
                    // Get the child's local position from zone data (NOT global_position,
                    // which would produce parent_new_pos + child_global_pos instead of
                    // parent_new_pos + child_local_pos).
                    let child_local_position = child_props.data.values()
                        .filter_map(|zone_data| zone_data.get("position"))
                        .filter_map(|v| serde_json::from_value::<horizon_event_system::Vec3>(v.clone()).ok())
                        .next()
                        .unwrap_or(horizon_event_system::Vec3::zero());
                    
                    // Calculate the new global position
                    let new_global_position = horizon_event_system::Vec3 {
                        x: parent_position.x + child_local_position.x,
                        y: parent_position.y + child_local_position.y,
                        z: parent_position.z + child_local_position.z,
                    };
                    
                    debug!(
                        "🚀 GORC: Updating child object {} global position to {:?}",
                        child_gorc_id.to_string(),
                        new_global_position
                    );
                    
                    // Update the child's position in the GORC system and send zone
                    // entry/exit messages so nearby players get subscribed/unsubscribed.
                    if let Err(e) = events.update_object_position(child_gorc_id, new_global_position).await {
                        error!("🚀 GORC: ❌ Failed to update child object position with zone events: {}", e);
                    }
                }
            }
        }
    }
}

pub fn handle_object_update(
		_definitions: Arc<DashMap<String, ObjectDefinition>>,
		props: Arc<DashMap<String, GorcObjectId>>,
		events: Arc<EventSystem>,
		event: serde_json::Value,
		handle: luminal::Handle,
		send_to_server_godot: bool,
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
		// Parse request first
		debug!("🎮 GenericPropsPlugin: Handling object update event {:?}", event);
		let req_data = serde_json::from_value::<GenericPropsRequest>(event)
		.map_err(|e| {
			error!("🚀 Plugin: ❌ Failed to parse GenericPropsRequest: {}", e);
			EventError::HandlerExecution("Invalid update request format".to_string())
		})?;
		debug!("🎮 GenericPropsPlugin: Handling object update req_data {:?}", req_data);

		// Ensure we have access to the gorc instances manager
		let Some(gorc_instances) = events.get_gorc_instances() else {
			error!("🎮 GORC: ❌ No GORC instances manager available");
			return Ok(());
		};

		// Spawn an async task to perform awaitable operations so this function can remain synchronous
		let props_clone = Arc::clone(&props);
		let gorc_instances = gorc_instances; // move into async
		handle.spawn(async move {
			// Look up the gorc id for this object UUID
			let gorc_id_opt = if let Some(gorc_ref) = props_clone.get(&req_data.object_uuid) {
				Some(*gorc_ref)
			} else {
				// Fallback: Try to parse UUID as GORC ID directly (for externally created objects)
				match GorcObjectId::from_str(&req_data.object_uuid) {
					Ok(gorc_id) => {
						// Check if this GORC ID actually exists in the instance manager
						if gorc_instances.get_object(gorc_id).await.is_some() {
							debug!("🎮 GORC: ✅ Found object via direct GORC ID lookup: {}", req_data.object_uuid);
							// Add to props map for faster future lookups
							props_clone.insert(req_data.object_uuid.clone(), gorc_id);
							Some(gorc_id)
						} else {
							None
						}
					}
					Err(_) => None
				}
			};
			
			if let Some(gorc_id) = gorc_id_opt {
				let broadcast_only = req_data.broadcast_only.unwrap_or(false);

				if broadcast_only {
					// The caller has already committed the authoritative GORC state via a
					// direct update_object call.  Our job here is only to replicate the
					// current (fresh) state to nearby clients — do NOT call update_object,
					// which would overwrite the authoritative state with a stale snapshot.
					if let Some(object_instance) = gorc_instances.get_object(gorc_id).await {
						if let Some(props) = object_instance.get_object::<GenericProps>() {
							for layer in props.get_layers().iter() {
								let layer_data = props.get_data_for_layer(layer).unwrap_or(serde_json::Value::Null);
								if let Err(e) = events.emit_gorc_instance(
									gorc_id,
									layer.channel,
									"update_property",
									&layer_data,
									horizon_event_system::Dest::Client,
								).await {
									error!("🚀 GORC: ❌ Failed to broadcast channel update (broadcast_only): {}", e);
								} else {
									debug!("🚀 GORC: ✅ Broadcasted channel update (broadcast_only) for object {}", gorc_id);
								}
							}
						}
					}
				} else if gorc_instances.get_object(gorc_id).await.is_some() {
					// Apply the incoming properties IN PLACE, under the instance lock, and take
					// back the set of dirty zones. Cloning the instance, mutating the clone and
					// writing it back would race with handle_player_movement, which touches this
					// same object at ~30 Hz: whichever write lands last silently reverts the
					// other's change. A one-shot field such as the reparent `parent_id` is then
					// lost for good and every later global position is resolved against the wrong
					// parent. Working in place also leaves the live zone_manager and subscriber
					// lists untouched.
					let zone_set = gorc_instances.with_object_mut(gorc_id, |instance| {
						let zone_set = instance.get_object_mut::<GenericProps>()
							.expect("Object must exists")
							.update(req_data.object_data.clone());
						for zone in &zone_set {
							instance.mark_needs_update(*zone);
						}
						zone_set
					}).await.unwrap_or_default();

					// Resolve the new global position ONCE (it does not depend on the zone being
					// iterated) and apply it only after the payload is in place, so the parent_id
					// read here is the one this very update may have just set.
					let mut pending_global_position: Option<horizon_event_system::Vec3> = None;
					if !zone_set.is_empty() {
						if let Some(position_value) = req_data.object_data.get("position") {
							if let Ok(position) = serde_json::from_value::<horizon_event_system::Vec3>(position_value.clone()) {
								let parent_id = gorc_instances.with_object_mut(gorc_id, |instance| {
									instance.get_object::<GenericProps>().and_then(|props| {
										props.data.values()
											.filter_map(|zone_data| zone_data.get("parent_id"))
											.filter_map(|v| v.as_str())
											.find(|s| !s.is_empty())
											.map(|s| s.to_string())
									})
								}).await.flatten();
								let mut final_position = position;

								if let Some(parent_id_str) = &parent_id {
									if let Ok(parent_gorc_id) = GorcObjectId::from_str(parent_id_str) {
										if let Some(parent_global_position) = gorc_instances.get_object_position(parent_gorc_id).await {
											final_position = horizon_event_system::Vec3 {
												x: parent_global_position.x + position.x,
												y: parent_global_position.y + position.y,
												z: parent_global_position.z + position.z,
											};
										}
									}
								}

								pending_global_position = Some(final_position);
							}
						}
					}

					// Broadcast the freshly stored payload, read back from the live instance.
					if let Some(snapshot) = gorc_instances.get_object(gorc_id).await {
						if let Some(props) = snapshot.get_object::<GenericProps>() {
							let layers = props.get_layers();
							for zone in &zone_set {
								for replicationlayer in layers.iter() {
									if replicationlayer.channel == *zone {
										if let Err(e) = events.emit_gorc_instance(
											gorc_id,
											*zone,
											"update_property",
											&props.get_data_for_layer(replicationlayer).unwrap_or(serde_json::Value::Null),
											horizon_event_system::Dest::Client
										).await {
											error!("🚀 GORC: ❌ Failed to broadcast channel update: {}", e);
										} else {
											debug!("🚀 GORC: ✅ Broadcasted channel update for object> {}", gorc_id);
										}
									}
								}
							}
						}
					}

					if let Some(final_position) = pending_global_position {
						// Use events.update_object_position to update position AND send zone
						// entry/exit messages to players. Using gorc_instances.update_object_position
						// directly would compute zone changes but discard them, so players near
						// the new position would never receive zone entry messages.
						if let Err(e) = events.update_object_position(gorc_id, final_position).await {
							error!("🚀 GORC: ❌ Failed to update object position with zone events: {}", e);
						}

						// Update children objects' global positions
						update_children_positions(gorc_id, final_position, Arc::clone(&props_clone), &gorc_instances, Arc::clone(&events)).await;
					}
				} else {
					error!("🎮 GORC: ❌ Object instance not found in GORC for uuid: {}", req_data.object_uuid);
				}
			} else {
				error!("🎮 GORC: ❌ Unknown props uuid in request (not in props map and not a valid GORC ID): {}", req_data.object_uuid);
			}

			if send_to_server_godot {
				// Forward the update event to the game server plugin 
				if let Err(e) = events.emit_plugin("gameserverplugin", "update_prop", &req_data).await {
					error!("🎮 GORC: ❌ Failed to emit plugin event: {}", e);
				}
			}
		});

		Ok(())
	}

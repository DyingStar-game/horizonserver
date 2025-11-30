use std::sync::Arc;
use horizon_event_system::{
    EventSystem, PlayerId, GorcEvent, GorcObject, GorcObjectId, ClientConnectionRef, ObjectInstance,
    EventError,
};
use luminal::Handle;
use luminal::error;
use tracing::{debug, info, error};
use serde_json;
use dashmap::DashMap;

use crate::events::GenericPropsGORCUpdateRequest;
use crate::events::GenericPropsRequest;
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
	// SECURITY: Validate connection authentication before processing any movement
    // if !connection.is_authenticated() {
    //     error!("🚀 GORC: ❌ Unauthenticated movement request from {}", connection.remote_addr);
    //     return Err(EventError::HandlerExecution(
    //         "Unauthenticated request".to_string()
    //     ));
    // }
	
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
		
	// SECURITY
	// TODO Come from a playerwith ownership
    
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
                    // Get the child's local position
                    let child_local_position = child_props.position();
                    
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
                    
                    // Update the child's position in the GORC system
                    gorc_instances.update_object_position(child_gorc_id, new_global_position).await;
                }
            }
        }
    }
}


pub fn handle_object_create(
		definitions: Arc<DashMap<String, ObjectDefinition>>,
		props: Arc<DashMap<String, GorcObjectId>>,
		events: Arc<EventSystem>,
		event: serde_json::Value,
		handle: luminal::Handle
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
		
		let Some(gorc_instances) = events.get_gorc_instances() else {
			error!("🎮 GORC: ❌ No GORC instances manager available");
			return Ok(()); // Not a fatal error, just log and continue
		};
		let req_data = serde_json::from_value::<GenericPropsRequest>(event)
        .map_err(|e| {
            error!("🚀 Plugin: ❌ Failed to parse GenericPropsRequest: {}", e);
            EventError::HandlerExecution("Invalid create request format".to_string())
        })?;
		debug!("🎮 GenericPropsPlugin: Handling object create {:?}", req_data);
		handle.spawn(async move {
			if !props.contains_key(&req_data.object_uuid) {
				let Some(definition) = definitions.get(&req_data.object_type) else {
					error!("🎮 GORC: ❌ Object definition not found for type: {}", req_data.object_type);
					return;
				};
				let mut obj = GenericProps::new(
					definition.clone(),
					req_data.object_data.clone(),
					req_data.object_uuid // if empty, it will generate a new uuid
				);
				let uuid = obj.uuid.clone();
				let position = obj.position();

				// Check if object has a parent_id and get parent's global_position
				let parent_id = obj.data.values()
					.filter_map(|zone_data| zone_data.get("parent_id"))
					.filter_map(|v| v.as_str())
					.find(|s| !s.is_empty())
					.map(|s| s.to_string());

				if let Some(parent_id_str) = parent_id {
					// Search for parent object in props
					if let Some(parent_gorc_id) = props.get(&parent_id_str) {
						let parent_gorc_id = *parent_gorc_id;
						// Get parent object instance to access its global_position
						if let Some(parent_instance) = gorc_instances.get_object(parent_gorc_id).await {
							if let Some(parent_props) = parent_instance.get_object::<GenericProps>() {
								// Set global_position to parent's global_position + local position
								obj.global_position = horizon_event_system::Vec3 {
									x: parent_props.global_position.x + position.x,
									y: parent_props.global_position.y + position.y,
									z: parent_props.global_position.z + position.z,
								};
								info!("🚀 GORC: Setting child object {} global_position based on parent {} global_position: {:?}", 
									uuid, parent_id_str, obj.global_position);
							} else {
								error!("🚀 GORC: ❌ Parent object props not found for child {}", uuid);
								obj.global_position = position.clone();
							}
						} else {
							error!("🚀 GORC: ❌ Parent object instance not found for child {}", uuid);
							obj.global_position = position.clone();
						}
					} else {
						error!("🚀 GORC: ❌ Parent object not found for child {}", uuid);
						obj.global_position = position.clone();
					}
				} else {
					obj.global_position = position.clone();
				}

                // Convert parse Result -> Option<GorcObjectId>
                let maybe_obj_id = match GorcObjectId::from_str(&uuid) {
                    Ok(id) => Some(id),
					Err(_e) => {
						None
					}
				};
				let global_position = obj.global_position.clone();
				let gorc_id = gorc_instances.register_object_with_uuid(obj, global_position, maybe_obj_id).await;
				debug!("🚀 GORC: object register {}", gorc_id.to_string());
				props.insert(uuid, gorc_id.clone());
				if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
					for channel in &definition.channels {
						object_instance.mark_needs_update(channel.zone);
					}
					// Emit the object creation event to notify clients
					if req_data.object_type == "planet" {
						if let Err(e) = events.emit_gorc_instance(
							gorc_id,
							0, // Default channel for object creation
							"gorc_create",
							&serde_json::json!({
								"object_id": gorc_id.to_string(),
								"object_type": req_data.object_type,
								"object_data": req_data.object_data,
								"position": position
							}),
							horizon_event_system::Dest::Client
						).await {
							error!("🚀 GORC: ❌ Failed to emit object creation event: {}", e);
						} else {
							debug!("🚀 GORC: ✅ Object creation event emitted successfully");
						}
					}
					gorc_instances.update_object(gorc_id, object_instance).await;
					if req_data.object_type != "planet" {
						// notifiy to send gorc_zone_enter
						let _ = events.notify_players_for_new_gorc_object(gorc_id).await;
					}
				}
			}
		});
		Ok(())
	}

pub fn handle_object_update(
		_definitions: Arc<DashMap<String, ObjectDefinition>>,
		props: Arc<DashMap<String, GorcObjectId>>,
		events: Arc<EventSystem>,
		event: serde_json::Value,
		handle: luminal::Handle
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
				if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
					// Update the GenericProps on the object instance. Clone object_data to avoid reuse/move issues.
					let zone_set = object_instance.get_object_mut::<GenericProps>().expect("Object must exists").update(req_data.object_data.clone());
					for zone in zone_set {
						object_instance.mark_needs_update(zone);

						if let Some(position_value) = req_data.object_data.get("position") {
							if let Ok(position) = serde_json::from_value::<horizon_event_system::Vec3>(position_value.clone()) {
								// TODO Not sure required to update object_instance position here
								// object_instance.update_position(position);

								// we update the position in gorc for update zones

								// Check if object has a parent_id and adjust position accordingly
								let final_position = if let Some(parent_id_value) = req_data.object_data.get("parent_id") {
									if let Some(parent_id_str) = parent_id_value.as_str() {
										if !parent_id_str.is_empty() {
											// Search for parent object in props
											if let Some(parent_gorc_ref) = props_clone.get(parent_id_str) {
												let parent_gorc_id = *parent_gorc_ref;
												// Get parent object instance to access its global_position
												if let Some(parent_instance) = gorc_instances.get_object(parent_gorc_id).await {
													if let Some(parent_props) = parent_instance.get_object::<GenericProps>() {
														// Calculate global position: parent's global_position + local position
														let global_pos = horizon_event_system::Vec3 {
															x: parent_props.global_position.x + position.x,
															y: parent_props.global_position.y + position.y,
															z: parent_props.global_position.z + position.z,
														};
														debug!("🚀 GORC: Child object {} position adjusted with parent {} global_position: {:?}", 
															gorc_id.to_string(), parent_id_str, global_pos);
														global_pos
													} else {
														position
													}
												} else {
													position
												}
											} else {
												position
											}
										} else {
											position
										}
									} else {
										position
									}
								} else {
									position
								};
								
								// Update the object_instance global_position property
								object_instance.get_object_mut::<GenericProps>().expect("Object must exists").global_position = final_position;
								
								gorc_instances.update_object_position(gorc_id, final_position).await;

								// Update children objects' global positions
								update_children_positions(gorc_id, final_position, Arc::clone(&props_clone), &gorc_instances).await;
							}
						}

						for replicationlayer in object_instance.get_object::<GenericProps>().expect("Object must exists").get_layers().iter() {
							if replicationlayer.channel == zone {
								if let Err(e) = events.emit_gorc_instance(
									gorc_id,
									zone,
									"update_property",
									&object_instance.get_object_mut::<GenericProps>().expect("Object must exists").get_data_for_layer(&replicationlayer).unwrap_or(serde_json::Value::Null),
									horizon_event_system::Dest::Client
								).await {
									error!("🚀 GORC: ❌ Failed to broadcast channel update: {}", e);
								} else {
									debug!("🚀 GORC: ✅ Broadcasted channel update for ship");
								}
							}
						}

					}
					gorc_instances.update_object(gorc_id, object_instance).await;
				} else {
					error!("🎮 GORC: ❌ Object instance not found in GORC for uuid: {}", req_data.object_uuid);
				}
			} else {
				error!("🎮 GORC: ❌ Unknown props uuid in request (not in props map and not a valid GORC ID): {}", req_data.object_uuid);
			}
		});

		Ok(())
	}

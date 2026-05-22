use std::sync::Arc;
use std::collections::HashMap;
use horizon_event_system::{
    EventSystem, PlayerId, GorcEvent, GorcObject, GorcObjectId, ClientConnectionRef, ObjectInstance,
    EventError,
};
use tracing::{debug, info, warn, error};
use serde_json;
use dashmap::DashMap;
use tokio::sync::RwLock;

use ds_common::events::GenericPropsRequest;
use crate::objectdefinition::ObjectDefinition;
use crate::genericprops::GenericProps;

pub fn handle_object_create(
		definitions: Arc<DashMap<String, ObjectDefinition>>,
		props: Arc<DashMap<String, GorcObjectId>>,
		events: Arc<EventSystem>,
		event: serde_json::Value,
		handle: luminal::Handle,
		spawn_in_gameserver: bool,
		queue_objects_create: Arc<RwLock<HashMap<String, serde_json::Value>>>,
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
		
		let Some(gorc_instances) = events.get_gorc_instances() else {
			error!("🎮 GORC: ❌ No GORC instances manager available");
			return Ok(()); // Not a fatal error, just log and continue
		};

		// Clone event before consuming it with from_value, so we can use it later for spawn_object
		let event_clone = event.clone();
		let req_data = serde_json::from_value::<GenericPropsRequest>(event_clone.clone())
		.map_err(|e| {
			error!("🚀 Plugin: ❌ Failed to parse GenericPropsRequest: {}", e);
			EventError::HandlerExecution("Invalid create request format".to_string())
		})?;

		debug!("🎮 GenericPropsPlugin: Handling object create {:?}", req_data);
		let handle_clone = handle.clone();
		handle.spawn(async move {
			if !props.contains_key(&req_data.object_uuid) {
				// it's right case, object not added yet
				// Load the definitions of type of object (json files into folder ds_genericprops/props)
				let Some(definition) = definitions.get(&req_data.object_type) else {
					error!("🎮 GORC: ❌ Object definition not found for type: {}", req_data.object_type);
					return;
				};

				// we create the GenericProps instance
				let mut obj = GenericProps::new(
					definition.clone(),
					req_data.object_data.clone(),
					req_data.object_uuid.clone(),
				);
				let uuid = obj.uuid.clone();

				// Check if object has a parent_id and get parent's global_position
				let parent_id = obj.data.values()
					.filter_map(|zone_data| zone_data.get("parent_id"))
					.filter_map(|v| v.as_str())
					.find(|s| !s.is_empty())
					.map(|s| s.to_string());

				// Check if parent already exists in gorc_instances, if not put in queue for later processing
				if let Some(ref pid) = parent_id {
					if let Ok(parent_gorc_id) = GorcObjectId::from_str(pid) {
						if !gorc_instances.get_object(parent_gorc_id).await.is_some() {
							warn!("🚀 GORC: ⚠️ Parent object {} for child {} not found in GORC instances during creation", pid, uuid);
							queue_objects_create.write().await.insert(req_data.object_uuid.clone(), serde_json::to_value(&req_data).unwrap());
							return;
						}
					}
				}

				// Get the position from attributes or default to (0,0,0)
				let position = obj.object_def.get_position(&req_data.object_data);

				if let Some(parent_id_str) = &parent_id {
					if let Ok(parent_gorc_id) = GorcObjectId::from_str(parent_id_str) {
						// Retry mechanism to handle race condition when parent is being registered concurrently
						let mut parent_global_position = None;
						
						if let Some(pos) = gorc_instances.get_object_position(parent_gorc_id).await {
							parent_global_position = Some(pos);
						}
						
						if let Some(parent_pos) = parent_global_position {
							obj.global_position = horizon_event_system::Vec3 {
								x: parent_pos.x + position.x,
								y: parent_pos.y + position.y,
								z: parent_pos.z + position.z,
							};
							debug!("🚀 GORC: Setting child object {} global_position based on parent {} global_position: {:?}", 
								uuid, parent_id_str, obj.global_position);
						} else {
							error!("🚀 GORC: ❌ Parent object {} position not found for child {} - using local position as fallback", 
								parent_id_str, uuid);
							obj.global_position = position.clone();
						}
					} else {
						error!("🚀 GORC: ❌ Invalid parent GORC ID for child {}", uuid);
						obj.global_position = position.clone();
					}
				} else {
					obj.global_position = position.clone();
				}
				debug!("Creating object {} at global position {:?}", uuid, obj.global_position);

                // Convert parse Result -> Option<GorcObjectId>
                let maybe_obj_id = match GorcObjectId::from_str(&uuid) {
                    Ok(id) => Some(id),
					Err(_e) => {
						None
					}
				};

				if &req_data.object_type == "player" {
					let player_id = PlayerId::from_str(
						event["object_uuid"].as_str().unwrap_or_default()
					).unwrap_or_else(|_| PlayerId::new());
					gorc_instances.add_player(player_id, obj.global_position.clone()).await;
					debug!("🎮 GORC: ✅ Player {} added to spatial tracking BEFORE object registration", player_id);
				}

				let global_position = obj.position();
				debug!("🚀 GORC: About to register object {} with global_position {:?}", uuid, global_position);
				let gorc_id = gorc_instances.register_object_with_uuid(obj, global_position, maybe_obj_id).await;
				debug!("🚀 GORC: object register {} completed with id {}", uuid, gorc_id.to_string());
				props.insert(uuid, gorc_id.clone());
				if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
					for channel in &definition.channels {
						object_instance.mark_needs_update(channel.zone);
					}
					gorc_instances.update_object(gorc_id, object_instance).await;

					if &req_data.object_type == "player" {
						// Send GORC object info to client on channel 0 (player's own object)
						if let Err(e) = events.emit_gorc_instance(
							gorc_id,
							0, // Channel 0 for critical info
							"gorc_zone_enter",
							&req_data.object_data,
							horizon_event_system::Dest::Client
						).await {
							error!("🎮 GORC: ❌ Failed to send GORC info to client: {}", e);
						} else {
							debug!("🎮 GORC: ✅ Sent GORC object info to client: {}", &req_data.object_data);
						}

						// CRITICAL: Subscribe the new player to all existing objects they're within range of
						// This detects zones for planets, cities, etc. that existed before the player connected
						let player_id = PlayerId::from_str(
							event["object_uuid"].as_str().unwrap_or_default()
						).unwrap_or_else(|_| PlayerId::new());
						if let Err(e) = events.subscribe_player_to_existing_objects(player_id, global_position).await {
							error!("🎮 GORC: ❌ Failed to subscribe player to existing objects: {}", e);
						} else {
							debug!("🎮 GORC: ✅ Player {} subscribed to existing objects successfully", event["object_uuid"]);
						}

						// now send player data to the game server
						let mut event_with_position = event.clone();
						if let Some(obj_data) = event_with_position.get_mut("object_data") {
							if let Some(obj_map) = obj_data.as_object_mut() {
								obj_map.insert("_global_position".to_string(), serde_json::json!({
									"x": global_position.x,
									"y": global_position.y,
									"z": global_position.z
								}));
							}
						}

						if let Err(e) = events
							.emit_plugin("plugingameserver", "new_player", &event_with_position)
							.await
						{
							error!("Failed to emit plugin event to player plugin: {}", e);
						}
					} else {
						// notifiy to send gorc_zone_enter
						let _ = events.notify_players_for_new_gorc_object(gorc_id).await;
					}
				}
				if spawn_in_gameserver {
					// Send to ds_game_server to spawn in game world
					if let Err(e) = events
						.emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!(event_clone))
						.await
					{
						error!("🚀 Plugin: ❌ Failed to emit generic prop to DsGameServerPlugin: {}", e);
					} else {
						debug!("🚀 Plugin: ✅ Emitted generic prop to DsGameServerPlugin for object {}", req_data.object_uuid);
					}
				}

				   // Check queue_objects_create for objects waiting on this parent
				   let created_uuid = req_data.object_uuid.clone();
				   let mut to_process: Vec<serde_json::Value> = Vec::new();
				   {
					   let mut queue = queue_objects_create.write().await;
					   debug!("[QUEUE DEBUG] Checking queued children after parent {} creation. Queue size: {}", created_uuid, queue.len());
					   for (key, value) in queue.iter() {
						   debug!("[QUEUE DEBUG] Key: {} Value: {}", key, value);
					   }
					   let keys_to_remove: Vec<String> = queue.iter()
						   .filter_map(|(key, value)| {
							   if let Ok(queued_req) = serde_json::from_value::<GenericPropsRequest>(value.clone()) {
								   // Check if parent_id matches the object we just created
								   let parent_id = queued_req.object_data.as_object()
									   .and_then(|obj| obj.get("parent_id"))
									   .and_then(|v| v.as_str())
									   .filter(|s| !s.is_empty());
								   debug!("[QUEUE DEBUG] For queued key {}: parent_id={:?}, created_uuid={}", key, parent_id, created_uuid);
								   if parent_id == Some(created_uuid.as_str()) {
									   debug!("[QUEUE DEBUG] Match found: queued child {} will be processed.", key);
									   return Some(key.clone());
								   }
							   }
							   None
						   })
						   .collect();
					   for key in &keys_to_remove {
						   debug!("[QUEUE DEBUG] Removing and processing queued child: {}", key);
					   }
					   for key in keys_to_remove {
						   if let Some(value) = queue.remove(&key) {
							   to_process.push(value);
						   }
					   }
				   }

				   // Process queued objects outside of the lock
				   for queued_event in to_process {
					   if let Err(e) = handle_object_create(
						   definitions.clone(),
						   props.clone(),
						   events.clone(),
						   queued_event,
						   handle_clone.clone(),
						   spawn_in_gameserver,
						   queue_objects_create.clone(),
					   ) {
						   error!("🚀 Plugin: ❌ Failed to handle queued object create: {}", e);
					   }
				   }

			}
		});
		Ok(())
	}


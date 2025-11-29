use std::sync::Arc;
use horizon_event_system::{
    EventSystem, PlayerId, GorcEvent, GorcObject, GorcObjectId, ClientConnectionRef, ObjectInstance,
    EventError,
};
use luminal::Handle;
use tracing::{debug, error};
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
				let obj = GenericProps::new(
					definition.clone(),
					req_data.object_data.clone(),
					req_data.object_uuid // if empty, it will generate a new uuid
				);
				let uuid = obj.uuid.clone();
				let position = obj.position();
                // Convert parse Result -> Option<GorcObjectId>
                let maybe_obj_id = match GorcObjectId::from_str(&uuid) {
                    Ok(id) => Some(id),
                    Err(_e) => {
                        None
                    }
                };
				let gorc_id = gorc_instances.register_object_with_uuid(obj, position.clone(), maybe_obj_id).await;
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
			if let Some(gorc_ref) = props_clone.get(&req_data.object_uuid) {
				let gorc_id = *gorc_ref; // GorcObjectId appears to be Copy in other code
				if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
					// Update the GenericProps on the object instance. Clone object_data to avoid reuse/move issues.
					let zone_set = object_instance.get_object_mut::<GenericProps>().expect("Object must exists").update(req_data.object_data.clone());
					for zone in zone_set {
						object_instance.mark_needs_update(zone);

						if let Some(position_value) = req_data.object_data.get("position") {
							if let Ok(position) = serde_json::from_value(position_value.clone()) {
								// TODO Not sure required to update object_instance position here
								// object_instance.update_position(position);

								// we update the position in gorc for update zones 
								gorc_instances.update_object_position(gorc_id, position).await;
								// TODO get children objects and update their position too in 'global position'
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
					error!("🎮 GORC: ❌ Invalid props uuid in request: {}", req_data.object_uuid);
				}
			} else {
				error!("🎮 GORC: ❌ Unknown props uuid in request: {}", req_data.object_uuid);
			}
		});

		Ok(())
	}

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
    client_player: PlayerId,
    connection: ClientConnectionRef,
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


pub fn handle_object_update(
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
            EventError::HandlerExecution("Invalid update request format".to_string())
        })?;
		handle.spawn(async move {
			if !props.contains_key(&req_data.object_uuid) {
				let definition = definitions.get(&req_data.object_type).unwrap();
				let obj = GenericProps::new(
					definition.clone(),
					req_data.object_data,
					req_data.object_uuid // if empty, it will generate a new uuid
				);
				let uuid = obj.uuid.clone();
				let position = obj.position();
				let gorc_id = gorc_instances.register_object(obj, position.clone()).await;
				debug!("🚀 GORC: object register {}", gorc_id.to_string());
				props.insert(uuid, gorc_id.clone());
				if let Some(mut object_instance) = gorc_instances.get_object(gorc_id).await {
					for channel in &definition.channels {
						object_instance.mark_needs_update(channel.zone);
					}
					//object_instance.update_position(position)
				}
			}
			//update
			else {
				let gorcid = props.get(&req_data.object_uuid).unwrap();
				if let Some(mut object_instance) = gorc_instances.get_object(*gorcid).await {
					let zone_set = object_instance.get_object_mut::<GenericProps>().expect("TODO").update(req_data.object_data);
					for zone in zone_set {
						object_instance.mark_needs_update(zone);
					}
					gorc_instances.update_object(*gorcid, object_instance).await;

				}
				else {
					EventError::HandlerExecution("Invalid props uuid in request".to_string());
				}
				//update as ref + mark_needs_update could be better than this kind of clone+replace update method
			}
		});
		Ok(())
	}
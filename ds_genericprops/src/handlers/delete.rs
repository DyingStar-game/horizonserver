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
use ds_common::events::GenericPropsRequest;
use crate::objectdefinition::ObjectDefinition;
use crate::genericprops::GenericProps;

pub fn handle_object_delete(
		_definitions: Arc<DashMap<String, ObjectDefinition>>,
		props: Arc<DashMap<String, GorcObjectId>>,
		events: Arc<EventSystem>,
		event: serde_json::Value,
		handle: luminal::Handle
	) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
		// Parse request first
		debug!("🎮 GenericPropsPlugin: Handling object delete event {:?}", event);
		let req_data = serde_json::from_value::<GenericPropsRequest>(event)
		.map_err(|e| {
			error!("🚀 Plugin: ❌ Failed to parse GenericPropsRequest: {}", e);
			EventError::HandlerExecution("Invalid delete request format".to_string())
		})?;
		debug!("🎮 GenericPropsPlugin: Handling object delete req_data {:?}", req_data);

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
				// Remove from props map
				props_clone.remove(&req_data.object_uuid);
				// Remove the object instance from GORC
				if let Err(e) = events.emit_gorc_instance(
					gorc_id,
					6,
					"gorc_zone_exit",
					&serde_json::json!({}),
					horizon_event_system::Dest::Client
				).await {
					error!("🎮 GORC: ❌ Failed to send GORC info to client: {}", e);
				} else {
					println!("🎮 GORC: ✅ Sent GORC zone exit info to client");
				}

				if gorc_instances.unregister_object(gorc_id).await {
					info!("🎮 GORC: ✅ Deleted object instance from GORC: {}", gorc_id);
				} else {
					error!("🎮 GORC: ❌ Failed to delete object instance from GORC: {}", gorc_id);
				}
			} else {
				error!("🎮 GORC: ❌ Unknown props uuid in delete request (not in props map and not a valid GORC ID): {}", req_data.object_uuid);
			}
		});
		Ok(())
	}

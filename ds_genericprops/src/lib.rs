use async_trait::async_trait;
use dashmap::DashMap;
use horizon_event_system::gorc::instance;
use horizon_event_system::{
    create_simple_plugin,
    EventSystem,
    GorcEvent,
    GorcObjectId,
    LogLevel,
    PluginError,
    ServerContext,
    SimplePlugin,
    PlayerDisconnectedEvent,
    Vec3,
};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{ info, debug, error, warn };
use serde::{Deserialize, Serialize};
use std::fs;
// Public modules for external access
pub mod genericprops;
pub mod events;
pub mod objectdefinition;
// Internal imports
mod handlers;
use handlers::*;

use crate::objectdefinition::ObjectDefinition;
use crate::genericprops::GenericProps;
use std::collections::HashMap;
use ds_common::events::GenericPropsRequest;
use serde_json::{json, Value};

pub struct GenericPropsPlugin {
    name: String,
	props: Arc<DashMap<String, GorcObjectId>>,
	definitions: Arc<DashMap<String, ObjectDefinition>>
}

impl GenericPropsPlugin {
    
    pub fn new() -> Self {
        debug!("🎮 GenericPropsPlugin: Creating new instance with GORC architecture");
		
        Self {
            name: "GenericPropsPlugin".to_string(),
            props: Arc::new(DashMap::with_capacity_and_shard_amount(64, 32)),
            definitions: Arc::new(DashMap::with_capacity_and_shard_amount(64, 32)),
        }
    }
	pub fn new_definition(&mut self, name: String, definition_data: serde_json::Value) {
		self.definitions.insert(name.clone(), ObjectDefinition::new(name, definition_data));
	}
}

impl Default for GenericPropsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SimplePlugin for GenericPropsPlugin {

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }
  
    async fn register_handlers(
        &mut self,
        events: Arc<EventSystem>,
        context: Arc<dyn ServerContext>
    ) -> Result<(), PluginError> {
        debug!("🎮 GenericPropsPlugin: Registering comprehensive GORC event handlers...");
        context.log(
            LogLevel::Info,
            "🎮 GenericPropsPlugin: Initializing multi-channel player management system..."
        );

        let luminal_handle = context.luminal_handle();
		//Register core server event handlers for player lifecycle management
        self.register_plugin_handlers(
            Arc::clone(&events),
            luminal_handle.clone(),
            context.clone(),
        ).await?;
		
		self.register_gorc_handler(Arc::clone(&events), luminal_handle.clone(), 0).await?;
		self.register_gorc_handler(Arc::clone(&events), luminal_handle.clone(), 1).await?;
		self.register_gorc_handler(Arc::clone(&events), luminal_handle.clone(), 2).await?;
		self.register_gorc_handler(Arc::clone(&events), luminal_handle.clone(), 3).await?;
        
        context.log(
            LogLevel::Info,
            "🎮 GenericPropsPlugin: ✅ All GORC object handlers registered successfully!"
        );
        Ok(())
    }

  
    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        let config = ds_common::config::Config::new("ds_genericprops");
        // get the log level from plugin.toml config file
        let log_level = config.get_value("log_level")
            .and_then(|v| v.as_str())
            .unwrap_or("info");
       
        // Set up tracing subscriber with the configured level
        let filter_level = match log_level {
            "error" => tracing::Level::ERROR,
            "warn" => tracing::Level::WARN,
            "info" => tracing::Level::INFO,
            "debug" => tracing::Level::DEBUG,
            "trace" => tracing::Level::TRACE,
            _ => tracing::Level::INFO,
        };
        tracing_subscriber::fmt()
            .with_max_level(filter_level)
            .try_init()
            .ok(); // Ignore errors if already initialized

        let props_path = if fs::metadata("props").is_ok() {
            "props/"
        } else {
            "../ds_genericprops/props/"
        };

        if let Ok(directory) = fs::read_dir(props_path) {
			for entry in directory {
				if let Ok(entry) = entry {
					if let Some(name) = entry.file_name().to_str() {
						if name.ends_with("_def.json") {
							let file = fs::File::open(entry.path())
							.expect("file should open read only");
							let json: serde_json::Value = serde_json::from_reader(file)
							.expect("file should be proper JSON");
							self.new_definition(name.get(0..(name.len()-9)).unwrap().into(), json);
							context.log(
								LogLevel::Info,
								"🎮 GenericPropsPlugin: new definition loaded"
							);
						}
					}
				}
			}
		}

        info!("🎮 GenericPropsPlugin: GORC player management system activated and ready!");
        Ok(())
    }

   
    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            &format!(
                "🎮 GenericPropsPlugin: Shutting down gracefully."
            )
        );

        Ok(())
    }
}

// Implementation of individual handler registration methods
impl GenericPropsPlugin {
    // Registers GORC channel handler
	async fn register_plugin_handlers(
        &self,
        events: Arc<EventSystem>,
        luminal_handle: luminal::Handle,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        println!("🎮 GenericPropsPlugin: Registering GORC handler");
		
        let queue_objects_create: Arc<RwLock<HashMap<String, serde_json::Value>>> = Arc::new(RwLock::new(HashMap::new()));

        // Clone for first handler
        let update_events1 = events.clone();
        let handle1 = luminal_handle.clone();
        let definitions1 = Arc::clone(&self.definitions);
        let props1 = Arc::clone(&self.props);

        events.on_plugin("genericprops", "update_object", move |event: serde_json::Value| {
            debug!("plugin genericprops (update): Receive object message {:?}", event);
            if let Err(e) = update::handle_object_update(
                                definitions1.clone(),
                                props1.clone(),
                                update_events1.clone(),
                                event.clone(),
                                handle1.clone()
                            )
                        {
                            error!("🎮 Failed to handle object update: {}", e);
                        }
        Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // Clone for second handler
        let update_events2 = events.clone();
        let handle2 = luminal_handle.clone();
        let definitions2 = Arc::clone(&self.definitions);
        let props2 = Arc::clone(&self.props);
        let queue_objects_create2 = Arc::clone(&queue_objects_create);
        
        events.on_plugin("genericprops", "create_object", move |event: serde_json::Value| {
            debug!("plugin genericprops (create): Receive object message {:?}", event);
            if let Err(e) = create::handle_object_create(
                                definitions2.clone(),
                                props2.clone(),
                                update_events2.clone(),
                                event.clone(),
                                handle2.clone(),
                                true,
                                queue_objects_create2.clone(),
                            )
                        {
                            error!("🎮 Failed to handle object update: {}", e);
                        }
        Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let update_events3 = events.clone();
        let handle3 = luminal_handle.clone();
        let definitions3 = Arc::clone(&self.definitions);
        let props3 = Arc::clone(&self.props);
        let queue_objects_create3 = Arc::clone(&queue_objects_create);

        events.on_plugin("genericprops", "create_object_from_gameserver", move |event: serde_json::Value| {
            debug!("plugin genericprops (create from gameserver): Receive object message {:?}", event);
            if let Err(e) = create::handle_object_create(
                                definitions3.clone(),
                                props3.clone(),
                                update_events3.clone(),
                                event.clone(),
                                handle3.clone(),
                                false,
                                queue_objects_create3.clone(),
                            )
                        {
                            error!("🎮 Failed to handle object update: {}", e);
                        }
        Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let delete_events = events.clone();
        let handle_delete = luminal_handle.clone();
        let definitions_delete = Arc::clone(&self.definitions);
        let props_delete = Arc::clone(&self.props);

        events.on_plugin("genericprops", "delete_object", move |event: serde_json::Value| {
            debug!("plugin genericprops (delete): Receive object message {:?}", event);
            if let Err(e) = delete::handle_object_delete(
                                definitions_delete.clone(),
                                props_delete.clone(),
                                delete_events.clone(),
                                event.clone(),
                                handle_delete.clone()
                            )
                        {
                            error!("🎮 Failed to handle object delete: {}", e);
                        }
        Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let props = Arc::clone(&self.props);
        let gorc_instances = context.events().get_gorc_instances().unwrap();
        let handle_objs_zone = luminal_handle.clone();
        let objs_zone_events = Arc::clone(&events);

        events.on_plugin("genericprops", "get_objects_on_zone", move |event: serde_json::Value| {
            info!("plugin genericprops (get_objects_on_zone): Receive object message {:?}", event);

            let props = Arc::clone(&props);
            let gorc_instances = Arc::clone(&gorc_instances);
            let events = Arc::clone(&objs_zone_events);

            handle_objs_zone.spawn(async move {
                info!("plugin genericprops (get_objects_on_zone): Number props {:?}", props.clone().len());

                let mut items: HashMap<String, GenericPropsRequest> = HashMap::new();

                // TODO: get all items on the zone
                for entry in props.iter() {
                    let prop_uuid = entry.key();
                    let mut prop_properties = HashMap::new();
                    let mut object_type = String::new();
                    if let Some(mut object_instance) = gorc_instances.get_object(GorcObjectId::from_str(prop_uuid).unwrap()).await {
                        if let Some(generic_props) = object_instance.get_object_mut::<GenericProps>() {
                            object_type = generic_props.object_def.name.clone();
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
                            prop_properties.insert("_global_position".to_string(), serde_json::to_value(&generic_props.global_position).unwrap_or(Value::Null));
                        }
                    }
                    debug!("plugin genericprops (get_objects_on_zone): Found prop uuid {:?} with properties {:?}", prop_uuid, prop_properties);
                    items.insert(
                        prop_uuid.to_string(),
                        GenericPropsRequest {
                            object_type: object_type,
                            object_uuid: prop_uuid.to_string(),
                            object_data: serde_json::to_value(&prop_properties).unwrap_or(Value::Null),
                        }
                    );
                }

                // loop on all props and check if zone match
                // then return the list of objects found to the gameserver that requested it
                events.emit_plugin(
                    "gameserver",
                    "initial_objects_on_zone",
                    &json!({
                        "server_uuid": event.get("server_uuid").and_then(|v| v.as_str()).unwrap_or(""),
                        "split_server_uuid": event.get("split_server_uuid").and_then(|v| v.as_str()).unwrap_or(""),
                        "items": items
                    }),
                ).await.unwrap();
            });

        Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


    
        let events_player_move = Arc::clone(&events);
        let props_player_move = Arc::clone(&self.props);
        let handle_player_move = luminal_handle.clone();
        // events
        //     .on_gorc_instance(
        //         // "GorcPlayer",
        //         "GorcGenericProps",
        //         0, // Channel 0: Critical movement data
        //         "move",
        //         move |gorc_event: GorcEvent, object_instance| {
        //             info!("🎮 PlayerPlugin: received GORC channel 0 (movement) event!");                    
        //             // Use the dedicated movement handler
        //             player_movement::handle_movement_request_sync(
        //                 gorc_event,
        //                 object_instance,
        //                 events_player_move.clone(),
        //             )
        //         }
        //     ).await
        //     .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        use std::sync::atomic::{AtomicU64, Ordering};
        static PLAYERMOVE_COUNTER: AtomicU64 = AtomicU64::new(0);
        
        events.on_plugin("genericprops", "playermove", move |event: serde_json::Value| {
            let count = PLAYERMOVE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let player_uuid = event["object_uuid"].as_str().unwrap_or("unknown");
            debug!("🔄 PLAYERMOVE #{}: Received for player {}", count, player_uuid);
            
            let start = std::time::Instant::now();
            let result = player_movement::handle_movement_request_sync(
                event.clone(),
                props_player_move.clone(),
                events_player_move.clone(),
                handle_player_move.clone(),
            );
            let elapsed = start.elapsed();
            
            if elapsed.as_millis() > 10 {
                warn!("🔄 PLAYERMOVE #{}: Slow processing took {:?} for player {}", count, elapsed, player_uuid);
            }
            
            match &result {
                Ok(_) => debug!("🔄 PLAYERMOVE #{}: ✅ Completed in {:?} for player {}", count, elapsed, player_uuid),
                Err(e) => error!("🔄 PLAYERMOVE #{}: ❌ Failed: {} for player {}", count, e, player_uuid),
            }
            
            result.map(|_| ())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let props_player_remove = Arc::clone(&self.props);
        let handle_player_remove = luminal_handle.clone();
        let events_core = Arc::clone(&events);
        events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
            let props_player_remove = Arc::clone(&props_player_remove);
            let events_core = Arc::clone(&events_core);
            let event = event.clone();
            handle_player_remove.spawn(async move {
                // Remove player from props and get their GORC object ID
                if let Some((_, gorc_id)) = props_player_remove.remove(&event.player_id.to_string()) {
                    debug!("🎮 GORC: ✅ Player {} disconnected and unregistered (GORC ID {:?})", 
                        event.player_id, gorc_id);
                } else {
                    // This could happen if the player was never successfully registered
                    error!("🎮 GORC: Player {} disconnected but was not in props", event.player_id);
                }

                // Send GORC object info to client on channel 0
                let gorc_id = match GorcObjectId::from_str(event.player_id.to_string().as_str()) {
                    Ok(id) => id,
                    Err(e) => {
                        error!("🎮 GORC: ❌ Failed to parse player ID as GORC ID: {}", e);
                        return Ok(());
                    }
                };
                if let Err(e) = events_core.emit_gorc_instance(
                    gorc_id,
                    0, // Channel 0 for critical info
                    "gorc_zone_exit",
                    &serde_json::json!({}),
                    horizon_event_system::Dest::Client
                ).await {
                    error!("🎮 GORC: ❌ Failed to send GORC info to client: {}", e);
                } else {
                    println!("🎮 GORC: ✅ Sent GORC zone exit info to client");
                }

                // Verify GORC instances manager is available
                let Some(gorc_instances) = events_core.get_gorc_instances() else {
                    error!("🎮 GORC: ❌ No GORC instances manager available for diconnect player");
                    return Ok(()); // Not a fatal error, just log and continue
                };

                gorc_instances.remove_player(event.player_id).await;

                // TODO create real delete event for servers
                let mut prop_properties = HashMap::new();
                prop_properties.insert("_global_position".to_string(), serde_json::to_value(Vec3::new(1000000000000.0, 1000000000000.0, 1000000000000.0)).unwrap_or(Value::Null));
                let item = GenericPropsRequest  {
                    object_type: "player".to_string(),
                    object_uuid: event.player_id.to_string(),
                    object_data: serde_json::to_value(&prop_properties).unwrap_or(Value::Null),
                };

                // loop on all props and check if zone match
                // then return the list of objects found to the gameserver that requested it
                events_core.emit_plugin(
                    "gameserverplugin",
                    "player_quit",
                    &json!({
                        "item": item,
                    }),
                ).await.unwrap();
                // End of the TODO block

                Ok::<(), PluginError>(())
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // let gorc_instances = context.events().get_gorc_instances().unwrap();
        // let handle_out_of_zone = luminal_handle.clone();
        // let events_out_of_zone = Arc::clone(&events);
        // events.on_plugin("genericprops", "player_out_of_zone", move |event: serde_json::Value| {
        //     info!("plugin genericprops (player_out_of_zone): Receive object message {:?}", event);

        //     let gorc_instances = Arc::clone(&gorc_instances);
        //     let events = Arc::clone(&events_out_of_zone);

        //     handle_out_of_zone.spawn(async move {
        //         let prop_uuid_string = event["object_data"]["player_uuid"].as_str().unwrap_or_default();
        //         let mut prop_properties = HashMap::new();
        //         let mut object_type = String::new();
        //         let global_position = gorc_instances.get_object_position(GorcObjectId::from_str(prop_uuid_string).unwrap()).await;
        //         if let Some(mut object_instance) = gorc_instances.get_object(GorcObjectId::from_str(prop_uuid_string).unwrap()).await {
        //             if let Some(generic_props) = object_instance.get_object_mut::<GenericProps>() {
        //                 object_type = generic_props.object_def.name.clone();
        //                 for properties in generic_props.data.values() {
        //                     match properties {
        //                         Value::Null => warn!("property null"),
        //                         Value::Object(map) => {
        //                             for(key, value) in map.iter() {
        //                                 prop_properties.insert(key.to_string(), value.clone());
        //                             }
        //                         },
        //                         _ => warn!("no properties"),
        //                     }
        //                 }
        //                 prop_properties.insert("_global_position".to_string(), serde_json::to_value(&generic_props.global_position).unwrap_or(Value::Null));
        //             }
        //         }
        //         debug!("plugin genericprops (get_objects_on_zone): Found prop uuid {:?} with properties {:?}", prop_uuid_string, prop_properties);
        //         let item = GenericPropsRequest  {
        //             object_type: object_type,
        //             object_uuid: prop_uuid_string.to_string(),
        //             object_data: serde_json::to_value(&prop_properties).unwrap_or(Value::Null),
        //         };

        //         // loop on all props and check if zone match
        //         // then return the list of objects found to the gameserver that requested it
        //         events.emit_plugin(
        //             "gameserverplugin",
        //             "player_out_of_zone",
        //             &json!({
        //                 "server_uuid": event["object_data"]["server_uuid"].as_str().unwrap_or_default(),
        //                 "item": item,
        //                 "global_position": global_position,
        //             }),
        //         ).await.unwrap();
        //     });

        //     Ok(())
        // }).await
        // .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        debug!("🎮 GenericPropsPlugin: handler registered");
        Ok(())
    }
							
	async fn register_gorc_handler(
        &self,
        events: Arc<EventSystem>,
        luminal_handle: luminal::Handle,
		channel: u8
    ) -> Result<(), PluginError> {
        debug!("🎮 GenericPropsPlugin: Registering GORC channel {} (movement) handler", channel);
		
		let events_update = Arc::clone(&events);
        let luminal_handle = luminal_handle.clone();
        events
            .on_gorc_client(
                luminal_handle.clone(),
                "GorcGenericProps",
                channel,
                "update",
                move |gorc_event, client_player, connection, object_instance| {
                    // Use the dedicated movement handler
                    update::handle_client_update_request(
                        gorc_event,
                        client_player,
                        connection,
                        object_instance,
                        events_update.clone(),
                        luminal_handle.clone()
                    )
                }
            ).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


		debug!("🎮 GenericPropsPlugin: GORC channel {} handler registered", channel);
        Ok(())
    }
	
}

// Create the plugin using our macro - zero unsafe code!
create_simple_plugin!(GenericPropsPlugin);

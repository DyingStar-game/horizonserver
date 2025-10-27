use async_trait::async_trait;
use dashmap::DashMap;
use horizon_event_system::{
    create_simple_plugin,
    EventSystem,
    GorcObjectId,
    LogLevel,
    //PlayerId,
    PluginError,
    ServerContext,
    SimplePlugin,
};
use std::sync::Arc;
use tracing::{ debug, error };
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
			props: Arc::new(DashMap::new()),
			definitions: Arc::new(DashMap::new()),
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
            //context.clone(),
            luminal_handle.clone()
        ).await?;
		
        //TODO? Register GORC client event handlers if any
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
		if let Ok(directory) = fs::read_dir("./") {
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
		
        context.log(
            LogLevel::Info,
            "🎮 GenericPropsPlugin: GORC player management system activated and ready!"
        );
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
        luminal_handle: luminal::Handle
    ) -> Result<(), PluginError> {
        debug!("🎮 GenericPropsPlugin: Registering GORC  handler");
		
		//do creation or update in the same event
		let update_events = events.clone();
		let handle2 = luminal_handle.clone();
		let definitions = Arc::clone(&self.definitions);
		let props = Arc::clone(&self.props);
		//events.on_plugin("genericprops", "object", move |event: serde_json::Value| {
		events.on_client("genericprops", "object", move |event: serde_json::Value, _player_id: horizon_event_system::PlayerId, _connection: horizon_event_system::ClientConnectionRef| {
			println!("plugin genericprops: Receive object message {:?}", event);
			if let Err(e) = update::handle_object_update(
								definitions.clone(),
								props.clone(),
								update_events.clone(),
                                event.get("data").unwrap().clone(),
                                handle2.clone()
                            )
                        {
                            error!("🎮 Failed to handle object update: {}", e);
                        }
		 Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
			
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

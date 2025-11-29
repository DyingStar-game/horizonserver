use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, current_timestamp, EventSystem, LogLevel,
    PluginError, ServerContext, SimplePlugin,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn, debug};

use websocket::ClientBuilder;
use websocket::r#async::client::{Client, ClientNew, Framed};
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
use websocket::result::WebSocketError;

/// DyingstarServices Plugin
pub struct DyingstarServicesPlugin {
    name: String,
    socket_url: String,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
}

impl DyingstarServicesPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarServicesPlugin: Creating new instance");
        Self {
            name: "dyingstar_services".to_string(),
            socket_url: "ws://localhost:9200".to_string(),
            websocket: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl SimplePlugin for DyingstarServicesPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn register_handlers(
        &mut self,
        events: Arc<EventSystem>,
        _context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        info!("🔧 DyingstarServicesPlugin: Registering event handlers...");
        
        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);

        events.on_plugin(
            "externalservices",
            "resourcesdynamic",
            move |event: serde_json::Value|
        {
            let url = url.clone();
            let websocket = Arc::clone(&websocket);
            debug!("🔧 DyingstarServicesPlugin: Handling 'resourcesdynamic' event: {:?}", event);
            let message = serde_json::to_string(&event).unwrap_or_else(|e| {
                error!("🔧 DyingstarServicesPlugin: ❌ Failed to serialize event to JSON string: {}", e);
                "{}".to_string()
            });
            debug!("🔧 DyingstarServicesPlugin: Sending message to external service: {}", message);
            match websocket.lock() {
                Ok(mut ws_guard) => {
                    if let Some(sender) = ws_guard.as_mut() {
                        match sender.send_message(&OwnedMessage::Text(message.clone())) {
                            Ok(_) => debug!("🔧 DyingstarServicesPlugin: ✅ Sent message to external service"),
                            Err(e) => error!("🔧 DyingstarServicesPlugin: ❌ Failed to send message: {}", e),
                        }
                    } else {
                        warn!("🔧 DyingstarServicesPlugin: ⚠️ WebSocket sender not initialized");
                    }
                }
                Err(e) => error!("🔧 DyingstarServicesPlugin: ❌ Failed to lock WebSocket mutex: {}", e),
            }
            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        
        info!("🔧 DyingstarServicesPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        // Get the log level from ServerContext
        let log_level = context.log_level();
        
        // Set up tracing subscriber with the configured level
        let filter_level = match log_level {
            LogLevel::Error => tracing::Level::ERROR,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Trace => tracing::Level::TRACE,
        };
        tracing_subscriber::fmt()
            .with_max_level(filter_level)
            .try_init()
            .ok(); // Ignore errors if already initialized


        // Init websocket connection
        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);
        std::thread::spawn(move || {
            // Connect and store writer
            info!("Attempting to connect to external service at {}", url);
            let mut builder = match ClientBuilder::new(&url) {
                Ok(b) => b,
                Err(e) => {
                    error!("Failed to create WebSocket client builder for {}: {:?}", url, e);
                    warn!("External service connection will be unavailable. Plugin will continue without it.");
                    return;
                }
            };
            
            let socket = match builder.connect_insecure() {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to external service at {}: {:?}", url, e);
                    warn!("External service connection will be unavailable. Plugin will continue without it.");
                    return;
                }
            };
            
            let (mut receiver, sender) = match socket.split() {
                Ok(split) => split,
                Err(e) => {
                    error!("Failed to split WebSocket: {:?}", e);
                    return;
                }
            };
            
            *websocket.lock().unwrap() = Some(sender);

            // local runtime to call async event system from this blocking thread
            let rt = tokio::runtime::Runtime::new().unwrap();

            info!("WebSocket reader thread started");
            for msg in receiver.incoming_messages() {
                match msg {
                    Ok(OwnedMessage::Text(s)) => {
                        debug!("[message][from][services]: {}", s);
                        
                        // Parse the JSON message
                        match serde_json::from_str::<serde_json::Value>(&s) {
                            Ok(json) => {
                                // Extract the data array
                                if let Some(data_array) = json.get("data").and_then(|d| d.as_array()) {
                                    debug!("Received {} objects from external service", data_array.len());

                                    // Spawn async task to emit events
                                    let context_clone = context.clone();
                                    let data_array_clone = data_array.clone();
                                    let _ = rt.block_on(async move {
                                        // Loop through each object in the data array
                                        for (index, obj) in data_array_clone.iter().enumerate() {
                                            if let Some(object_type) = obj.get("object_type").and_then(|t| t.as_str()) {
                                                let mut modified_obj = obj.clone();
                                                if object_type == "planet" {
                                                    debug!("Emitting planet object to genericprops...");
                                                    if let Some(scenename) = modified_obj["object_data"]["scenename"].as_str() {
                                                        modified_obj["object_data"]["scenename"] = scenename.replace("scenes/planet/", "scenes/systems/tarsis/").into();
                                                    }

                                                    if let Err(e) = context_clone.events().emit_plugin("genericprops", "create_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }

                                                    // to gameserver
                                                    debug!("Emitting planet object to gameserver...");
                                                    if let Err(e) = context_clone.events().emit_plugin("gameserverplugin", "spawn_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }
                                                }
                                                else if object_type == "moon" {
                                                    debug!("Emitting moon object to genericprops...");
                                                    modified_obj["object_type"] = "planet".into();
                                                    if let Some(scenename) = modified_obj["object_data"]["scenename"].as_str() {
                                                        modified_obj["object_data"]["scenename"] = scenename.replace("scenes/moon/", "scenes/systems/tarsis/").into();
                                                    }
                                                    
                                                    if let Err(e) = context_clone.events().emit_plugin("genericprops", "create_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }

                                                    // to gameserver
                                                    debug!("Emitting moon object to gameserver...");
                                                    if let Err(e) = context_clone.events().emit_plugin("gameserverplugin", "spawn_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }
                                                } else if object_type == "star" {
                                                    debug!("Emitting star object to genericprops...");
                                                    if let Some(scenename) = modified_obj["object_data"]["scenename"].as_str() {
                                                        modified_obj["object_data"]["scenename"] = scenename.replace("scenes/systems/tarsis/tarsis.tscn", "scenes/star/star.tscn").into();
                                                    }
                                                    if let Some(parent_id) = modified_obj["object_data"]["parent_id"].as_str() {
                                                        modified_obj["object_data"]["parent_id"] = "".into();
                                                    }
                                                    modified_obj["object_data"]["position"] = serde_json::json!({
                                                        "x": 0.0,
                                                        "y": 0.0,
                                                        "z": 0.0
                                                    });
                                                    
                                                    if let Err(e) = context_clone.events().emit_plugin("genericprops", "create_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }

                                                    // to gameserver
                                                    debug!("Emitting star object to gameserver...");
                                                    if let Err(e) = context_clone.events().emit_plugin("gameserverplugin", "spawn_object", &modified_obj).await {
                                                        error!("Failed to emit plugin event: {}", e);
                                                    }
                                                } else {
                                                    warn!("Unknown object_type '{}' at index {}", object_type, index);
                                                    
                                                }
                                            }
                                        }
                                    });
                                } else {
                                    warn!("No 'data' array found in message");
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse JSON message: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("WebSocket error: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }
            info!("WebSocket reader thread stopped");
        });

        
        info!("🔧 DsGameServerPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarServicesPlugin: Shutting down!",
        );

        // TODO: Add your cleanup logic here

        info!("🔧 DyingstarServicesPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DyingstarServicesPlugin);

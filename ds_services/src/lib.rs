use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, current_timestamp, EventSystem, LogLevel,
    PluginError, RegionStartedEvent, ServerContext, SimplePlugin,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn, debug};

/// Round a f64 value to 3 decimal places
fn round_to_3_decimals(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Round all numeric values in positions and rotations arrays to 3 decimal places
fn round_positions_rotations(obj: &mut serde_json::Value) {
    if let Some(object_data) = obj.get_mut("object_data") {
        // Round positions
        if let Some(positions) = object_data.get_mut("positions").and_then(|p| p.as_array_mut()) {
            for pos in positions.iter_mut() {
                for key in ["x", "y", "z"] {
                    if let Some(val) = pos.get_mut(key).and_then(|v| v.as_f64()) {
                        pos[key] = serde_json::json!(round_to_3_decimals(val));
                    }
                }
            }
        }
        // Round rotations
        if let Some(rotations) = object_data.get_mut("rotations").and_then(|r| r.as_array_mut()) {
            for rot in rotations.iter_mut() {
                for key in ["w", "x", "y", "z"] {
                    if let Some(val) = rot.get_mut(key).and_then(|v| v.as_f64()) {
                        rot[key] = serde_json::json!(round_to_3_decimals(val));
                    }
                }
            }
        }
    }
}

use websocket::ClientBuilder;
use websocket::r#async::client::{Client, ClientNew, Framed};
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
use websocket::result::WebSocketError;
use std::fs;
use std::path::Path;

/// DyingstarServices Plugin
pub struct DyingstarServicesPlugin {
    name: String,
    socket_url: String,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
    can_query_service: Arc<Mutex<bool>>,
}

impl DyingstarServicesPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarServicesPlugin: Creating new instance");

        // Read socket URL from plugins.toml configuration file
        let socket_url = Self::read_config_url().unwrap_or_else(|e| {
            error!("Failed to read configuration: {}. Using default URL.", e);
            "ws://localhost:9200".to_string()
        });

        info!("🔧 DyingstarServicesPlugin: Using resources dynamic address: {}", socket_url);

        Self {
            name: "ds_services".to_string(),
            socket_url,
            websocket: Arc::new(Mutex::new(None)),
            can_query_service: Arc::new(Mutex::new(false)),
        }
    }

    fn read_config_url() -> Result<String, String> {
        // Try multiple possible paths for the plugins.toml file
        let possible_paths = vec![
            "../Horizon/plugins.toml",
            "Horizon/plugins.toml",
            "plugins.toml",
        ];

        for path in possible_paths {
            if Path::new(path).exists() {
                let contents = fs::read_to_string(path)
                    .map_err(|e| format!("Failed to read {}: {}", path, e))?;
                
                let config: toml::Value = toml::from_str(&contents)
                    .map_err(|e| format!("Failed to parse TOML: {}", e))?;
                
                if let Some(ds_services) = config.get("ds_services") {
                    if let Some(address) = ds_services.get("resources_dynamic_address") {
                        if let Some(address_str) = address.as_str() {
                            return Ok(format!("ws://{}", address_str));
                        }
                    }
                }
                
                return Err(format!("'ds_services.resources_dynamic_address' not found in {}", path));
            }
        }

        Err("plugins.toml file not found in any expected location".to_string())
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

        let can_query_service = Arc::new(Mutex::new(false));
        let can_query_service_clone = Arc::clone(&can_query_service);

        let websocket = Arc::clone(&self.websocket);
        events.on_core("region_started", move |_event: RegionStartedEvent| {
            // query to get planets
            if let Ok(mut guard) = can_query_service_clone.lock() {
                *guard = true;
                debug!("🔧 DyingstarServicesPlugin: Region started, can now query external service");
                // do request to external service to get planets
                if let Ok(mut ws_guard) = websocket.lock() {
                    if let Some(sender) = ws_guard.as_mut() {
                        let request = serde_json::json!({
                            "event_type": "init",
                            "data": {
                                "system_internal_name": "tarsis",
                                "duration_s": "3",
                                "frequency": "60",
                                "from_timestamp": "0",
                            }
                        });
                        let message = serde_json::to_string(&request).unwrap_or_else(|e| {
                            error!("🔧 DyingstarServicesPlugin: ❌ Failed to serialize request to JSON string: {}", e);
                            "{}".to_string()
                        });
                        debug!("🔧 DyingstarServicesPlugin: Sending get_planets request to external service: {}", message);
                        match sender.send_message(&OwnedMessage::Text(message.clone())) {
                            Ok(_) => debug!("🔧 DyingstarServicesPlugin: ✅ Sent get_planets request to external service"),
                            Err(e) => error!("🔧 DyingstarServicesPlugin: ❌ Failed to send get_planets request: {}", e),
                        }
                    }
                }
            }
            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        self.can_query_service = can_query_service;
        
        info!("🔧 DyingstarServicesPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        let config = ds_common::config::Config::new("ds_services");
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


        // Init websocket connection
        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);
        let can_query_service = Arc::clone(&self.can_query_service);

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
                                    if let Ok(can_query) = can_query_service.lock() {
                                        if *can_query {
                                            debug!("Received {} objects from external service", data_array.len());

                                            // Spawn async task to emit events
                                            let context_clone = context.clone();
                                            let data_array_clone = data_array.clone();
                                            let _ = rt.block_on(async {
                                                // Loop through each object in the data array
                                                for (index, obj) in data_array_clone.iter().enumerate() {
                                                    if let Some(object_type) = obj.get("object_type").and_then(|t| t.as_str()) {
                                                        let mut modified_obj = obj.clone();
                                                        if object_type == "planet" {
                                                            debug!("Emitting planet object to genericprops...");
                                                            if let Some(scenename) = modified_obj["object_data"]["scenename"].as_str() {
                                                                modified_obj["object_data"]["scenename"] = scenename.replace("scenes/planet/", "scenes/systems/tarsis/").into();
                                                            }
                                                            modified_obj["object_data"]["parent_id"] = "".into();

        // Round positions and rotations to 3 decimal places
                                                            round_positions_rotations(&mut modified_obj);

                                                            if let Err(e) = context_clone.events().emit_plugin("genericprops", "create_object", &modified_obj).await {
                                                                error!("Failed to emit plugin event: {}", e);
                                                            }
                                                            if let Err(e) = context_clone.events().emit_plugin("props", "planet", &modified_obj).await {
                                                                error!("Failed to emit plugin event to props: {}", e);
                                                            }
                                                        }
                                                        else if object_type == "moon" {
                                                            debug!("Emitting moon object to genericprops...");
                                                            modified_obj["object_type"] = "planet".into();
                                                            if let Some(scenename) = modified_obj["object_data"]["scenename"].as_str() {
                                                                modified_obj["object_data"]["scenename"] = scenename.replace("scenes/moon/", "scenes/systems/tarsis/").into();
                                                            }

                                                            // Round positions and rotations to 3 decimal places
                                                            round_positions_rotations(&mut modified_obj);
                                                            
                                                            if let Err(e) = context_clone.events().emit_plugin("genericprops", "create_object", &modified_obj).await {
                                                                error!("Failed to emit plugin event: {}", e);
                                                            }
                                                            if let Err(e) = context_clone.events().emit_plugin("props", "planet", &modified_obj).await {
                                                                error!("Failed to emit plugin event to props: {}", e);
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
                                                        } else {
                                                            warn!("Unknown object_type '{}' at index {}", object_type, index);
                                                            
                                                        }
                                                    }
                                                }
                                                // Send event for load persistence
                                                if let Err(e) = context_clone.events().emit_plugin(
                                                    "bridge_persistence",
                                                    "get_all_items",
                                                    &serde_json::json!({}),
                                                ).await {
                                                    error!("Failed to emit get_all_items event: {}", e);
                                                }
                                            });                                            
                                        }
                                    }
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

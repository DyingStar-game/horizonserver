use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin, ClientEventWrapper, PlayerDisconnectedEvent, ClientConnectionRef, GorcObjectId, Dest, GorcEvent, Vec3, current_timestamp
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::{info, error, debug, warn};
use std::path::Path;
use websocket::ClientBuilder;
use websocket::r#async::client::{Client, ClientNew, Framed};
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
use websocket::result::WebSocketError;
use std::process::exit;
use serde_json::json;
use std::collections::HashMap;
use std::fs;

/// Message types for the async processor
#[derive(Debug)]
enum GameServerMessage {
    PlayerPositions(Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64)>),
    PropPosition(serde_json::Value),
    PropCreate(serde_json::Value),
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInit {
    pub data: PlayerInitData,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInitData {
    pub name: String,
    pub spawnpoint: i32,
}

// DsGameServer Plugin
pub struct DsGameServerPlugin {
    name: String,
    socket_url: String,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
}

impl DsGameServerPlugin {
    pub fn new() -> Self {
        info!("🔧 DsGameServerPlugin: Creating new instance");

        // Read socket URL from plugins.toml configuration file
        let socket_url = Self::read_config_url().unwrap_or_else(|e| {
            error!("Failed to read configuration: {}. Using default URL.", e);
            "ws://127.0.0.1:8980".to_string()
        });

        info!("🔧 DsGameServerPlugin: Using game server address: {}", socket_url);

        Self {
            name: "ds_game_server".to_string(),
            socket_url,
            websocket: Arc::new(Mutex::new(None)),
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
                
                if let Some(ds_game_server) = config.get("ds_game_server") {
                    if let Some(address) = ds_game_server.get("game_server_address") {
                        if let Some(address_str) = address.as_str() {
                            return Ok(format!("ws://{}", address_str));
                        }
                    }
                }
                return Err(format!("'ds_game_server.game_server_address' not found in {}", path));
            }
        }
        Err("plugins.toml file not found in any expected location".to_string())
    }
}

#[async_trait]
impl SimplePlugin for DsGameServerPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn register_handlers(
        &mut self,
        events: Arc<EventSystem>,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        info!("🔧 DsGameServerPlugin: Registering event handlers...");

        // Get the tokio runtime handle from the ServerContext
        // This is the proper way to get the runtime handle across DLL boundaries
        let runtime_handle = context.tokio_handle();

        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);
        let events1 = events.clone();
        let runtime_handle_clone = runtime_handle.clone();

        // initialize websocket connection to game server
        events.on_plugin("gameserverplugin", "init_server", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: Initializing server with event {:?}", event);

            let url = url.clone();
            let websocket = Arc::clone(&websocket);
            let events2 = events1.clone();
            let _initial_event = event.clone();
            let runtime_handle = runtime_handle_clone.clone();

            std::thread::spawn(move || {
                // Connect and store writer
                let socket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();
                let (mut receiver, sender) = socket.split().unwrap();
                *websocket.lock().unwrap() = Some(sender);

                // Create tokio channel for async processing
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<GameServerMessage>();

                // Spawn async processor on the MAIN runtime (not a new one!)
                // This avoids cross-runtime lock issues with EventSystem
                let events_processor = events2.clone();
                let websocket_processor = websocket.clone();
                
                runtime_handle.spawn(async move {
                    info!("🔧 DsGameServerPlugin: Async processor task started on main runtime");
                    let mut message_count = 0u64;
                    while let Some(msg) = rx.recv().await {
                        message_count += 1;
                        if message_count % 100 == 0 {
                            debug!("� Async processor: processed {} messages", message_count);
                        }
                        match msg {
                            GameServerMessage::PlayerPositions(position_updates) => {
                                for (gorc_id, player_id, x, y, z, rx, ry, rz) in position_updates {
                                    if let Err(e) = events_processor.emit_gorc_instance(
                                        gorc_id,
                                        0,
                                        "move",
                                        &serde_json::json!({
                                            "player_id": player_id,
                                            "new_position": Vec3::new(x, y, z),
                                            "new_rotation": Vec3::new(rx, ry, rz),
                                            "velocity": { "x": 0.0, "y": 0.0, "z": 0.0 },
                                            "movement_state": 1,
                                            "client_timestamp": chrono::Utc::now().to_rfc3339(),
                                        }),
                                        Dest::Both
                                    ).await {
                                        error!("Failed to update player position via EventSystem: {}", e);
                                    }
                                }
                            }
                            GameServerMessage::PropPosition(prop_data) => {
                                if let Err(e) = events_processor.emit_plugin("genericprops", "update_object", &serde_json::json!({
                                    "object_type": prop_data["type"],
                                    "object_uuid": prop_data["uuid"],
                                    "object_data": prop_data,
                                })).await {
                                    error!("Failed to emit plugin event to propsplugin: {}", e);
                                }
                            }
                            GameServerMessage::PropCreate(prop_data) => {
                                if let Err(e) = events_processor.emit_plugin("genericprops", "create_object_from_gameserver", &serde_json::json!({
                                    "object_type": prop_data["type"],
                                    "object_uuid": prop_data["uuid"],
                                    "object_data": prop_data,
                                })).await {
                                    error!("Failed to emit plugin event to propsplugin: {}", e);
                                }

                                let message = json!({
                                    "namespace": "server",
                                    "event": "add_prop",
                                    "data": {
                                        "object_type": prop_data["type"],
                                        "object_uuid": prop_data["uuid"],
                                        "object_data": prop_data,
                                    }
                                });
                                if let Ok(mut ws_guard) = websocket_processor.lock() {
                                    if let Some(w) = ws_guard.as_mut() {
                                        if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                                            error!("Failed to send websocket message: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    warn!("🔧 DsGameServerPlugin: Async processor channel closed");
                });

                info!("WebSocket reader thread started");
                for msg in receiver.incoming_messages() {
                    match msg {
                        Ok(OwnedMessage::Text(s)) => {
                            debug!("[message][from][gamesever]: {}", s);
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                                if value["namespace"] == "players" && value["event"] == "position" {
                                    let mut position_updates: Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64)> = Vec::new();
                                    
                                    for player_data in value["data"].as_array().unwrap() {
                                        if let Some(uuid_str) = player_data["player_id"].as_str() {
                                            if let (Ok(player_id), Ok(gorc_id)) = (
                                                PlayerId::from_str(uuid_str),
                                                GorcObjectId::from_str(uuid_str)
                                            ) {
                                                if let (Some(x), Some(y), Some(z), Some(rx), Some(ry), Some(rz)) = (
                                                    player_data["pos"]["x"].as_f64(),
                                                    player_data["pos"]["y"].as_f64(),
                                                    player_data["pos"]["z"].as_f64(),
                                                    player_data["rot"]["x"].as_f64(),
                                                    player_data["rot"]["y"].as_f64(),
                                                    player_data["rot"]["z"].as_f64()
                                                ) {
                                                    position_updates.push((gorc_id, player_id, x, y, z, rx, ry, rz));
                                                } else {
                                                    error!("Invalid position coordinates in player data: {:?}", player_data["pos"]);
                                                }
                                            } else {
                                                error!("Invalid player_id format: {:?}", player_data["player_id"]);
                                            }
                                        } else {
                                            error!("Missing player_id in player data: {:?}", player_data);
                                        }
                                    }
                                    
                                    // Send to async processor - non-blocking!
                                    if !position_updates.is_empty() {
                                        if let Err(e) = tx.send(GameServerMessage::PlayerPositions(position_updates)) {
                                            error!("Failed to send position updates to processor: {}", e);
                                        }
                                    }
                                } else if value["namespace"] == "props" && value["event"] == "position" {
                                    // Send each prop update to async processor - non-blocking
                                    for prop_data in value["data"].as_array().unwrap() {
                                        if let Err(e) = tx.send(GameServerMessage::PropPosition(prop_data.clone())) {
                                            error!("Failed to send prop position to processor: {}", e);
                                        }
                                    }
                                } else if value["namespace"] == "props" && value["event"] == "create_object" {
                                    println!("Props creation object received: {:?}", value);
                                    for prop_data in value["data"].as_array().unwrap() {
                                        if let Err(e) = tx.send(GameServerMessage::PropCreate(prop_data.clone())) {
                                            error!("Failed to send prop create to processor: {}", e);
                                        }
                                    }
                                }
                            } else {
                                debug!("Failed to parse incoming JSON: {}", s);
                            }
                        }
                        Ok(OwnedMessage::Binary(b)) => {
                            if let Ok(s) = String::from_utf8(b) {
                                debug!("[message][from][gamesever] (binary->text): {}", s);
                                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                                    if value["namespace"] == "players" && value["event"] == "position" {
                                        let mut position_updates: Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64)> = Vec::new();
                                        
                                        for player_data in value["data"].as_array().unwrap() {
                                            if let Some(uuid_str) = player_data["player_id"].as_str() {
                                                if let (Ok(player_id), Ok(gorc_id)) = (
                                                    PlayerId::from_str(uuid_str),
                                                    GorcObjectId::from_str(uuid_str)
                                                ) {
                                                    if let (Some(x), Some(y), Some(z), Some(rx), Some(ry), Some(rz)) = (
                                                        player_data["pos"]["x"].as_f64(),
                                                        player_data["pos"]["y"].as_f64(),
                                                        player_data["pos"]["z"].as_f64(),
                                                        player_data["rot"]["x"].as_f64(),
                                                        player_data["rot"]["y"].as_f64(),
                                                        player_data["rot"]["z"].as_f64()
                                                    ) {
                                                        position_updates.push((gorc_id, player_id, x, y, z, rx, ry, rz));
                                                    }
                                                }
                                            }
                                        }
                                        
                                        if !position_updates.is_empty() {
                                            if let Err(e) = tx.send(GameServerMessage::PlayerPositions(position_updates)) {
                                                error!("Failed to send position updates to processor: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(_) => { /* ignore ping/pong/close frames */ }
                        Err(WebSocketError::NoDataAvailable) => {
                            info!("\nDisconnected!");
                            exit(2);
                        }
                        Err(e) => {
                            error!("WebSocket read error: {:?}", e);
                            exit(2);
                        }
                    }
                }
            });

            Ok(())
        }).await.unwrap();

        let websocket = Arc::clone(&self.websocket);
        events.on_plugin("gameserverplugin", "spawn_object", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: Adding prop with event {:?}", event);
            
            let message = json!({
                "namespace": "server",
                "event": "add_prop",
                "data": event,
            });
            let mut ws_guard = websocket.lock().map_err(|e| EventError::HandlerExecution(format!("websocket lock error: {}", e)))?;
            debug!("[message][to][gamesever]: {:?}", message);
            if let Some(w) = ws_guard.as_mut() {
                if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                    return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                }
            } else {
                return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
            }
            Ok(())
        }).await.unwrap();

        // specific player
        let websocket = Arc::clone(&self.websocket);
        events.on_plugin("plugingameserver", "new_player", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: New player event {:?}", event);
            
            let message = json!({
                "namespace": "server",
                "event": "add_prop",
                "data": event,
            });
            let mut ws_guard = websocket.lock().map_err(|e| EventError::HandlerExecution(format!("websocket lock error: {}", e)))?;
            debug!("[message][to][gamesever]: {:?}", message);
            if let Some(w) = ws_guard.as_mut() {
                if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                    return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                }
            } else {
                return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
            }
            Ok(())
        }).await.unwrap();


        let websocket = Arc::clone(&self.websocket);
        events.on_client(
            "movement",
            "update_velocity",
            move |wrapper: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {
                debug!("📝 LoggerPlugin: 🦘 Client movement from player {}", wrapper.player_id);

                let websocket = Arc::clone(&websocket);

                std::thread::spawn(move || {

                    // Parse the movement data
                    let message = json!({
                        "namespace": "player",
                        "event": "move",
                        "player_id": wrapper.player_id.to_string(),
                        "data": wrapper.data.clone(),
                    });
                    debug!("[message][to][gamesever]: {:?}", message);
                    match websocket.lock() {
                        Ok(mut guard) => {
                            if let Some(w) = guard.as_mut() {
                                if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                                    error!("Failed to send websocket message: {}", e);
                                }
                            } else {
                                error!("No websocket writer available to send movement");
                            }
                        }
                        Err(e) => {
                            error!("Failed to lock websocket mutex: {}", e);
                        }
                    }
                });
                 Ok(())
            },
        )
        .await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        // let websocket2 = Arc::clone(&self.websocket);
        // events.on_client(
        //     "actions",
        //     "action_pressed",
        //     move |wrapper: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {
        //         info!("📝 LoggerPlugin: 🦘 Client action from player {}", wrapper.player_id);
 
        //         let websocket = Arc::clone(&websocket2);

        //         std::thread::spawn(move || {
        //             // Parse the movement data
        //             let message = json!({
        //                 "namespace": "player",
        //                 "event": "action",
        //                 "player_id": wrapper.player_id.to_string(),
        //                 "data": wrapper.data.clone(),
        //             });
        //             debug!("[message][to][gamesever]: {:?}", message);
        //             match websocket.lock() {
        //                 Ok(mut guard) => {
        //                     if let Some(w) = guard.as_mut() {
        //                         if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
        //                             error!("Failed to send websocket message: {}", e);
        //                         }
        //                     } else {
        //                         error!("No websocket writer available to send action");
        //                     }
        //                 }
        //                 Err(e) => {
        //                     error!("Failed to lock websocket mutex: {}", e);
        //                 }
        //             }
        //         });
 
        //         Ok(())
        //     },
        // )
        // .await
        // .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        info!("🔧 DsGameServerPlugin: ✅ All handlers registered successfully!");
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

        info!("🔧 DsGameServerPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "🔧 DsGameServerPlugin: Shutting down!");
        info!("🔧 DsGameServerPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DsGameServerPlugin);

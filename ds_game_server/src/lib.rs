use async_trait::async_trait;
use std::env;
use once_cell::sync::Lazy;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin, ClientEventWrapper, PlayerDisconnectedEvent, ClientConnectionRef, GorcObjectId, Dest, GorcEvent, Vec3, current_timestamp
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::{info, error, debug};
use tracing_appender::rolling;
use tracing_appender::non_blocking;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use std::path::Path;
use dotenvy::dotenv;
use websocket::ClientBuilder;
// use websocket::client::sync::Client;
use websocket::r#async::client::{Client, ClientNew, Framed};
// use websocket::r#async::TcpStream;
// use websocket::stream::sync::TcpStream;
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
use websocket::result::WebSocketError;
use std::process::exit;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Once;

// Global mapping from player UUID to internal connection ID
static mut MAPPING_PLAYER_GORC_TO_INTERNAL: Option<Mutex<HashMap<String, String>>> = None;
static INIT_MAPPING: Once = Once::new();

fn get_mapping() -> &'static Mutex<HashMap<String, String>> {
    unsafe {
        INIT_MAPPING.call_once(|| {
            MAPPING_PLAYER_GORC_TO_INTERNAL = Some(Mutex::new(HashMap::new()));
        });
        MAPPING_PLAYER_GORC_TO_INTERNAL.as_ref().unwrap()
    }
}

static SOCKET_URL: Lazy<String> = Lazy::new(|| {
    dotenv().ok(); // Loads variables from `.env` file
    env::var("SOCKET_URL").unwrap_or_else(|_| "ws://127.0.0.1:8980".to_string())
});

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

        Self {
            name: "ds_game_server".to_string(),
            socket_url: SOCKET_URL.clone(),
            websocket: Arc::new(Mutex::new(None)),
        }
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
        _context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        info!("🔧 DsGameServerPlugin: Registering event handlers...");

        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);
        let events1 = events.clone();

        /// initialize websocket connection to game server
        events.on_plugin("gameserverplugin", "init_server", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: Initializing server with event {:?}", event);
            info!("Connecting to server: {:?}", SOCKET_URL.clone());

            let url = url.clone();
            let websocket = Arc::clone(&websocket);
            let events2 = events1.clone();
            let initial_event = event.clone();

            std::thread::spawn(move || {
                // Connect and store writer
                let socket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();
                let (mut receiver, sender) = socket.split().unwrap();
                *websocket.lock().unwrap() = Some(sender);

                // // Send initial add_props
                // let message = json!({
                //     "namespace": "server",
                //     "event": "add_props",
                //     "data": {
                //         "planets": initial_event["planets"],
                //         "player": initial_event["player"]
                //     },
                // });
                // debug!("[message][to][gamesever]: {:?}", message);
                // if let Some(w) = websocket.lock().unwrap().as_mut() {
                //     let _ = w.send_message(&OwnedMessage::Text(message.to_string()));
                // }

                // local runtime to call async event system from this blocking thread
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build temp runtime");

                info!("WebSocket reader thread started");
                for msg in receiver.incoming_messages() {
                    match msg {
                        Ok(OwnedMessage::Text(s)) => {
                            debug!("[message][from][gamesever]: {}", s);
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                                if value["namespace"] == "players" && value["event"] == "position" {
                                    // notify EventSystem about the player position
                                    for player_data in value["data"].as_array().unwrap() {
                                        if let Some(uuid_str) = player_data["player_id"].as_str() {
                                            // Get gorc_id from mapping or fallback to direct field
                                            let gorc_id_str = if let Ok(mapping) = get_mapping().lock() {
                                                mapping.get(uuid_str).cloned()
                                            } else {
                                                None
                                            }.or_else(|| player_data["gorc_id"].as_str().map(|s| s.to_string()));
                                            
                                            if let Some(gorc_id_str) = gorc_id_str {
                                                if let (Ok(player_id), Ok(gorc_id)) = (
                                                    PlayerId::from_str(uuid_str),
                                                    GorcObjectId::from_str(&gorc_id_str)
                                                ) {
                                                    if let (Some(x), Some(y), Some(z)) = (
                                                        player_data["pos"]["x"].as_f64(),
                                                        player_data["pos"]["y"].as_f64(),
                                                        player_data["pos"]["z"].as_f64()
                                                    ) {
                                                        let events_clone = events2.clone();
                                                        let _ = rt.block_on(async move {
                                                            if let Err(e) = events_clone.emit_gorc_client(
                                                                player_id,
                                                                gorc_id,
                                                                0,
                                                                "move",
                                                                &serde_json::json!({
                                                                    "player_id": player_id,
                                                                    "new_position": Vec3::new(x, y, z),
                                                                    "velocity": { "x": 0.0, "y": 0.0, "z": 0.0 },
                                                                    "movement_state": 1,
                                                                    "client_timestamp": chrono::Utc::now().to_rfc3339(),
                                                                }),
                                                            ).await {
                                                                error!("Failed to update player position via EventSystem: {}", e);
                                                            }
                                                        });
                                                    }
                                                } else {
                                                    error!("Invalid position coordinates in player data: {:?}", player_data["pos"]);
                                                }
                                            } else {
                                                error!("Failed to parse player/object ID from UUID (mapping): {}", uuid_str);
                                            }
                                        } else {
                                            error!("Missing player_id in player data: {:?}", player_data);
                                        }
                                    }



                                    //     if let Err(e) = events_clone.emit_plugin("propsplugin", "players_position_update", &payload).await {
                                    //         tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                    //     }
                                    //     // loop on value["data"] array
                                    //     let players_newposition = value["data"].as_array().unwrap();
                                    //     for p in players_newposition {
                                    //         let new_pos = serde_json::json!({
                                    //             "x": p["pos"]["x"],
                                    //             "y": p["pos"]["y"],
                                    //             "z": p["pos"]["z"]
                                    //         });
                                    //         let event = serde_json::json!({
                                    //             "player_id": p["uuid"],
                                    //             "new_position": new_pos,
                                    //             "velocity": { "x": 10.0, "y": 0.0, "z": 5.0 },
                                    //             "movement_state": 1,
                                    //             "client_timestamp": "2024-01-15T10:30:45Z"
                                    //         });
                                    //         // println!("Updating position for player {} to {:?}", p["uuid"], new_pos);

                                    //         if let Err(e) = events_clone.emit_gorc_instance(GorcObjectId::from_str(p["uuid"].as_str().unwrap()).unwrap(), 0, "move", &event, Dest::Both).await {
                                    //             error!("Failed to update player position via EventSystem: {}", e);
                                    //         }
                                    //     }
                                } else if value["namespace"] == "props" && value["event"] == "position" {
                                    println!("Props position update received: {:?}", value);
                                    // Iterate over props data if it's an array
                                    for prop_data in value["data"].as_array().unwrap() {
                                        let events_clone = events2.clone();
                                        let _ = rt.block_on(async move {
                                            if let Err(e) = events_clone.emit_plugin("genericprops", "update_object", &serde_json::json!({
                                                    "object_type": prop_data["type"],
                                                    "object_uuid": prop_data["uuid"],
                                                    "object_data": prop_data,
                                                })).await {
                                                tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                            }
                                        });
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
                                        let payload = serde_json::json!({ "players": value["data"] });
                                        let events_clone = events2.clone();
                                        let _ = rt.block_on(async move {
                                            if let Err(e) = events_clone.emit_plugin("propsplugin", "players_position_update", &payload).await {
                                                tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                            }
                                        });
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
        events.on_plugin("gorcplugin", "new_player", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: New player event {:?}", event);
            
            // Check if this is a player object and store the mapping
            if let Some(object_type) = event["object_type"].as_str() {
                if object_type == "player" {
                    if let (Some(object_uuid), Some(connection_id)) = (
                        event["object_uuid"].as_str(),
                        event["object_data"]["connection_id"].as_str()
                    ) {
                        if let Ok(mut mapping) = get_mapping().lock() {
                            mapping.insert(connection_id.to_string(), object_uuid.to_string());
                            info!("🔧 DsGameServerPlugin: Stored mapping {} -> {}", connection_id, object_uuid);
                        }
                    }
                }
            }
            
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
                info!("📝 LoggerPlugin: 🦘 Client movement from player {}", wrapper.player_id);
                // println!("player movement {:?}", wrapper);
                // println!("📝 LoggerPlugin: 🦘 Client movement");

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

        // events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
        //     debug!("[disconnected]: {:?}", event);
        //     println!("Player disconnected.");
        //     Ok(())
        // }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;

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

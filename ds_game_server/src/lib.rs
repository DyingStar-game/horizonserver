use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin, ClientEventWrapper, PlayerDisconnectedEvent,
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
            socket_url: "ws://192.168.20.174:8980".to_string(),
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

        // register_handlers!(events; client {
        //     "chat", "message" => |event: serde_json::Value| {
        //         println!("DsGameServerPlugin: message received {:?}", event);
        //         Ok(())
        //     },
        // })?;

        let url = self.socket_url.clone();
        let websocket = Arc::clone(&self.websocket);
        let events1 = events.clone();
        // events.on_client("player", "init", move |event: PlayerInit| {
        //     println!("Receive player init message {:?}", event);
        //     let message = json!({
        //         "type": "playerinit",
        //         "name": event.data.name,
        //         "spawnpoint": event.data.spawnpoint
        //     });
        //     // self.websocket.expect("Problem for send to game server").send_message(&message).unwrap();

        //     let mut ws_guard = websocket.lock().unwrap();
        //     if let Err(e) = ws_guard
        //         .as_mut()
        //         .expect("Problem for send to game server")
        //         .send_message(&OwnedMessage::Text(message.to_string()))
        //     {
        //         return Err(EventError::HandlerExecution(format!("Message blocked: {e}")));
        //     }
        //     Ok(())
        // }).await.unwrap();

        events.on_plugin("gameserverplugin", "init_server", move |event: serde_json::Value| {
            println!("🔧 DsGameServerPlugin: Initializing server with event {:?}", event);

            let url = url.clone();
            let websocket = Arc::clone(&websocket);
            let events2 = events1.clone();
            let initial_event = event.clone();

            std::thread::spawn(move || {
                // Connect and store writer
                let socket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();
                let (mut receiver, sender) = socket.split().unwrap();
                *websocket.lock().unwrap() = Some(sender);

                // Send initial add_props
                let message = json!({
                    "namespace": "server",
                    "event": "add_props",
                    "data": {
                        "planets": initial_event["planets"],
                        "player": initial_event["player"]
                    },
                });
                debug!("[message][to][gamesever]: {:?}", message);
                if let Some(w) = websocket.lock().unwrap().as_mut() {
                    let _ = w.send_message(&OwnedMessage::Text(message.to_string()));
                }

                // local runtime to call async event system from this blocking thread
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build temp runtime");

                println!("WebSocket reader thread started");
                for msg in receiver.incoming_messages() {
                    match msg {
                        Ok(OwnedMessage::Text(s)) => {
                            debug!("[message][from][gamesever]: {}", s);
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                                if value["namespace"] == "players" && value["event"] == "position" {
                                    let payload = serde_json::json!({ "players": value["data"] });
                                    let events_clone = events2.clone();
                                    let _ = rt.block_on(async move {
                                        if let Err(e) = events_clone.emit_plugin("propsplugin", "players_position_update", &payload).await {
                                            tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                        }
                                    });
                                } else if value["namespace"] == "props" && value["event"] == "position" {
                                    println!("Props position update received: {:?}", value);
                                    let payload = serde_json::json!({ "props": value["data"] });
                                    let events_clone = events2.clone();
                                    let _ = rt.block_on(async move {
                                        if let Err(e) = events_clone.emit_plugin("propsplugin", "props_position_update", &payload).await {
                                            tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                        }
                                    });
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
                            println!("\nDisconnected!");
                            exit(2);
                        }
                        Err(e) => {
                            println!("WebSocket read error: {:?}", e);
                            exit(2);
                        }
                    }
                }
            });

            Ok(())
        }).await.unwrap();

        let websocket = Arc::clone(&self.websocket);
        events.on_plugin("gameserverplugin", "add_props", move |event: serde_json::Value| {
            println!("🔧 DsGameServerPlugin: Adding props with event {:?}", event);
            let message = json!({
                "namespace": "server",
                "event": "add_props",
                "data": {
                    "planets": [],
                    "player": event["player"]
                },
            });
            let mut ws_guard = websocket.lock().map_err(|e| EventError::HandlerExecution(format!("websocket lock error: {}", e))).unwrap();
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
        events.on_plugin("gameserverplugin", "add_prop", move |event: serde_json::Value| {
            println!("🔧 DsGameServerPlugin: Adding prop with event {:?}", event);
            let message = json!({
                "namespace": "server",
                "event": "add_prop",
                "data": event,
            });
            let mut ws_guard = websocket.lock().map_err(|e| EventError::HandlerExecution(format!("websocket lock error: {}", e))).unwrap();
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
        events.on_client_with_connection(
            "movement",
            "update_position",
            move |wrapper: ClientEventWrapper<serde_json::Value>, _connection| {
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

        events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
            debug!("[disconnected]: {:?}", event);
            println!("Player disconnected.");
            Ok(())
        }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        info!("🔧 DsGameServerPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "🔧 DsGameServerPlugin: Starting up!");

        // --- plugin-specific file logger ---
        // ensure logs directory exists (optional)
        let log_dir = "logs";
        let _ = std::fs::create_dir_all(log_dir);
        // never rotate, single file in logs/
        let file_appender = rolling::never(log_dir, "ds_game_server.log");
        let (non_blocking, guard) = non_blocking(file_appender);
        // keep guard alive for program lifetime so logs flush on exit
        std::mem::forget(Box::new(guard));
        // create a layer that writes into the file (non-ANSI)
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(non_blocking);
        // Try to add the file layer to the global subscriber. If the global subscriber
        // is already initialized elsewhere this will return Err — ignore in that case.
        let _ = tracing_subscriber::registry().with(file_layer).try_init();
        // --- end file logger ---

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

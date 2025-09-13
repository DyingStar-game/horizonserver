use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin, ClientEventWrapper,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::{info, error};

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

            std::thread::spawn(move || {
                let socket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();
                let (mut receiver, sender) = socket.split().unwrap();
                *websocket.lock().unwrap() = Some(sender);

                let message = json!({
                    "namespace": "server",
		            "event": "add_props",
                    "data": {
                        "planets": event["planets"],
                        "player": event["player"]
                    },
                });
                websocket.lock().unwrap().as_mut().unwrap().send_message(&OwnedMessage::Text(message.to_string())).unwrap();
                
                std::thread::spawn(move || {
                    println!("WebSocket reader thread spawned");

                    for message in receiver.incoming_messages() {
                        match message {
                            Ok(msg) => {
                                println!("Receive message from the game server {:?}", msg);
                                // convert OwnedMessage -> String (Text or Binary)
                                let text_opt: Option<String> = match msg {
                                    OwnedMessage::Text(s) => Some(s.clone()),
                                    OwnedMessage::Binary(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                                    _ => None,
                                };
                                if let Some(text) = text_opt {
                                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                                        if value["namespace"] == "player" && value["event"] == "position" {
                                            let events3 = events2.clone();
                                            std::thread::spawn(move || {
                                                let rt = tokio::runtime::Builder::new_current_thread()
                                                    .enable_all()
                                                    .build()
                                                    .expect("failed to build temp runtime");

                                                rt.block_on(async move {                                            
                                                    // send to plugin props
                                                    let payload = serde_json::json!({
                                                        "player": value["data"],
                                                        "player_id": value["player_id"],
                                                    });

                                                    if let Err(e) = events3.emit_plugin("propsplugin", "player_position_update", &payload)
                                                        .await
                                                    {
                                                        tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                                                    }
                                                });
                                            });
                                        }
                                    }
                                }
                            }
                            Err(err) => {

                                // Handle the different types of possible errors
                                match err {
                                    WebSocketError::NoDataAvailable => {
                                        println!("\nDisconnected!");
                                        println!("Error: {:?}", err);
                                        exit(2);
                                    },
                                    _ => {
                                        println!("Error: {:?}", err);
                                        println!("Error in WebSocket reader: {}", err);
                                        exit(2);
                                    }
                                }
                            }
                        }
                    }
                });                
            });

            Ok(())
        }).await.unwrap();

        let websocket = Arc::clone(&self.websocket);
        events.on_client_with_connection(
            "movement",
            "update_position",
            move |wrapper: ClientEventWrapper<serde_json::Value>, _connection| {
                info!("📝 LoggerPlugin: 🦘 Client movement from player {}", wrapper.player_id);
                println!("player movement {:?}", wrapper);
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
                    websocket.lock().unwrap().as_mut().unwrap().send_message(&OwnedMessage::Text(message.to_string())).unwrap();
                });
 
                Ok(())
            },
        )
        .await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        info!("🔧 DsGameServerPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "🔧 DsGameServerPlugin: Starting up!");

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

use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing::info;

use websocket::ClientBuilder;
// use websocket::client::sync::Client;
use websocket::r#async::client::{Client, ClientNew, Framed};
// use websocket::r#async::TcpStream;
// use websocket::stream::sync::TcpStream;
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
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

        let websocket = Arc::clone(&self.websocket);
        events.on_client("player", "init", move |event: PlayerInit| {
            println!("Receive player init message {:?}", event);
            let message = json!({
                "type": "playerinit",
                "name": event.data.name,
                "spawnpoint": event.data.spawnpoint
            });
            // self.websocket.expect("Problem for send to game server").send_message(&message).unwrap();

            let mut ws_guard = websocket.lock().unwrap();
            if let Err(e) = ws_guard
                .as_mut()
                .expect("Problem for send to game server")
                .send_message(&OwnedMessage::Text(message.to_string()))
            {
                return Err(EventError::HandlerExecution(format!("Message blocked: {e}")));
            }
            Ok(())
        }).await.unwrap();



        info!("🔧 DsGameServerPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "🔧 DsGameServerPlugin: Starting up!");

        let url = self.socket_url.clone();

        // Spawn a thread for the blocking WebSocket client
        // std::thread::spawn(move || {
            
            // self.websocket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();

            // self.websocket = match ClientBuilder::new(&url).unwrap().connect_insecure() {

            let socket = ClientBuilder::new(&url).unwrap().connect_insecure().unwrap();
            let (receiver, sender) = socket.split().unwrap();
            *self.websocket.lock().unwrap() = Some(sender);

 
            // self.websocket = match sender {
            //     Ok(sender) => {
            //         Some(sender)
            //     }
            //     Err(e) => {
            //         println!("Unable to connect to the game server");
            //         None
            //     }
            // }
            

            // match ClientBuilder::new(&url)
            //     .unwrap()
            //     .connect_insecure()
            // {
            //     Ok(mut dying_star_game_server) => {
            //         self.websocket = dying_star_game_server;
            //         info!("🔧 Connected to game server at {}", url);
            //         println!("🔧 Connected to game server at {}", url);

            //         // Send a hello message
            //         dying_star_game_server
            //             .send_message(&OwnedMessage::Text("Hello, World!".into()))
            //             .unwrap();

            //         // Listen for incoming messages
            //         for message in dying_star_game_server.incoming_messages() {
            //             match message {
            //                 Ok(OwnedMessage::Text(txt)) => {
            //                     println!("📩 Received: {}", txt);
            //                 }
            //                 Ok(other) => {
            //                     println!("📩 Received other message: {:?}", other);
            //                 }
            //                 Err(e) => {
            //                     println!("❌ Error reading from socket: {:?}", e);
            //                     break;
            //                 }
            //             }
            //         }
            //     }
            //     Err(e) => {
            //         println!("❌ Failed to connect to {}: {:?}", url, e);
            //     }
            // }
        // });

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

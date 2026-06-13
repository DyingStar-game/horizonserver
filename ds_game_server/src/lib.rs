mod server;
mod servermanager;

use async_trait::async_trait;
use futures::io::ReuniteError;
use horizon_event_system::{
    create_simple_plugin, EventError, EventSystem, PlayerId, LogLevel, PluginError, ServerContext, SimplePlugin, ClientEventWrapper, PlayerDisconnectedEvent, ClientConnectionRef, GorcObjectId, Dest, GorcEvent, Vec3, current_timestamp
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tracing_subscriber::field::debug;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, error, info, warn};
// use std::path::Path;
// use websocket::ClientBuilder;
use websocket::r#async::client::{Client, ClientNew, Framed};
// use std::net::TcpStream;
// use websocket::message::OwnedMessage;
// use websocket::sender::Writer;
// use websocket::result::WebSocketError;
// use std::process::exit;
// use serde_json::json;
// use std::collections::HashMap;
// use std::fs;

pub mod handlers;

/// Plugin-owned runtime handle, published in `on_init`. Every spawn in server.rs /
/// servermanager.rs goes through `crate::plugin_rt()` so ds_game_server stays entirely
/// on ONE runtime (never spawning host futures on the host handle from these threads,
/// which corrupts memory). This is the only variant that connects to godotserver.
static PLUGIN_RUNTIME: OnceLock<Handle> = OnceLock::new();

pub(crate) fn plugin_rt() -> Handle {
    PLUGIN_RUNTIME
        .get()
        .expect("plugin runtime not initialized (on_init has not run yet)")
        .clone()
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
    runtime: Arc<tokio::runtime::Runtime>,
    // websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
}

impl DsGameServerPlugin {
    pub fn new() -> Self {
        info!("🔧 DsGameServerPlugin: Creating new instance");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build plugin runtime"),
        );
        Self {
            name: "ds_game_server".to_string(),
            runtime,
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
        _events: Arc<EventSystem>,
        _context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        info!("🔧 DsGameServerPlugin: Registering event handlers...");

        info!("🔧 DsGameServerPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {
        let config = ds_common::config::Config::new("ds_game_server");
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


        // TODO start the servermanager
        debug!("🔧 DsGameServerPlugin: starting servermanager");
        debug!("STEP0: Before spawning servermanager");

        // Publish the plugin runtime handle for server.rs / servermanager.rs spawns.
        if PLUGIN_RUNTIME.set(self.runtime.handle().clone()).is_err() {
            warn!("🔧 DsGameServerPlugin: plugin runtime handle already set");
        }

        let context_clone = Arc::clone(&context);
        let manager_handle = self.runtime.handle().spawn(async move {
            debug!("STEP1: Inside spawned task");
            let manager: servermanager::ServerManager = servermanager::ServerManager::new();
            manager.run(context_clone).await;
        });

        // Watchdog: surface a ServerManager panic instead of losing it silently.
        self.runtime.handle().spawn(async move {
            if let Err(e) = manager_handle.await {
                error!("❌ ServerManager panicked: {:?}", e);
            }
        });


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

use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventSystem, PlayerId, SimplePlugin, PluginError, LogLevel, ClientConnectionRef, ServerContext
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, debug};

pub mod handlers;

// Internal imports
use handlers::*;


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSession {
    pub username: String,
    pub player_id: PlayerId,
}

/// DsPlayerAuthentication Plugin
/// Authentication plugin that handles integration with external services
/// This design allows you to swap authentication providers without touching game logic
pub struct DsPlayerAuthenticationPlugin {
    name: String,
    server_state_ok: Arc<AtomicBool>,
    // Plugin-owned runtime. Event handlers fire in a context with NO ambient tokio runtime
    // (the host dispatches events inside a spawn_blocking BlockingTask), so tokio::spawn
    // panics "no reactor" and context.tokio_handle() is a dead handle. Spawning on this
    // runtime's handle works, and running handle_player_init on it also gives the downstream
    // host handler (game_server core update_player_id -> core.rs:567 bare tokio::spawn) an
    // ambient runtime so it doesn't panic.
    runtime: Arc<tokio::runtime::Runtime>,
    // event_system: Arc<EventSystem>,
    // auth_service: ExternalAuthService,
    // database_pool: sqlx::PgPool, // Your existing database connection
}

impl DsPlayerAuthenticationPlugin {
    pub fn new() -> Self {
        info!("🔧 DsPlayerAuthenticationPlugin: Creating new instance");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build plugin runtime"),
        );
        Self {
            name: "ds_player_authentication".to_string(),
            server_state_ok: Arc::new(AtomicBool::new(false)),
            runtime,
            // event_system: Arc<EventSystem>,
            // auth_service: ExternalAuthService{base_url: "https://toto".to_string(), api_key: "xxxx".to_string(), client: reqwest::Client::new()},
            // database_pool: sqlx::PgPool
        }
    }
}

#[async_trait]
impl SimplePlugin for DsPlayerAuthenticationPlugin {
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
        info!("🔧 DsPlayerAuthenticationPlugin: Registering event handlers...");
        
        let events_system = events.clone();
        // Spawn on the plugin-owned runtime handle (see struct comment). NOT tokio::spawn
        // (handler runs with no ambient runtime -> "no reactor" panic) and NOT
        // context.tokio_handle() (dead handle -> task silently never runs).
        let rt_handle = self.runtime.handle().clone();
        let server_state_ok_init = Arc::clone(&self.server_state_ok);
        let server_state_ok_ready = Arc::clone(&self.server_state_ok);

        events.on_client("player", "init", move |event: authentication::PlayerInit, player_id: PlayerId, connection: ClientConnectionRef| {
            debug!("plugin auth: Receive player init message {:?}", event);

            let events = events_system.clone();
            let event = event.clone();
            let server_state_ok = Arc::clone(&server_state_ok_init);

            let jh = rt_handle.spawn(async move {
                let _ = authentication::handle_player_init(
                    event,
                    player_id,
                    connection,
                    events,
                    server_state_ok,
                ).await;
            });
            // Watchdog: surface a panic in the init task instead of losing it silently.
            rt_handle.spawn(async move {
                if let Err(e) = jh.await {
                    tracing::error!("❌ auth handle_player_init task panicked: {:?}", e);
                }
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        
        events.on_plugin("ds_game_server", "server_registered", move |event: serde_json::Value| {
            debug!("plugin auth: Receive server_registered message {:?}", event);
            server_state_ok_ready.store(true, Ordering::Relaxed);
            info!("🔧 DsPlayerAuthenticationPlugin: server is ready, accepting player connections");
            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        info!("🔧 DsPlayerAuthenticationPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        let config = ds_common::config::Config::new("ds_player_authentication");
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

        info!("🔧 DsPlayerAuthenticationPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DsPlayerAuthenticationPlugin: Shutting down!",
        );

        // TODO: Add your cleanup logic here

        info!("🔧 DsPlayerAuthenticationPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DsPlayerAuthenticationPlugin);

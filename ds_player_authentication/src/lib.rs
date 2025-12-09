use async_trait::async_trait;
use horizon_event_system::{
    AuthenticationStatusSetEvent, EventError, Event, AuthenticationStatusGetEvent, AuthenticationStatus, create_simple_plugin, EventSystem, PlayerId, current_timestamp, RawClientMessageEvent, SimplePlugin, PluginError, LogLevel, ClientConnectionRef, ServerContext, Vec3
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info};
use serde_json::json;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInit {
    pub data: PlayerInitData,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInitData {
    pub login: String,
    pub password: String,
    pub spawn_point: i8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSession {
    pub username: String,
    pub player_id: PlayerId,
}

/// External authentication service client
/// This represents integration with your existing account system
pub struct ExternalAuthService {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct AuthValidationRequest {
    token: String,
    game_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoginRequestEvent {
    pub username: String,
    pub password_hash: String,
}


#[derive(Deserialize)]
struct AuthValidationResponse {
    valid: bool,
    player_id: Option<String>,
    permissions: Vec<String>,
    expires_at: u64,
}

impl ExternalAuthService {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

/// DsPlayerAuthentication Plugin
/// Authentication plugin that handles integration with external services
/// This design allows you to swap authentication providers without touching game logic
pub struct DsPlayerAuthenticationPlugin {
    name: String,
    // event_system: Arc<EventSystem>,
    // auth_service: ExternalAuthService,
    // database_pool: sqlx::PgPool, // Your existing database connection    
}

impl DsPlayerAuthenticationPlugin {
    pub fn new() -> Self {
        info!("🔧 DsPlayerAuthenticationPlugin: Creating new instance");
        Self {
            name: "ds_player_authentication".to_string(),
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

    async fn register_handlers(&mut self, events: Arc<EventSystem>, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("🔧 DsPlayerAuthenticationPlugin: Registering event handlers...");
        
        let events_system = events.clone();
        events.on_client("player", "init", move |event: PlayerInit, player_id: PlayerId, connection: ClientConnectionRef| {
            println!("plugin auth: Receive player init message {:?}", event);

            let events_system = events_system.clone();
            let event = event.clone();

            // Spawn a dedicated thread and runtime for the emit so we don't require
            // the current thread to be inside a Tokio runtime.
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build temp runtime");

                if event.data.login == "I am an idiot !" {
                    return;
                }


                rt.block_on(async move {
                    // send to client its uuid
                    println!("plugin auth: Emitting init_registered for player_id {:?}", player_id);

                    // TODO replace by uuid found in user database
                    let player_db_id = PlayerId::new();

                    let payload = serde_json::to_vec(&serde_json::json!({
                        "player_id": player_db_id,
                        "type": "init_ack"
                    })).expect("failed to serialize payload");

                    if let Err(e) = connection.respond(&payload).await
                    {
                        println!("plugin auth: FAILED to send init_ack to client: {}", e);
                        tracing::error!("Failed to send init_ack to client: {}", e);
                    }

                    // Update the player_id stored in the connection manager
                    // This replaces the temporary connection-level player_id with the database player_id
                    if let Err(e) = events_system
                        .emit_core("update_player_id", &serde_json::json!({
                            "old_player_id": player_id,
                            "new_player_id": player_db_id,
                            "connection_id": player_id,  // The connection_id is currently the old player_id
                        }))
                        .await
                    {
                        tracing::error!("Failed to emit update_player_id event: {}", e);
                    }

                    // send to gorcplugin (player plugin) the new player event
                    if let Err(e) = events_system
                        .emit_plugin("propsplugin", "new_player", &serde_json::json!({
                            "object_type": "player",
                            "object_uuid": player_db_id,
                            "object_data": {
                                "name": event.data.login,
                                "position": Vec3::new(0.0, 0.0, 0.0),
                                "rotation": Vec3::new(0.0, 0.0, 0.0),
                                "connection_id": player_db_id,  // Use the new player_db_id here
                                "spawn_point": event.data.spawn_point,
                            }
                        }))
                        .await
                    {
                        tracing::error!("Failed to emit plugin event to gorcplugin: {}", e);
                    }
                });
            });
            Ok::<(), EventError>(())
        }).await.unwrap();
        
        info!("🔧 DsPlayerAuthenticationPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
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

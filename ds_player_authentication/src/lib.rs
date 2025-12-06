use async_trait::async_trait;
use horizon_event_system::{
    AuthenticationStatusSetEvent, EventError, Event, AuthenticationStatusGetEvent, AuthenticationStatus, create_simple_plugin, EventSystem, PlayerId, current_timestamp, RawClientMessageEvent, SimplePlugin, PluginError, LogLevel, ClientConnectionRef, ServerContext, Vec3
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
// use tokio::time::{timeout, Duration};
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

    // /// Validates a player token against the external authentication service
    // /// This might be your existing OAuth provider, custom auth API, or third-party service
    // async fn validate_token(&self, token: &str) -> Result<AuthValidationResponse, Box<dyn std::error::Error + Send + Sync>> {
    //     let request = AuthValidationRequest {
    //         token: token.to_string(),
    //         game_id: "DyingStar".to_string(),
    //     };

    //     let response = timeout(Duration::from_secs(5), 
    //         self.client
    //             .post(&format!("{}/auth/validate", self.base_url))
    //             .header("Authorization", &format!("Bearer {}", self.api_key))
    //             .header("Content-Type", "application/json")
    //             .json(&request)
    //             .send()
    //     ).await??;

    //     if !response.status().is_success() {
    //         return Err(format!("Authentication service returned status: {}", response.status()).into());
    //     }

    //     let auth_response: AuthValidationResponse = response.json().await?;
    //     Ok(auth_response)
    // }
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


    // pub async fn authenticate_player(&self, events: Arc<EventSystem>, event: serde_json::Value) {
    //     println!("ds_player-authentication: run baby!");
    //     // let Some(playerrr_id) = event.get("player_id").and_then(|v| v.as_str());
    //     // let pid = PlayerId::from_str(playerrr_id).unwrap();

    //     // event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
    //     //     player_id: pid,
    //     //     status: AuthenticationStatus::Authenticated,
    //     //     timestamp: current_timestamp(),
    //     // }).await.map_err(|e| format!("Failed to set authenticating status: {}", e))?;
    //     // Ok(())

    // }

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
        
    //     // Handle login requests from clients
    //     // let auth_service = self.auth_service.clone();
    //     let event_system = events.clone();
    //     // let db_pool = self.database_pool.clone();
    //     // TODO: Register your event handlers here
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


                    // TODO emit event to the player_plugin for registration (connection)
                    // TODO will replace the .on_core("player_connected")
                    // TODO simplify game_server plugin to use unique player_id instead use mapping

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

            // on_client expects a synchronous Result<(), EventError>
            Ok::<(), EventError>(())
        }).await.unwrap();

            // async move {
            //     // events_system
            //     //     .emit_plugin("propsplugin", "new_player", &PlayerSession {
            //     //         username: event.data.login,
            //     //         player_id: event.player_id,
            //     //     })
            //     //     .await;
            //     let data = serde_json::json!({
            //         "position": "test",
            //         "rotation": "yolo"
            //     });
            //     println!("les DATA {:?}", data);

            //     // events_system
            //     //     .emit_client("box50cm", "spawn", &data)
            //     //     .await;
            // };


            // events_system.emit_plugin("DyingstarPropsPlugin", "new_player", &PlayerSession {
            //     username: event.data.login,
            //     player_id: event.player_id,
            // })

            // }.await();
            // events_system.emit_plugin("DyingstarPropsPlugin", "new_player", &PlayerSession {
            //     username: event.data.login,
            //     player_id: event.player_id,
            // }).await
            // .map_err(|e| PluginError::InitializationFailed(e.to_string()))?;
            // Ok(())
        // }).await.unwrap();


        // events.on_client("auth", "login", move |event: RawClientMessageEvent| {
        // events.on_client_with_connection("auth", "login",
        //     |event: LoginRequestEvent, client: ClientConnectionRef| async move {
        //         println!("ds_player-authentication: Receive auth login {:?}", event);
        //         // Object {"data": Object {
        //         //        "credentials": Object {
        //         //            "password": String("password123"),
        //         //            "username": String("admin")},
        //         //        "instance_uuid": String("12345678-1234-1234-1234-123456789abc"),
        //         //        "object_id": String("auth_session_001")
        //         //        },
        //         //        "player_id": String("36004641-5529-4de3-aedb-06c3dd014740")
        //         //   }

        //         // let auth_service = auth_service.clone();
        //         // let event_system = event_system.clone();
        //         // let db_pool = db_pool.clone();
        //         // let event = event.clone();
        //         // let pid;

        //         // if let Some(playerrr_id) = event.get("player_id").and_then(|v| v.as_str()) {
        //         //     println!("ds_player-authentication: player_id defined");
        //         //     pid = PlayerId::from_str(playerrr_id).unwrap();
        //         // } else {
        //         //     println!("ds_player-authentication: Problem with player_id :/");
        //         //     return async move { Ok(()) };
        //         // }
        //         // let pid = pid.clone();

        //         // let event_system = event_system.clone();
        //         // let event = event.clone();
        //         println!("ds_player-authentication: player_id defined");
        //         // DsPlayerAuthenticationPlugin::authenticate_player(event_system, event).await?;
        //         // async move {

        //         //     // let Some(playerrr_id) = event.get("player_id").and_then(|v| v.as_str());
        //         //     // let pid = PlayerId::from_str(playerrr_id).unwrap();
        
        //         //     println!("ds_player-authentication: run baby!");
        //         //     // event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //     //     player_id: pid,
        //         //     //     status: AuthenticationStatus::Authenticated,
        //         //     //     timestamp: current_timestamp(),
        //         //     // }).await.map_err(|e| format!("Failed to set authenticating status: {}", e))?;
        //         //     // Ok(())
        //         // };
        //             // println!("ds_player-authentication: go go in async");
        //         //     if let Some(player_id) = event.get("player_id").and_then(|v| v.as_str()) {
        //         //         println!("ds_player-authentication: player_id defined");

        //         //         // Parse login data from client
        //         //         // let login_data: serde_json::Value = serde_json::from_slice(&event.data)
        //         //         //     .map_err(|e| format!("Invalid login data: {}", e))?;
        //         //         // let token = login_data["token"].as_str()
        //         //         //     .ok_or("Missing authentication token")?;



        //         //         // Set status to authenticating immediately
        //         //         event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //             player_id: PlayerId::from_str(player_id).unwrap(),
        //         //             status: AuthenticationStatus::Authenticating,
        //         //             timestamp: current_timestamp(),
        //         //         }).await.map_err(|e| format!("Failed to set authenticating status: {}", e))?;

        //         //         // TODO we do on hard code, must implement external service
        //         //         // Set authentication status to authenticated
        //         //         event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //             player_id: PlayerId::from_str(player_id).unwrap(),
        //         //             status: AuthenticationStatus::Authenticated,
        //         //             timestamp: current_timestamp(),
        //         //         }).await.map_err(|e| format!("Failed to set authenticated status: {}", e))?;
        //         //     } else {
        //         //         println!("ds_player-authentication: Problem with player_id :/");
        //         //     }

        //         //     // TODO this is the external service to use
        //         //     // // This is where the magic happens - external service integration
        //         //     // match auth_service.validate_token(token).await {
        //         //     //     Ok(auth_response) if auth_response.valid => {
        //         //     //         // Update player data in your existing database
        //         //     //         if let Some(external_player_id) = auth_response.player_id {
        //         //     //             let _ = sqlx::query!(
        //         //     //                 "INSERT INTO player_sessions (internal_player_id, external_player_id, permissions, expires_at) 
        //         //     //                  VALUES ($1, $2, $3, $4)
        //         //     //                  ON CONFLICT (internal_player_id) DO UPDATE SET 
        //         //     //                  external_player_id = $2, permissions = $3, expires_at = $4",
        //         //     //                 event.player_id.to_string(),
        //         //     //                 external_player_id,
        //         //     //                 &auth_response.permissions,
        //         //     //                 auth_response.expires_at as i64
        //         //     //             ).execute(&db_pool).await;
        //         //     //         }

        //         //     //         // Set authentication status to authenticated
        //         //     //         event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //     //             player_id: event.player_id,
        //         //     //             status: AuthenticationStatus::Authenticated,
        //         //     //             timestamp: current_timestamp(),
        //         //     //         }).await.map_err(|e| format!("Failed to set authenticated status: {}", e))?;

        //         //     //         info!("🎉 Player {} successfully authenticated via external service", event.player_id);
        //         //     //     },
        //         //     //     Ok(_auth_response) => {
        //         //     //         // Token was processed but marked as invalid
        //         //     //         event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //     //             player_id: event.player_id,
        //         //     //             status: AuthenticationStatus::AuthenticationFailed,
        //         //     //             timestamp: current_timestamp(),
        //         //     //         }).await.map_err(|e| format!("Failed to set failed status: {}", e))?;

        //         //     //         warn!("🔒 Authentication failed for player {} - invalid token", event.player_id);
        //         //     //     },
        //         //     //     Err(e) => {
        //         //     //         // Network error, service unavailable, etc.
        //         //     //         event_system.emit_core("auth_status_set", &AuthenticationStatusSetEvent {
        //         //     //             player_id: event.player_id,
        //         //     //             status: AuthenticationStatus::AuthenticationFailed,
        //         //     //             timestamp: current_timestamp(),
        //         //     //         }).await.map_err(|e| format!("Failed to set failed status: {}", e))?;

        //         //     //         error!("❌ Authentication service error for player {}: {}", event.player_id, e);
        //         //     //     }
        //         //     // }
                    
        //             // Ok::<(), String>(())
        //         //     Ok(())
        //         // };
        //         // info!("OKkkkkkk: {:?}", event);
        //         Ok(())
        //     }
        // ).await?;
        // .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // Handle session refresh/validation requests
        // let db_pool = self.database_pool.clone();
        // events.on_client("auth", "refresh", move |event: RawClientMessageEvent, client| {
        //     let db_pool = db_pool.clone();
        //     async move {
        //         // Only allow refresh if currently authenticated
        //         if !client.is_authenticated() {
        //             warn!("🔒 Refresh attempt from unauthenticated player: {}", client.player_id);
        //             return Ok(());
        //         }

        //         // Check session expiration in database
        //         let session = sqlx::query!(
        //             "SELECT expires_at FROM player_sessions WHERE internal_player_id = $1",
        //             event.player_id.to_string()
        //         ).fetch_optional(&db_pool).await.map_err(|e| format!("Database error: {}", e))?;

        //         if let Some(session) = session {
        //             let now = current_timestamp();
        //             if session.expires_at < now as i64 {
        //                 // Session expired - this could trigger re-authentication
        //                 warn!("⏰ Session expired for player {}", client.player_id);
        //                 // Could emit AuthenticationStatusSetEvent with AuthenticationFailed here
        //             }
        //         }

        //         Ok(())
        //     };
        //     Ok(())
        // }).await?;
        //     Ok(())
        // }).await.unwrap();
        




        
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

use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, current_timestamp, register_handlers, AuthenticationStatus, AuthenticationStatusSetEvent, AuthenticationStatusGetEvent, AuthenticationStatusGetResponseEvent, AuthenticationStatusChangedEvent, EventSystem, LogLevel, PlayerConnectedEvent, PluginError, ServerContext, SimplePlugin, SubscriptionManager, PlayerMovementEvent, ClientEventWrapper
};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, warn};
use horizon_event_system::{Vec3, PlayerId};
use uuid::{uuid, Uuid};

/// Dyingstar Plugin
pub struct DyingstarPlugin {
    name: String,
}

impl DyingstarPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarPlugin: Creating new instance");
        Self {
            name: "dyingstar".to_string(),
        }
    }
}

#[async_trait]
impl SimplePlugin for DyingstarPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn register_handlers(&mut self, events: Arc<EventSystem>, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("🔧 DyingstarPlugin: Registering event handlers...");
        
        // TODO: Register your event handlers here
        // Example:
        let events_for_connected = events.clone();
        let context_clone = context.clone();
        let events_clone = events.clone();

        events.on_core("player_connected", move |event: PlayerConnectedEvent| {
            println!("dyingstar: New player connected! {:?}", event);
            // let subscription_manager = SubscriptionManager::new();
            // let rt = tokio::runtime::Runtime::new().unwrap();
            // rt.block_on(subscription_manager.add_player(event.player_id, Vec3::new(0.0, 2000.0, 0.0).into()));

            // let auth_event = AuthenticationStatusSetEvent {
            //     player_id: event.player_id,
            //     status: AuthenticationStatus::Authenticating,
            //     timestamp: current_timestamp(),
            // };

            // // This should not panic and should serialize correctly
            // let events_for_c = events_for_connected.clone();
            // if let Ok(handle) = tokio::runtime::Handle::try_current() {
            //     println!("dyingstar: On passe la :D");
            //     handle.block_on(async {
            //         let tt = events_for_c.emit_core("auth_status_set", &auth_event).await;
            //         println!("dyingstar: yolo {:?}", tt);
            //     });
            // };

            // let auth_event = AuthenticationStatusSetEvent {
            //     player_id: event.player_id,
            //     status: AuthenticationStatus::Authenticated,
            //     timestamp: current_timestamp(),
            // };
            // // This should not panic and should serialize correctly
            // let events_for_c = events_for_connected.clone();
            // tokio::spawn(async move {
            //     let _ = events_for_c.emit_core("auth_status_set", &auth_event).await;
            // });

            Ok(())
        }).await.unwrap();

        events.on_core("player_disconnected", |event: serde_json::Value| {
            println!("dyingstar: Player disconnected. Farewell! {:?}", event);
            Ok(())
        }).await.unwrap();

        events
            .on_client_with_connection(
                "movement",
                "update_position",
                move |wrapper: ClientEventWrapper<serde_json::Value>, _connection| {
                    context_clone.log(LogLevel::Info, format!("📝 LoggerPlugin: 🦘 Client movement from player {}", wrapper.player_id).as_str(),);
                    // println!("📝 LoggerPlugin: 🦘 Client movement");
                    // Parse the movement data

                    #[derive(serde::Deserialize)]
                    struct PlayerMovementData {
                        pos: Vec3,
                        rot: Vec3
                    }

                    match serde_json::from_value::<PlayerMovementData>(wrapper.data.clone()) {
                        Ok(movement_data) => {
                            // Convert Transform to Vec3 for core event
                            let new_position = horizon_event_system::Vec3 {
                                x: movement_data.pos.x,
                                y: movement_data.pos.y,
                                z: movement_data.pos.z,
                            };
                            println!("Test of the position {:?}", new_position);

                            // Create and emit core movement event for GORC and other systems
                            let core_movement_event = PlayerMovementEvent {
                                player_id: wrapper.player_id,
                                old_position: None,
                                new_position,
                                timestamp: current_timestamp(),
                            };

                            let events_system = events_clone.clone();
                            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                                handle.block_on(async move {
                                    if let Err(e) = events_system
                                        .emit_core("player_movement", &core_movement_event)
                                        .await
                                    {
                                        // Best effort - don't fail if core event emission fails
                                        println!("Error send to core {:?}", e)
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            context_clone.log(
                                LogLevel::Error,
                                format!("📝 LoggerPlugin: Failed to parse movement: {}", e)
                                    .as_str(),
                            );
                        }
                    }
                    Ok(())
                },
            )
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        // Event "movement", "move_direction"
        // will send to game server

        
        info!("🔧 DyingstarPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPlugin: Starting up!",
        );

        // TODO: Add your initialization logic here
        
        info!("🔧 DyingstarPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPlugin: Shutting down!",
        );

        // TODO: Add your cleanup logic here

        info!("🔧 DyingstarPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DyingstarPlugin);

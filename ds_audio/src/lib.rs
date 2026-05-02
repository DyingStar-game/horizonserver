use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventSystem, LogLevel, PlayerId, PluginError, ServerContext,
    SimplePlugin,
};
use livekit_api::access_token::{AccessToken, VideoGrants};
use livekit_api::services::room::{CreateRoomOptions, RoomClient};
use serde_json::json;
use dashmap::DashMap;
use std::env;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::{error, info, warn};
use uuid::Uuid;

mod livekit;
mod zone;

use crate::livekit::LiveKitRoom;
use crate::zone::ProximityManager;

/// DyingstarAudio Plugin
pub struct DyingstarAudioPlugin {
    name: String,
    /// Internal HTTP URL for the LiveKit server API (http:// or https://)
    /// Only used in on_init, so a plain String is fine.
    livekit_api_url: String,
    /// Public WebSocket URL sent to game clients (ws:// or wss://).
    /// Shared via Arc<Mutex> because register_handlers captures it before on_init sets it.
    livekit_public_url: Arc<Mutex<String>>,
    /// Shared via Arc<Mutex> for the same reason as livekit_public_url.
    api_key: Arc<Mutex<String>>,
    api_secret: Arc<Mutex<String>>,
    manager: Arc<ProximityManager>,
    /// Maps game player_id → server-generated LiveKit identity UUID.
    /// DashMap is internally sharded — no outer Mutex needed.
    player_to_livekit: Arc<DashMap<String, String>>,
    /// Dedicated runtime for plugin async work (bridges cross-DLL runtime boundary)
    runtime: Arc<tokio::runtime::Runtime>,
}

impl DyingstarAudioPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarAudioPlugin: Creating new instance");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build DyingstarAudioPlugin runtime"),
        );

        let public_url = env::var("LIVEKIT_URL")
            .unwrap_or_else(|_| "ws://192.168.49.2:30188".to_string());
        let api_url = env::var("LIVEKIT_API_URL")
            .unwrap_or_else(|_| "http://livekit:7880".to_string());
        let api_key = env::var("LIVEKIT_API_KEY")
            .unwrap_or_else(|_| "devkey".to_string());
        let api_secret = env::var("LIVEKIT_API_SECRET")
            .unwrap_or_else(|_| "devsecret-replace-me-32-chars-min".to_string());

        let lk = Arc::new(
            LiveKitRoom::new(&api_url, &api_key, &api_secret, "universe")
                .expect("failed to create LiveKitRoom"),
        );
        let manager = Arc::new(ProximityManager::new(lk));

        Self {
            name: "dyingstar_audio".to_string(),
            livekit_api_url: api_url,
            livekit_public_url: Arc::new(Mutex::new(public_url)),
            api_key: Arc::new(Mutex::new(api_key)),
            api_secret: Arc::new(Mutex::new(api_secret)),
            manager,
            player_to_livekit: Arc::new(DashMap::new()),
            runtime,
        }
    }
}

#[async_trait]
impl SimplePlugin for DyingstarAudioPlugin {
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
        info!("🔧 DyingstarAudioPlugin: Registering event handlers...");

        // Clone the Arcs — the closures will lock them at call time, after on_init has set values.
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let livekit_public_url = Arc::clone(&self.livekit_public_url);
        let tokio_handle = context.tokio_handle();
        let tokio_handle_enter = tokio_handle.clone();
        let tokio_handle_exit = tokio_handle.clone();
        let events_send = events.clone(); // used to get the real ClientResponseSender at call time
        let events_send_enter = events.clone();
        let events_send_exit = events.clone();
        // Use the plugin's own runtime for livekit HTTP calls — avoids cross-DLL
        // "no reactor running" panic when hyper resolves DNS on a foreign tokio binary.
        let runtime_enter = Arc::clone(&self.runtime);
        let runtime_exit = Arc::clone(&self.runtime);
        let runtime_quit = Arc::clone(&self.runtime);
        let player_to_livekit_update = Arc::clone(&self.player_to_livekit);
        let player_to_livekit_new = Arc::clone(&self.player_to_livekit);
        let player_to_livekit_enter = Arc::clone(&self.player_to_livekit);
        let player_to_livekit_exit = Arc::clone(&self.player_to_livekit);
        let player_to_livekit_quit = Arc::clone(&self.player_to_livekit);

        // Pre-register the livekit_uuid as soon as the real player_id is assigned
        // (update_player_id fires before gorc_zone_entered, eliminating the race).
        events
            .on_core("update_player_id", move |event: serde_json::Value| {
                let new_player_id = event["new_player_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if new_player_id.is_empty() {
                    return Ok(());
                }
                // Only insert if not already present (new_player may also insert later).
                player_to_livekit_update
                    .entry(new_player_id.clone())
                    .or_insert_with(|| {
                        let livekit_uuid = Uuid::new_v4().to_string();
                        info!(
                            "🔊 DyingstarAudioPlugin: update_player_id pre-registered player_id={} livekit_uuid={}",
                            new_player_id, livekit_uuid
                        );
                        livekit_uuid
                    });
                Ok(())
            })
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        events
            .on_plugin(
                "plugingameserver",
                "new_player",
                move |event: serde_json::Value| {
                    let player_uuid = event["object_uuid"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let player_name = event["object_data"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();

                    // Ensure uuid is in the map (may already be set by update_player_id handler).
                    let livekit_uuid = player_to_livekit_new
                        .entry(player_uuid.clone())
                        .or_insert_with(|| {
                            let uuid = Uuid::new_v4().to_string();
                            info!(
                                "🔊 DyingstarAudioPlugin: new_player (late) player_uuid={} livekit_uuid={}",
                                player_uuid, uuid
                            );
                            uuid
                        })
                        .clone();
                    info!(
                        "🔊 DyingstarAudioPlugin: new_player player_uuid={} livekit_uuid={}",
                        player_uuid, livekit_uuid
                    );

                    // Read the credentials at call time — on_init has set them by now.
                    let api_key = api_key.lock().unwrap().clone();
                    let api_secret = api_secret.lock().unwrap().clone();
                    let livekit_public_url = livekit_public_url.lock().unwrap().clone();

                    let events_send = events_send.clone();
                    tokio_handle.spawn(async move {
                        // Generate a LiveKit JWT for this player to join universe room
                        let token = match AccessToken::with_api_key(&api_key, &api_secret)
                            .with_identity(&livekit_uuid)
                            .with_name(&player_name)
                            .with_grants(VideoGrants {
                                room_join: true,
                                room: "universe".to_string(),
                                can_publish: true,       // publish their own mic
                                can_subscribe: true,
                                can_publish_data: true,
                                ..Default::default()
                            })
                            .to_jwt()
                        {
                            Ok(t) => t,
                            Err(e) => {
                                error!(
                                    "🔊 DyingstarAudioPlugin: Failed to generate LiveKit token for player {}: {}",
                                    player_uuid, e
                                );
                                return;
                            }
                        };

                        // Parse the player UUID as a PlayerId to target the right client
                        let player_id = match PlayerId::from_str(&player_uuid) {
                            Ok(id) => id,
                            Err(_) => {
                                error!(
                                    "🔊 DyingstarAudioPlugin: Invalid PlayerId for player_uuid: {}",
                                    player_uuid
                                );
                                return;
                            }
                        };

                        // Send the token directly to this player's WebSocket connection only.
                        let data = serde_json::to_vec(&json!({
                            "event_type": "livekit_token",
                            "livekit_url": livekit_public_url,
                            "livekit_identity": livekit_uuid,
                            "token": token,
                            "room": "universe",
                        })).unwrap_or_default();
                        match events_send.get_client_response_sender() {
                            Some(sender) => {
                                if let Err(e) = sender.send_to_client(player_id, data).await {
                                    error!(
                                        "🔊 DyingstarAudioPlugin: Failed to send livekit_token to player {}: {}",
                                        player_uuid, e
                                    );
                                } else {
                                    info!(
                                        "🔊 DyingstarAudioPlugin: ✅ Sent LiveKit token to player {}",
                                        player_uuid
                                    );
                                }
                            }
                            None => {
                                error!("🔊 DyingstarAudioPlugin: No client response sender — cannot send token to player {}", player_uuid);
                            }
                        }
                    });

                    Ok(())
                },
            )
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let manager_enter = self.manager.clone();
        events
            .on_core("gorc_zone_entered", move |event: serde_json::Value| {
                let player_id   = event["player_id"].as_str().unwrap_or_default();
                let object_id   = event["object_id"].as_str().unwrap_or_default();
                let object_type = event["object_type"].as_str().unwrap_or_default();
                let channel     = event["channel"].as_u64().unwrap_or_default();

                // e.g. only react when a player enters another Player's zone:
                if object_type == "player" && player_id != object_id {
                    // Own the strings before borrowing event ends
                    let player_id = player_id.to_string();
                    let object_id = object_id.to_string();

                    info!("🔊 DyingstarAudioPlugin - gorc_zone_entered: player_id={} object_id={} object_type={} channel={}", player_id, object_id, object_type, channel);

                    let lk_player = match player_to_livekit_enter.get(&player_id) {
                        Some(v) => v.clone(),
                        None => {
                            warn!("🔊 DyingstarAudioPlugin: gorc_zone_entered: no livekit_uuid for player_id={}", player_id);
                            return Ok(());
                        }
                    };
                    let lk_object = match player_to_livekit_enter.get(&object_id) {
                        Some(v) => v.clone(),
                        None => {
                            warn!("🔊 DyingstarAudioPlugin: gorc_zone_entered: no livekit_uuid for object_id={}", object_id);
                            return Ok(());
                        }
                    };

                    let m = manager_enter.clone();
                    let lk_object_sub = lk_object.clone();
                    let lk_player_sub = lk_player.clone();
                    runtime_enter.spawn(async move {
                        if let Err(e) = m.on_zone_enter(&lk_player, &lk_object).await {
                            error!("🔊 DyingstarAudioPlugin: on_zone_enter failed: {}", e);
                        }
                    });

                    let events_send_enter = events_send_enter.clone();
                    let object_id_enter = object_id.clone();
                    tokio_handle_enter.spawn(async move {
                        match events_send_enter.get_client_response_sender() {
                            Some(sender) => {
                                // Notify player_id about object_id's LiveKit identity.
                                let data = serde_json::to_vec(&json!({
                                    "event_type": "livekit_subscribe",
                                    "participant_uuid": lk_object_sub,
                                    "player_id": &player_id,
                                })).unwrap_or_default();
                                match PlayerId::from_str(&player_id) {
                                    Ok(pid) => {
                                        if let Err(e) = sender.send_to_client(pid, data).await {
                                            error!(
                                                "🔊 DyingstarAudioPlugin: Failed to send livekit_subscribe to player {}: {}",
                                                player_id, e
                                            );
                                        } else {
                                            info!(
                                                "🔊 DyingstarAudioPlugin: ✅ Sent livekit_subscribe to player {} for participant {}",
                                                player_id, lk_object_sub
                                            );
                                        }
                                    }
                                    Err(_) => {
                                        error!("🔊 DyingstarAudioPlugin: Invalid PlayerId for player_id={}", player_id);
                                    }
                                }
                                // Also notify object_id about player_id's LiveKit identity.
                                let data2 = serde_json::to_vec(&json!({
                                    "event_type": "livekit_subscribe",
                                    "participant_uuid": lk_player_sub,
                                    "player_id": &object_id_enter,
                                })).unwrap_or_default();
                                match PlayerId::from_str(&object_id_enter) {
                                    Ok(oid) => {
                                        if let Err(e) = sender.send_to_client(oid, data2).await {
                                            error!(
                                                "🔊 DyingstarAudioPlugin: Failed to send livekit_subscribe to player {}: {}",
                                                object_id_enter, e
                                            );
                                        } else {
                                            info!(
                                                "🔊 DyingstarAudioPlugin: ✅ Sent livekit_subscribe to player {} for participant {}",
                                                object_id_enter, lk_player_sub
                                            );
                                        }
                                    }
                                    Err(_) => {
                                        error!("🔊 DyingstarAudioPlugin: Invalid PlayerId for object_id={}", object_id_enter);
                                    }
                                }
                            }
                            None => {
                                error!("🔊 DyingstarAudioPlugin: No client response sender — cannot send livekit_subscribe to players {} / {}", player_id, object_id_enter);
                            }
                        };
                    });
                }
                Ok(())
            })
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let manager_exit = self.manager.clone();
        events
            .on_core("gorc_zone_exited", move |event: serde_json::Value| {
                let player_id   = event["player_id"].as_str().unwrap_or_default();
                let object_id   = event["object_id"].as_str().unwrap_or_default();
                let object_type = event["object_type"].as_str().unwrap_or_default();
                let channel     = event["channel"].as_u64().unwrap_or_default();
                // e.g. only react when a player exits another Player's zone:
                if object_type == "player" && player_id != object_id {
                    let player_id = player_id.to_string();
                    let object_id = object_id.to_string();

                    let lk_player = match player_to_livekit_exit.get(&player_id) {
                        Some(v) => v.clone(),
                        None => {
                            warn!("🔊 DyingstarAudioPlugin: gorc_zone_exited: no livekit_uuid for player_id={}", player_id);
                            return Ok(());
                        }
                    };
                    let lk_object = match player_to_livekit_exit.get(&object_id) {
                        Some(v) => v.clone(),
                        None => {
                            warn!("🔊 DyingstarAudioPlugin: gorc_zone_exited: no livekit_uuid for object_id={}", object_id);
                            return Ok(());
                        }
                    };

                    let m = manager_exit.clone();
                    let lk_object_sub = lk_object.clone();
                    let lk_player_sub = lk_player.clone();
                    runtime_exit.spawn(async move {
                        if let Err(e) = m.on_zone_exit(&lk_player, &lk_object).await {
                            error!("🔊 DyingstarAudioPlugin: on_zone_exit failed: {}", e);
                        }
                    });

                    let events_send_exit = events_send_exit.clone();
                    let object_id_exit = object_id.clone();
                    tokio_handle_exit.spawn(async move {
                        match events_send_exit.get_client_response_sender() {
                            Some(sender) => {
                                // Notify player_id to unsubscribe from object_id.
                                let data = serde_json::to_vec(&json!({
                                    "event_type": "livekit_unsubscribe",
                                    "participant_uuid": lk_object_sub,
                                    "player_id": &player_id,
                                })).unwrap_or_default();
                                match PlayerId::from_str(&player_id) {
                                    Ok(pid) => {
                                        if let Err(e) = sender.send_to_client(pid, data).await {
                                            error!(
                                                "🔊 DyingstarAudioPlugin: Failed to send livekit_unsubscribe to player {}: {}",
                                                player_id, e
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "🔊 DyingstarAudioPlugin: Invalid player_id {}: {}",
                                            player_id, e
                                        );
                                    }
                                }
                                // Also notify object_id to unsubscribe from player_id.
                                let data2 = serde_json::to_vec(&json!({
                                    "event_type": "livekit_unsubscribe",
                                    "participant_uuid": lk_player_sub,
                                    "player_id": &object_id_exit,
                                })).unwrap_or_default();
                                match PlayerId::from_str(&object_id_exit) {
                                    Ok(oid) => {
                                        if let Err(e) = sender.send_to_client(oid, data2).await {
                                            error!(
                                                "🔊 DyingstarAudioPlugin: Failed to send livekit_unsubscribe to player {}: {}",
                                                object_id_exit, e
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "🔊 DyingstarAudioPlugin: Invalid object_id {}: {}",
                                            object_id_exit, e
                                        );
                                    }
                                }
                            }
                            None => {
                                error!("🔊 DyingstarAudioPlugin: No client response sender — cannot send livekit_unsubscribe to players {} / {}", player_id, object_id_exit);
                            }
                        };
                    });
                }
                Ok(())
            })
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        let manager_quit = self.manager.clone();
        events
            .on_plugin("gameserverplugin", "player_quit", move |event: serde_json::Value| {
                let player_uuid = event["item"]["object_uuid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();

                if let Some((_, lk_uuid)) = player_to_livekit_quit.remove(&player_uuid) {
                    info!(
                        "🔊 DyingstarAudioPlugin: player_quit player_uuid={} livekit_uuid={}",
                        player_uuid, lk_uuid
                    );
                    let m = manager_quit.clone();
                    runtime_quit.spawn(async move {
                        m.on_player_leave(&lk_uuid).await;
                    });
                } else {
                    warn!(
                        "🔊 DyingstarAudioPlugin: player_quit: no livekit_uuid for player_uuid={}",
                        player_uuid
                    );
                }

                Ok(())
            })
            .await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        info!("🔧 DyingstarAudioPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        let config = ds_common::config::Config::new("ds_audio");
        let log_level = config
            .get_value("log_level")
            .and_then(|v| v.as_str())
            .unwrap_or("info");

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

        info!("🔧 DyingstarAudioPlugin: Starting up!");

        info!("🔧 DyingstarAudioPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "🔧 DyingstarAudioPlugin: Shutting down!");
        info!("🔧 DyingstarAudioPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DyingstarAudioPlugin);

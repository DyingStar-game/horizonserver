use async_trait::async_trait;
use chrono::Utc;
use horizon_event_system::{
    create_complete_horizon_system, CompressionType, create_simple_plugin, defObject, EventSystem, PlayerId, LogLevel, PluginError, ReplicationLayer, ReplicationPriority, ServerContext, SimplePlugin, Vec3, PlayerDisconnectedEvent, ClientConnectionRef, GorcObject, GorcEvent, Dest
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
pub mod props;
use crate::props::testplanet::Testplanet;
use crate::props::player::Player;
use crate::props::box50cm::Box50cm;
use crate::props::storagewarehouse::{randomize_storage_warehouse, StorageType, get_item_scene_path};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSession {
    pub username: String,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPlayerDataObjectData {
    pub name: String,
    pub position: Vec3,
    pub rotation: Vec3,
    pub connection_id: PlayerId,
    pub spawn_point: i8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPlayerData {
    pub object_type: String,
    pub object_uuid: PlayerId,
    pub object_data: NewPlayerDataObjectData,
}

/// DyingstarProps Plugin
pub struct DyingstarPropsPlugin {
    name: String,
    boxes50cm: Arc<RwLock<HashMap<String, Box50cm>>>,
    planets: Arc<RwLock<HashMap<String, Testplanet>>>,
    // object_registry: Arc<GorcObjectRegistry>,
    players: Arc<RwLock<HashMap<PlayerId, Player>>>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl DyingstarPropsPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarPropsPlugin: Creating new instance");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build plugin runtime"),
        );
        Self {
            name: "dyingstar_props".to_string(),
            boxes50cm: Arc::new(RwLock::new(HashMap::new())),
            planets: Arc::new(RwLock::new(HashMap::new())),
            // object_registry: Arc::new(GorcObjectRegistry::new()),
            players: Arc::new(RwLock::new(HashMap::new())),
            runtime,
        }
    }

    pub async fn get_initial_props_for_server(&self) {
        let sandbox = Testplanet::new("Sandbox".to_string(), Vec3::new(15067000000.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 0.0));
        {
            let mut planets = self.planets.write().await;
            planets.insert(sandbox.uuid.clone(), sandbox.clone());
        }
    }
}

#[async_trait]
impl SimplePlugin for DyingstarPropsPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn register_handlers(&mut self, events: Arc<EventSystem>, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("🔧 DyingstarPropsPlugin: Registering event handlers...");

        // Enter the plugin runtime only for the synchronous call that needs a reactor.
        // Drop the EnterGuard before any .await so the register_handlers future remains Send.
        let (gevents, mut gorc_system) = {
            let _enter = self.runtime.handle().enter();
            create_complete_horizon_system(context.clone())
        }.map_err(|e| PluginError::ExecutionError(format!("failed to create complete horizon system: {}", e)))?;
 
        // clone the plugin runtime so sync handlers can spawn tasks onto it without requiring
        // a reactor on the current thread.
        let runtime_for_handlers = self.runtime.clone();
        // separate clone for the client spawn_request handler (each handler should capture its own Arc<Runtime>)
        let runtime_for_spawn = self.runtime.clone();
        // create per-handler runtime clones so each `move` closure takes its own Arc and doesn't move the same value twice
        let runtime_for_new_player = self.runtime.clone();
        let runtime_for_players_update = self.runtime.clone();
        let runtime_for_disconnect = self.runtime.clone();
        let runtime_for_props_update = self.runtime.clone();


        // create per-handler clones of the GORC system so closures don't move the same value
        let gorc_for_new_player = gorc_system.clone();
        let gorc_for_spawn = gorc_system.clone();
        let gorc_for_players_update = gorc_system.clone();
        let gorc_for_disconnect = gorc_system.clone();
        let gorc_for_props_update = gorc_system.clone();

        // register_handlers runs inside an async runtime — spawn tasks directly with tokio::spawn
        // (remove the previous runtime construction/Handle logic)

        // on_plugin expects a synchronous callback returning Result<_, EventError>.
        // spawn an async task to perform async work inside the handler.
        let players_clone = self.players.clone();
        let planets_clone = self.planets.clone();
        let events_clone = events.clone();
        events.on_plugin("propsplugin", "new_player", move |event: NewPlayerData| {
            let players = players_clone.clone();
            let events = events_clone.clone();
            let runtime = runtime_for_new_player.clone();
            runtime.spawn(async move {
                println!("PROP Receive new player: {:?}", event);
                info!("🔧 DyingstarPropsPlugin: ✅ New player connected: {} ({})", event.object_data.name, event.object_data.connection_id);

                // TODO get player from persistence service
                
                // Set default coordinates based on spawn_point
                let spawn_point = event.object_data.spawn_point;
                let (base_x, base_y, base_z, parent_uuid) = match spawn_point {
                    1 => (10500.0, 0.5, 10500.0, "ed20bda3-f6f3-4053-b9de-968f73ebc44c".to_string()), // Sandbox surface => city
                    2 => (-2422100.0, 100.0, 0.0, "9f29bc8f-c01d-4bfc-a781-a38a70807da3".to_string()), // Sandbox
                    3 => (0.0, 3.0, -152.0, "b9d2e503-0adb-4add-919f-85aaff65be0f".to_string()), // moon 5_1 => storage warehouse 1
                    4 => (-1200100.0, 0.0, 0.0, "5ef9afed-e754-4410-8087-691619c7e776".to_string()), // moon 5_1
                    _ => (-2422100.0, 100.0, 0.0, "9f29bc8f-c01d-4bfc-a781-a38a70807da3".to_string()),
                };


                // store in variable z the number of players and multiply it by 2.0
                // The goal is to not spawn in same place (temporary code)
                let player_count = players.read().await.len() as f64;
                let row = (player_count / 20.0).floor();
                let col = player_count % 20.0;
                let z = col * 5.0;
                let y = row * 5.0;

                let player = props::player::Player::new(
                    event.object_data.name.clone(),
                    Vec3::new(base_x, base_y + y, base_z + z),
                    // Vec3::new(-2125000.667, 249.366 + y, 6000.0 + z), // City
                    Vec3::new(0.0, 0.0, 0.0),
                    event.object_uuid.clone(),
                );

                // Send to pluginplayer for gorc integration
                if let Err(e) = events
                    .emit_plugin("pluginplayer", "new_player", &serde_json::json!({
                        "object_type": "player",
                        "object_uuid": player.uuid,
                        "object_data": {
                            "name": player.name,
                            "position": player.position,
                            "rotation": player.rotation,
                            "connection_id": event.object_data.connection_id,
                            "parent_id": parent_uuid,
                            // "parent_id": "65345350-5a40-4f44-a3c1-0ca5641cb97a", // Tarsis 5
                            // "parent_id": "c27c3d25-cdeb-4fef-a794-30f684fd8f67", // tarsis_5_2

                        }
                    }))
                    .await
                {
                    tracing::error!("Failed to emit plugin event to player plugin: {}", e);
                }

                players.write().await.insert(player.uuid, player.clone());
            });
            Ok(())
        }).await.unwrap();

        // no runtime cloning needed; use tokio::spawn in handlers

        // // create fresh clones — each handler must capture its own Arc so we don't move the same value into multiple closures
        // let boxes50cm_for_spawn = self.boxes50cm.clone();
        // let boxes50cm_for_updatepos = self.boxes50cm.clone();
        let events_for_spawn = events.clone();
        // // spawn requests will use tokio::spawn

        // we receive spawn_request from godot client (press key for example)
        // we will spawn a genericobject (box50cm, box4m, ship, etc)
        events.on_client("props", "spawn_request", move |event: serde_json::Value, _player_id: PlayerId, _connection: ClientConnectionRef| {
            // prepare clones/local copies used by the async task so they are moved, not the outer variables
            let events = events_for_spawn.clone();


            // prepare clones for the async task
            let events_task = events.clone();
            let event_task = event.clone();
            let runtime = runtime_for_spawn.clone();
            
            runtime.spawn(async move {
                // 1/ generate an uuid
                let uuid = uuid::Uuid::new_v4().to_string();

                // 2/ send to plugin genericobject for creation in gorc
                // TODO: perhaps change the "object_data" with the data deceived to be dynamic, to test
                if let Err(e) = events_task.emit_plugin("genericprops", "create_object", &serde_json::json!({
                    "object_type": event_task["data"]["entity"],
                    "object_uuid": uuid,
                    "object_data": {
                        "position": event_task["data"]["position"],
                        "rotation": {"x":0.0, "y": 0.0, "z":0.0},
                        "scenename": event_task["data"]["scenename"],
                        "parent_id": event_task["data"]["parent_id"],
                    } 
                })).await {
                    tracing::error!("Failed to emit plugin event to genericprops: {}", e);
                }
            });

            // keep original `event` available for sync logging (we cloned for the task)
            println!("PROP (sync) Receive spawn_request: {:?}", event);
            Ok(())
        }).await.unwrap();


        let events_clone2 = events.clone();
        // clone players map for the players_position_update handler so we don't capture &mut self
        let players_for_players_update = self.players.clone();

        // prepare clones for player-disconnected handler (no await in sync closure)
        let players_for_disconnect = self.players.clone();
        let events_for_disconnect = events.clone();
        let events_clone3 = events.clone();

        // Spawn the GORC tick loop in a dedicated OS thread with its own current-thread Tokio runtime.
        // This avoids requiring the gorc tick future to be `Send`.
        {
            // take ownership of gorc_system
            let mut gorc_loop = gorc_system;
            std::thread::Builder::new()
                .name("dyingstar-gorc-loop".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build gorc loop runtime");

                    rt.block_on(async move {
                        loop {
                            // Process GORC replication
                            if let Err(e) = gorc_loop.tick().await {
                                error!("GORC tick error: {}", e);
                            }
                            
                            // print result of gorc_loop.get_stats()
                            // let stats = gorc_loop.get_stats().await;
                            // println!("GORC stats: {:?}", stats);

                            // Run at ~60Hz
                            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
                        }
                    });
                })
                .expect("failed to spawn gorc loop thread");
        }

        info!("🔧 DyingstarPropsPlugin: ✅ All handlers registered successfully!");
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

        info!("🔧 DyingstarPropsPlugin: Starting up!");

        // wait 2 seconds to finish all plugins initialized

        self.runtime.spawn(async move {
            println!("Waiting 2 seconds before emitting planet object...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            println!("Send init to gameserver plugin to connect to GORC server...");
            if let Err(e) = context.events().emit_plugin("gameserverplugin", "init_server", &serde_json::json!({})).await {
                error!("Failed to emit plugin event: {}", e);
                return;
            }

            // wait 2 seconds, time to connect to the first game server
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let result: Result<(), PluginError> = async {
                let serverinfo_uuid = Uuid::new_v4().to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "serverinfo",
                "object_uuid": serverinfo_uuid,
                "object_data": {
                    "position": {"x": 0.0, "y": 0.0, "z": 0.0},
                    "name": "serverinfo",
                    "scenename": "",
                    "fps": 60,
                    "objects_number": 0,
                    "players_number": 0
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                info!("Waiting 2 seconds before emitting planet object...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                info!("Send init to gameserver plugin to connect to GORC server...");
                context.events().emit_plugin("gameserverplugin", "init_server", &serde_json::json!({})).await
                .map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // wait 2 seconds, time to connect to the first game server
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                context.events().emit_plugin(
                    "externalservices",
                    "resourcesdynamic",
                    &serde_json::json!({
                        "event_type": "init",
                        "data": {
                            "system_internal_name": "tarsis",
                            "duration_s": 1,
                            "frequency": 2,
                            "from_timestamp": Utc::now().timestamp(),
                        }
                    }),
                ).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // wait 10 seconds, time to planets spawned
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;

                // spawn the city
                let city_uuid = "ed20bda3-f6f3-4053-b9de-968f73ebc44c".to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "city",
                "object_uuid": city_uuid,
                "object_data": {
                    "name": "city",
                    "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                    "scenename": "scenes/props/city/sandbox_capital.tscn",
                    "position": {"x": -2122000.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // Spawn a box50cm for testing
                let box50cm_uuid = Uuid::new_v4().to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "box",
                "object_uuid": box50cm_uuid,
                "object_data": {
                    "name": "box50cm_test",
                    "parent_id": city_uuid,
                    "scenename": "scenes/props/testbox/box_50cm.tscn",
                    "position": {"x": 10500.0, "y": 0.5, "z": 10510.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0},
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;


                /////////////////////////////////////////////////////////////////////////////////////////////////////////
                /// Code for storagewarehouse object spawning
                /////////////////////////////////////////////////////////////////////////////////////////////////////////

                // spawn the storagewarehouse
                let storagewarehouse_uuid = "b9d2e503-0adb-4add-919f-85aaff65be0f".to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "storagewarehouse",
                "object_uuid": storagewarehouse_uuid,
                "object_data": {
                    "name": "storagewarehouse",
                    "parent_id": "5ef9afed-e754-4410-8087-691619c7e776", // Moon 5_1
                    "scenename": "scenes/props/StorageBoxes/storagewarehouse.tscn",
                    "position": {"x": -854100.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // spawn dynamic boxes inside the storagewarehouse

                // 15 rangees de containers, jusqu'a  4 de hauteur

                // 1 rangee de container = 3 palettes (largeur) jusqu'a 9 de hauteur. en longeur 10 palettes

                // scenes/props/StorageBoxes/container_liquid_1200x240x240.tscn
                // scenes/props/StorageBoxes/container_standard_a_1200x240x240.tscn
                // scenes/props/StorageBoxes/container_standard_b_1200x240x240.tscn
                // scenes/props/StorageBoxes/pallet_benne_120x80x100.tscn
                // scenes/props/StorageBoxes/pallet_crate_120x80x100.tscn
                // scenes/props/StorageBoxes/pallet_liquid_120x80x100.tscn

                // generate the boxes
                let mut boxes_counter = 0;
                for row in 0..17 {
                    if row == 7 || row == 8 || row == 9 {
                        continue;
                    }
                    let storage_config = randomize_storage_warehouse();
                    let row_offset = row as f32 * 3.0;

                    info!("🔧 DyingstarPropsPlugin: Generated storage configuration: {:?}", storage_config.storage_type);
                    info!("🔧 DyingstarPropsPlugin: Number of items to spawn: {}", storage_config.items.len());
                    
                    // Base position for the storage warehouse
                    let base_x = -16.675;
                    let base_y = 0.0;
                    let base_z = -26.06 + row_offset;
                    
                    // Spawn each item in the storage configuration
                    for item in storage_config.items {
                        let item_uuid = Uuid::new_v4().to_string();
                        let scene_path = get_item_scene_path(storage_config.storage_type, &item.item_type);
                        
                        // Calculate position based on storage type and item position
                        let (pos_x, pos_y, pos_z) = match storage_config.storage_type {
                            StorageType::Container => {
                                // Linear positioning for containers
                                // Spacing: 13 units between containers
                                // size = 1200x240x240
                                let y_offset = item.position.2 as f32 * 2.4;
                                (6.0 + base_x, base_y + y_offset, 1.5 + base_z)
                            }
                            StorageType::Pallet => {
                                // Grid positioning for pallets
                                // line (depth), column (width), height
                                // size = 120x80x100
                                let line_spacing = 1.4; // 10 lines with spacing
                                let column_spacing = 1.0; // 3 columns with spacing
                                let height_spacing = 1.0; // vertical stacking

                                let x_offset = item.position.0 as f32 * line_spacing;
                                let z_offset = item.position.1 as f32 * column_spacing;
                                let y_offset = item.position.2 as f32 * height_spacing;
                                
                                (0.6 + base_x + x_offset, base_y + y_offset, 0.5 + base_z + z_offset)
                            }
                        };
                        
                        // Spawn the item
                        // TODO with gorc position problem, we not parent to storagewarehouse but to the planet directly
                        let message = &serde_json::json!({
                            "object_type": "box",
                            "object_uuid": item_uuid,
                            "object_data": {
                                "name": format!("storage_{}_{}", 
                                    match storage_config.storage_type {
                                        StorageType::Container => "container",
                                        StorageType::Pallet => "pallet",
                                    },
                                    item.item_type
                                ),
                                // "parent_id": storagewarehouse_uuid,
                                "parent_id": storagewarehouse_uuid,
                                "scenename": scene_path,
                                "position": {"x": pos_x, "y": pos_y, "z": pos_z},
                                "rotation": {"x": 0.0, "y": 0.0, "z": 0.0},
                                "led_state": false,
                                "weight": 10.0,
                            }
                        });
                        boxes_counter += 1;
                        context.events().emit_plugin("genericprops", "create_object", message).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;
                    }
                }
                
                info!("🔧 DyingstarPropsPlugin: ✅ Storage warehouse items ({} boxes) spawned successfully!", boxes_counter);

                Ok(())
            }.await;
            if let Err(e) = result {
                error!("Error in initialization async task: {}", e);
            }
        });
        
        info!("🔧 DyingstarPropsPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPropsPlugin: Shutting down!",
        );

        // TODO: Add your cleanup logic here

        info!("🔧 DyingstarPropsPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DyingstarPropsPlugin);

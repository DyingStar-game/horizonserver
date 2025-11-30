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

    // async fn setup_object_registry(&self) -> Result<(), String> {
    //     // Register Box5ocm object types
    //     Box50cm::register_with_gorc(self.object_registry.clone()).await
    //         .map_err(|e| e.to_string())?;
        
    //     let objects = self.object_registry.list_objects().await;
    //     info!("📦 Registered GORC objects: {:?}", objects);
        
    //     Ok(())
    // }

    // async fn setup_gorc_handlers(&self, events: Arc<EventSystem>) -> Result<(), PluginError> {
    //     // Register GORC event handlers for Box50cm objects
    //     events.on_gorc_instance("Box50cm", 2, "cosmetic_update", |event: GorcEvent, _instance| {
    //         info!("✨ Box50cm cosmetic update: {}", event.object_id);
    //         Ok(())
    //     }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;

    //     Ok(())
    // }
    
    // async fn demonstrate_object_replication(&self, events: Arc<EventSystem>) -> Result<(), String> {
    //     // Create a box50cm and demonstrate replication
    //     let box50cm = Box50cm::new(Vec3::new(500.0, 100.0, 300.0), Vec3::new(0.0, 0.0, 0.0));
    //     let box50cm_id = "box50cm_001".to_string();
        
    //     let mut boxes = self.boxes50cm.write().await;
    //     // let mut planets = self.planets.write().await;

    //     boxes.insert(box50cm_id.clone(), box50cm.clone());

    //     let critical_data = box50cm.serialize_for_layer(&ReplicationLayer::new(
    //         0, 100.0, 60.0, vec!["position".to_string()], CompressionType::None
    //     )).map_err(|e| format!("Serialization error: {}", e))?;


    //     // Emit GORC events for the box
    //     events.emit_gorc("Box50cm", 1, "mineral_scan", &GorcEvent {
    //         object_id: box50cm_id.clone(),
    //         instance_uuid: format!("box50cm_instance_{}", box50cm_id),
    //         object_type: "Box50cm".to_string(),
    //         channel: 1,
    //         data: critical_data,
    //         priority: "High".to_string(),
    //         timestamp: std::time::SystemTime::now()
    //             .duration_since(std::time::UNIX_EPOCH)
    //             .map_err(|e| e.to_string())?
    //             .as_secs(),
    //     }).await.map_err(|e| e.to_string())?;


    //     // Load Sandbox planet
    //     // let sandbox = Testplanet::new("Sandbox".to_string(), Vec3::new(15067000000.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 0.0));
    //     // planets.insert(sandbox.uuid.clone(), sandbox.clone());
        

    //     info!("✨ Demonstrated object replication for Box50cm");
    //     Ok(())
    // }

    pub async fn get_initial_props_for_server(&self) {
        let sandbox = Testplanet::new("Sandbox".to_string(), Vec3::new(15067000000.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 0.0));
        {
            let mut planets = self.planets.write().await;
            planets.insert(sandbox.uuid.clone(), sandbox.clone());
        }
    }

    // get player position and all arrounding props in dgraph database
    // pub async fn get_initial_props_to_player(&self, session: &PlayerSession) -> Player {
    //     info!("🔧 DyingstarPropsPlugin: initial props for player {} ({:?})", session.username, session.player_id);
    //     // instantiate player with playersession data
    //     let player = props::player::Player::new(
    //         session.username.clone(), 
    //         Vec3::new(15067000000.0, 12000.0, 0.0), 
    //         Vec3::new(0.0, 0.0, 0.0),
    //         session.player_id.to_string(),
    //         "".to_string(),
    //     );
    //     self.players.write().await.insert(session.player_id.clone(), player.clone());
    //     player
    // }
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
        //     let planets = planets_clone.clone();
            let events = events_clone.clone();
            // use the per-handler clone captured above
        //     let mut gorc_system = gorc_for_new_player.clone();
            let runtime = runtime_for_new_player.clone();
            runtime.spawn(async move {
                println!("PROP Receive new player: {:?}", event);
                info!("🔧 DyingstarPropsPlugin: ✅ New player connected: {} ({})", event.object_data.name, event.object_data.connection_id);

                // TODO get player from persistence service
                
                // store in variable z the number of players and multiply it by 2.0
                // The goal is to not spawn in same place (temporary code)
                let player_count = players.read().await.len() as f64;
                let row = (player_count / 20.0).floor();
                let col = player_count % 20.0;
                let z = col * 4.0;
                let y = row * 4.0;

                let player = props::player::Player::new(
                    event.object_data.name.clone(),
                    Vec3::new(-2422100.0, 0.0 + y, 0.0 + z),
                    // Vec3::new(-2125000.667, 249.366 + y, 6000.0 + z), // City
                    Vec3::new(0.0, 0.0, 0.0),
                    event.object_uuid.clone(),
                );

                // Send to pluginplayer for gorc integration
                if let Err(e) = events
                    .emit_plugin("gorcplugin", "new_player", &serde_json::json!({
                        "object_type": "player",
                        "object_uuid": player.uuid,
                        "object_data": {
                            "name": player.name,
                            "position": player.position,
                            "rotation": player.rotation,
                            "connection_id": event.object_data.connection_id,
                            "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                            // "parent_id": "65345350-5a40-4f44-a3c1-0ca5641cb97a", // Tarsis 5
                            // "parent_id": "85927094-ccd5-4d21-9980-ee087cc46ce8", // tarsis_5_2

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

                // 3/ send to game_server plugin to spawn on godot server for the collision / physics calculations
                if let Err(e) = events_task.emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
                    "object_type": event_task["data"]["entity"],
                    "object_uuid": uuid,
                    "object_data": {
                        "position": event_task["data"]["position"],
                        "rotation": {"x":0.0, "y": 0.0, "z":0.0},
                        "scenename": event_task["data"]["scenename"],
                        "parent_id": event_task["data"]["parent_id"],
                    } 
                })).await {
                    tracing::error!("Failed to emit plugin event to gameserverplugin: {}", e);
                }
            });








            // // clone the event and the boxes Arc for the spawned async task
            // let event_task = event.clone();
            // let boxes_for_task = boxes50cm_for_spawn.clone();

            // let runtime = runtime_for_spawn.clone();
            // let mut gorc_system = gorc_for_spawn.clone();
            // runtime.spawn(async move {
            //     // check if event["type"] == "box50cm" or "box4m" or "ship" with match
            //     match event_task["data"]["type"].as_str().unwrap_or("") {
            //         "box50cm" => {
            //             // println!("SPAWN BOX50CM YEAH");
            //             // spawn box50cm
            //             // create Box50cm and store it
            //             let mut box50cm = Box50cm::new(
            //                 Vec3::new(0.0, 0.0, 0.0),
            //                 Vec3::new(0.0, 0.0, 0.0),
            //                 "".to_string(),
            //             );
            //             let box50cm_id = uuid::Uuid::new_v4().to_string();


            //             let gorc_id = gorc_system.register_object(box50cm.clone(), box50cm.position.clone()).await;
            //             box50cm.gorc_id = Some(gorc_id);

            //             // store box in boxes50cm (use the cloned Arc inside async task)
            //             {
            //                 let mut boxes = boxes_for_task.write().await;
            //                 boxes.insert(box50cm_id.clone(), box50cm.clone());
            //             }

            //             let payload = serde_json::json!({
            //                 "box50cm": box50cm.clone(),
            //                 "player_uuid": event_task["data"]["player_uuid"].as_str().unwrap_or(""),
            //             });

            //             if let Err(e) = events.emit_plugin("gameserverplugin", "add_prop", &payload).await {
            //                 tracing::error!("Failed to emit plugin event to propsplugin, add_prop: {}", e);
            //             }
            //         },
            //         "box4m" => {
            //             // spawn box4m
            //         },
            //         "ship" => {
            //             // spawn ship
            //         },
            //         _ => {
            //             error!("Unknown prop type: {}", event_task["type"]);
            //         }
            //     }
            // });

            // keep original `event` available for sync logging (we cloned for the task)
            println!("PROP (sync) Receive spawn_request: {:?}", event);
            Ok(())
        }).await.unwrap();


        let events_clone2 = events.clone();
        // clone players map for the players_position_update handler so we don't capture &mut self
        let players_for_players_update = self.players.clone();
        // use tokio::spawn in this handler
        // events.on_plugin("propsplugin", "players_position_update", move |event: serde_json::Value| {
        //     // Clone the incoming event for the spawned task so we don't move `event`
        //     // out of the sync handler closure.
        //     let event_task = event.clone();

        //     // TODO update position and rotation of the player (use `event_task` if needed)

        //     // broadcast new position to all clients
        //     let events = events_clone2.clone();
        //     let runtime = runtime_for_players_update.clone();
        //     // clone the players Arc here (per-call) so we don't move the captured Arc into the async task
        //     let players_map_arc = players_for_players_update.clone();
        //     runtime.spawn(async move {
        //         let announcement = serde_json::json!({
        //             "type": "update_props",
        //             "planets": serde_json::json!([]),
        //             "players": event_task["players"],
        //         });

        //         // loop on players in event and update server state
        //         for player in event_task["players"].as_array().unwrap() {
        //             let target_uuid = player["uuid"].as_str().unwrap_or("");
        //             let new_pos = Vec3::new(
        //                 player["pos"]["x"].as_f64().unwrap_or(0.0),
        //                 player["pos"]["y"].as_f64().unwrap_or(0.0),
        //                 player["pos"]["z"].as_f64().unwrap_or(0.0),
        //             );

        //             // acquire write lock to mutate Player entries
        //             let mut players_map = players_map_arc.write().await;
        //             for (_id, p) in players_map.iter_mut() {
        //                 if p.uuid == target_uuid {
        //                     // send emit_gorc_client to update player position
        //                     let event = serde_json::json!({
        //                         "type": "update_player_position",
        //                         "player_uuid": p.internal_uuid.clone(),
        //                         "pos": {
        //                             "x": new_pos.x,
        //                             "y": new_pos.y,
        //                             "z": new_pos.z,
        //                         },
        //                     });

        //                     // // notify EventSystem about the player position
        //                     // if let Err(e) = events.update_player_position(p.hs_player_id, new_pos.clone()).await {
        //                     //     error!("Failed to update player position via EventSystem: {}", e);
        //                     // }
        //                     // // println!("Updating position for player {} to {:?}", p.uuid, new_pos);
        //                     // events.emit_client_with_context("test", "test", p.internal_uuid, &event).await;

        //                     // update local player object
        //                     p.position = new_pos;
        //                 }
        //             }
        //         }
        //     });

        //     Ok(())
        // }).await.unwrap();

        // prepare clones for player-disconnected handler (no await in sync closure)
        let players_for_disconnect = self.players.clone();
        let events_for_disconnect = events.clone();

        // events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
        //     // move clones into the handler
        //     let players = players_for_disconnect.clone();
        //     let events = events_for_disconnect.clone();
        //     // we'll spawn an async task with tokio::spawn

        //     let internal_uuid = event.player_id.clone();

        //     // spawn async task to use .await inside
        //     let runtime = runtime_for_disconnect.clone();
        //     runtime.spawn(async move {
        //         // println!("PROP Player disconnected event: {:?}", event);
        //         // println!("PROP Player disconnected, list of players {:?}", players.read().await);
        //         // acquire write lock to remove the player
        //         let mut players_map = players.write().await;
        //         // loop on players_map for player have the internal_uuid = internal_uuid
        //         for (uuid, player) in players_map.iter() {
        //             if player.internal_uuid == internal_uuid {
        //                 println!("Found player: {:?}", player);
        //                 // send to all clients the player disconnected
        //                 let payload = serde_json::json!({
        //                     "type": "delete_player",
        //                     "player_uuid": player.uuid.clone(),
        //                 });
        //                 println!("Broadcasting player disconnected: {:?}", payload);
        //                 if let Err(e) = events.broadcast(&payload).await {
        //                     error!("Failed to broadcast event: {}", e);
        //                 }
        //             }
        //         }
        //     });

        //     Ok(())
        // }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        let events_clone3 = events.clone();
        // props update from game server
        // events.on_plugin("propsplugin", "props_position_update", move |event: serde_json::Value| {
        //     // clone event for the spawned task
        //     let event_task = event.clone();
        //     let events = events_clone3.clone();
        //     let runtime = runtime_for_props_update.clone();
        //     let boxes_for_updatepos = boxes50cm_for_updatepos.clone();
        //    // use the per-handler GORC clone (do not use the original gorc_system here)
        //    let gorc = gorc_for_props_update.clone();
        //     runtime.spawn(async move {
        //         let announcement = serde_json::json!({
        //             "type": "props_position_update",
        //             "props": event_task["props"],
        //         });

        //         // loop on event_task["props"] that is Vec of objects
        //         for prop in event_task["props"].as_array().unwrap() {
        //             match prop["type"].as_str().unwrap_or("") {
        //                 "box50cm" => {
        //                     // "uuid": uuid,
        //                     // "pos": {
        //                     // 	"x": convert_value_to_universe(position[0], POSITION_CONVERSION_X),
        //                     // 	"y": convert_value_to_universe(position[1], POSITION_CONVERSION_Y),
        //                     // 	"z": convert_value_to_universe(position[2], POSITION_CONVERSION_Z)
        //                     // },
        //                     // "rot": {
        //                     // 	"x": rotation[0],
        //                     // 	"y": rotation[1],
        //                     // 	"z": rotation[2]
        //                     // },
        //                     // "type": type,                            

        //                     // acquire a write lock so we can mutate the Box50cm
        //                     let mut boxes = boxes_for_updatepos.write().await;
        //                     if let Some(box50cm) = boxes.get_mut(prop["uuid"].as_str().unwrap_or("")) {
        //                         box50cm.update_position(
        //                             Vec3::new(
        //                                 prop["pos"]["x"].as_f64().unwrap_or(0.0),
        //                                 prop["pos"]["y"].as_f64().unwrap_or(0.0),
        //                                 prop["pos"]["z"].as_f64().unwrap_or(0.0),
        //                             )
        //                         );

        //                         // Update GORC object position via EventSystem API
        //                         if let Some(gorc_id) = box50cm.gorc_id.as_ref() {
        //                             // events.update_object_position expects (object_id, new_position)
        //                             if let Err(e) = events.update_object_position(gorc_id.clone(), box50cm.position.clone()).await {
        //                                 error!("Failed to update GORC object position via EventSystem: {}", e);
        //                             }
        //                         } else {
        //                             error!("Box50cm has no gorc_id, cannot update position");
        //                         }

        //                     } else {
        //                         error!("Box50cm with uuid {} not found for position update", prop["uuid"]);
        //                     }
        //                 },
        //                 // 
        //                 // "box4m" => {
        //                 //     // println!("Update position for box4m {:?}", prop);
        //                 // },
        //                 // "ship" => {
        //                 //     // println!("Update position for ship {:?}", prop);
        //                 // },
        //                 _ => {
        //                     error!("Unknown prop type in position update: {}", prop["type"]);
        //                 }
        //             }
        //         }

        //         // if let Err(e) = events.broadcast(&announcement).await {
        //         //     error!("Failed to broadcast event: {}", e);
        //         // }
        //     });

        //     Ok(())
        // }).await.unwrap();

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

        // TODO: Add your initialization logic here


        // wait 10 seconds to finish all plugins initialized

        self.runtime.spawn(async move {
            println!("Waiting 4 seconds before emitting planet object...");
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;

            println!("Send init to gameserver plugin to connect to GORC server...");
            if let Err(e) = context.events().emit_plugin("gameserverplugin", "init_server", &serde_json::json!({})).await {
                error!("Failed to emit plugin event: {}", e);
                return;
            }

            // wait 2 seconds, time to connect to the first game server
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // println!("Emitting planet object to genericprops...");
            // context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
            //     "object_type": "planet",
            //     "object_uuid": "3388a817-f3ef-421d-b10f-4325e105628e",
            //     "object_data": {
            //         "name": "Sandbox",
            //         "scenename": "scenes/planet/tarsis_IV.tscn",
            //         "position": {"x": 10000000.0, "y": 0.0, "z": 0.0},
            //         // "position": {"x":-34289753828.218235, "y": 572788198.6034999, "z":36200805980.425224},
            //         "rotation": {"x":0.0, "y": 0.0, "z":0.0},
            //     } 
            // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

            // println!("Emitting second planet object to genericprops...");
            // context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
            //     "object_type": "planet",
            //     "object_uuid": "6f3b006e-a6e3-493b-ba3b-57a180a09cc5",
            //     "object_data": {
            //         "name": "tarsis II",
            //         "scenename": "scenes/planet/tarsis_II.tscn",
            //         "position": {"x": 0.0, "y": 10000000.0, "z": 10000000.0},
            //         "rotation": {"x":0.0, "y": 0.0, "z":0.0},
            //     } 
            // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

            // // to gameserver
            // println!("Emitting planet object to gameserver...");
            // context.events().emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
            //     "object_type": "planet",
            //     "object_uuid": "3388a817-f3ef-421d-b10f-4325e105628e",
            //     "object_data": {
            //         "name": "Sandbox",
            //         "scenename": "scenes/planet/tarsis_IV.tscn",
            //         "position": {"x": 10000000.0, "y": 0.0, "z": 0.0},
            //         "rotation": {"x":0.0, "y": 0.0, "z":0.0},
            //     } 
            // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

            // println!("Emitting second planet object to gameserver...");
            // context.events().emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
            //     "object_type": "planet",
            //     "object_uuid": "6f3b006e-a6e3-493b-ba3b-57a180a09cc5",
            //     "object_data": {
            //         "name": "tarsis II",
            //         "scenename": "scenes/planet/tarsis_II.tscn",
            //         "position": {"x": 0.0, "y": 10000000.0, "z": 10000000.0},
            //         "rotation": {"x":0.0, "y": 0.0, "z":0.0},
            //     } 
            // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))
            let result: Result<(), PluginError> = async {
                let serverinfo_uuid = Uuid::new_v4().to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "serverinfo",
                "object_uuid": serverinfo_uuid,
                "object_data": {
                    "name": "serverinfo",
                    "scenename": "",
                    "fps": 60,
                    "objects_number": 0,
                    "players_number": 0
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                context.events().emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
                "object_type": "serverinfo",
                "object_uuid": serverinfo_uuid,
                "object_data": {
                    "name": "serverinfo",
                    "scenename": "",
                    "fps": 60,
                    "objects_number": 0,
                    "players_number": 0
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;


                info!("Waiting 4 seconds before emitting planet object...");
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;

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
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;

                // spawn the city
                // let city_uuid = Uuid::new_v4().to_string();
                // context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                // "object_type": "city",
                // "object_uuid": city_uuid,
                // "object_data": {
                //     "name": "city",
                //     "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                //     "scenename": "scenes/props/city/sandbox_capital.tscn",
                //     "position": {"x": -2122000.0, "y": 0.0, "z": 0.0},
                //     "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                // }
                // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // context.events().emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
                // "object_type": "city",
                // "object_uuid": city_uuid,
                // "object_data": {
                //     "name": "city",
                //     "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                //     "scenename": "scenes/props/city/sandbox_capital.tscn",
                //     "position": {"x": -2122000.0, "y": 0.0, "z": 0.0},
                //     "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                // }
                // })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                /////////////////////////////////////////////////////////////////////////////////////////////////////////
                /// Code for storagewarehouse object spawning
                /////////////////////////////////////////////////////////////////////////////////////////////////////////

                // spawn the storagewarehouse
                let storagewarehouse_uuid = Uuid::new_v4().to_string();
                context.events().emit_plugin("genericprops", "create_object", &serde_json::json!({
                "object_type": "storagewarehouse",
                "object_uuid": storagewarehouse_uuid,
                "object_data": {
                    "name": "storagewarehouse",
                    "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                    "scenename": "scenes/props/StorageBoxes/storagewarehouse.tscn",
                    "position": {"x": -2422000.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                context.events().emit_plugin("gameserverplugin", "spawn_object", &serde_json::json!({
                "object_type": "storagewarehouse",
                "object_uuid": storagewarehouse_uuid,
                "object_data": {
                    "name": "storagewarehouse",
                    "parent_id": "9f29bc8f-c01d-4bfc-a781-a38a70807da3", // Sandbox
                    "scenename": "scenes/props/StorageBoxes/storagewarehouse.tscn",
                    "position": {"x": -2422000.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 1.5708},
                }
                })).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;

                // spawn dynamic boxes inside the storagewarehouse

                // 15 rangees de containers, jusqu'a  4 de hauteur

                // 1 rangee de container = 3 palettes (largeur) jusqu'a 9 de hauteur. en longeur 10 palettes

// -16.675 0.0 -26.06
// -16.675 0.0 -22.927


// 21.706 0.0 -29.52
// 21.706 0.0 -28.52







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
                        context.events().emit_plugin("gameserverplugin", "spawn_object", message).await.map_err(|e| PluginError::ExecutionError(format!("failed to emit plugin event: {}", e)))?;
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

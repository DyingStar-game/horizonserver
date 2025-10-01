use async_trait::async_trait;
use horizon_event_system::{
    CompressionType, create_simple_plugin, defObject, EventSystem, PlayerId, LogLevel, PluginError, ReplicationLayer, ReplicationPriority, ServerContext, SimplePlugin, Vec3, PlayerDisconnectedEvent
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSession {
    pub username: String,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPlayerData {
    pub username: String,
    pub uuid: String,
    pub internal_uuid: String,
}

/// DyingstarProps Plugin
pub struct DyingstarPropsPlugin {
    name: String,
    boxes50cm: Arc<RwLock<HashMap<String, Box50cm>>>,
    planets: Arc<RwLock<HashMap<String, Testplanet>>>,
    // object_registry: Arc<GorcObjectRegistry>,
    players: Arc<RwLock<HashMap<PlayerId, Player>>>,
}

impl DyingstarPropsPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarPropsPlugin: Creating new instance");
        Self {
            name: "dyingstar_props".to_string(),
            boxes50cm: Arc::new(RwLock::new(HashMap::new())),
            planets: Arc::new(RwLock::new(HashMap::new())),
            // object_registry: Arc::new(GorcObjectRegistry::new()),
            players: Arc::new(RwLock::new(HashMap::new())),
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
    pub async fn get_initial_props_to_player(&self, session: &PlayerSession) -> Player {
        info!("🔧 DyingstarPropsPlugin: initial props for player {} ({:?})", session.username, session.player_id);
        // instantiate player with playersession data
        let player = props::player::Player::new(
            session.username.clone(), 
            Vec3::new(15067000000.0, 12000.0, 0.0), 
            Vec3::new(0.0, 0.0, 0.0),
            session.player_id.to_string(),
            "".to_string(),
        );
        self.players.write().await.insert(session.player_id.clone(), player.clone());
        player
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

    async fn register_handlers(&mut self, events: Arc<EventSystem>, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("🔧 DyingstarPropsPlugin: Registering event handlers...");

        // Obtain the current Tokio runtime handle once (register_handlers runs inside runtime)
        // Try to use the current Tokio runtime if available. If not, create one and keep it alive.
        let mut owned_runtime: Option<Arc<tokio::runtime::Runtime>> = None;
        let rt_handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                let rt = Arc::new(
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| PluginError::ExecutionError(format!("failed to create runtime: {}", e)))?,
                );
                let handle = rt.handle().clone();
                owned_runtime = Some(rt);
                handle
            }
        };

        // Make distinct clones of the runtime handle for each closure so none of them
        // takes ownership of the original `rt_handle`.
        let rt_handle_for_new_player = rt_handle.clone();
        let rt_handle_for_position_update = rt_handle.clone();

        // on_plugin expects a synchronous callback returning Result<_, EventError>.
        // spawn a tokio task to perform async work inside the handler.
        let players_clone = self.players.clone();
        let planets_clone = self.planets.clone();
        let events_clone = events.clone();
        // clone owned_runtime to keep it alive inside the closure if we created one
        let owned_runtime_clone = owned_runtime.clone();
        events.on_plugin("propsplugin", "new_player", move |event: NewPlayerData| {
            let players = players_clone.clone();
            let planets = planets_clone.clone();
            let events = events_clone.clone();
            // use the captured runtime handle (may point to an existing runtime or the owned one)
            let rt = rt_handle_for_new_player.clone();
            // keep the owned runtime alive for the lifetime of the spawned task (if any)
            let _owned_rt = owned_runtime_clone.clone();
            rt.spawn(async move {
                println!("PROP Receive new player: {:?}", event);
                info!("🔧 DyingstarPropsPlugin: ✅ New player connected: {} ({})", event.username, event.uuid);

                let mut new_players: Vec<Player> = Vec::new();
                let mut first_player: bool = false;

                // if players list is empty -> create server initial planets inline (avoid calling self)
                if players.read().await.len() == 0 {
                    first_player = true;
                    // create sandbox planet and store it
                    let sandbox = Testplanet::new(
                        "Sandbox".to_string(),
                        Vec3::new(19000098785.898, 13339.8, -10386.2), // Vec3::new(15067000000.0, 0.0, 0.0),
                        Vec3::new(0.0, 0.0, 0.0),
                    );
                    planets.write().await.insert(sandbox.uuid.clone(), sandbox.clone());
                }

                // create player and store it
                
                // store in variable z the number of players and multiply it by 10.0
                let z = players.read().await.len() as f64 * 10.0;

                let player = props::player::Player::new(
                    event.username.clone(),
                    Vec3::new(18999588785.9, 13339.8, (-10386.2 + z)), // Vec3::new(15067000000.0, 12000.0, z),
                    Vec3::new(0.0, 0.0, 0.0),
                    event.internal_uuid.clone(),
                    event.uuid.clone(),
                );
                players.write().await.insert(PlayerId::from_str(&player.uuid).unwrap(), player.clone());
                new_players.push(player.clone());

                if first_player {
                    let payload = serde_json::json!({
                        "planets": planets.read().await.values().cloned().collect::<Vec<Testplanet>>(),
                        "player": player.clone(),
                    });

                    if let Err(e) = events.emit_plugin("gameserverplugin", "init_server", &payload)
                        .await
                    {
                        tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                } else {
                    let payload = serde_json::json!({
                        "player": player.clone(),
                    });

                    if let Err(e) = events.emit_plugin("gameserverplugin", "add_props", &payload)
                        .await
                    {
                        tracing::error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                }
                

                // send all props to the new client
                // let props = serde_json::json!({
                //     "type": "player_props",
                //     "planets": planets.read().await.values().cloned().collect::<Vec<Testplanet>>(),
                //     "players": players.read().await.values().cloned().collect::<Vec<Player>>(),
                // });
                // TODO
                // if let Err(e) = events.send_to_player(&event.player_id, &props).await {
                //     error!("Failed to send props to new player: {}", e);
                // }

                // send new props to all clients
                let announcement = serde_json::json!({
                    "type": "player_props", //"new_props",
                    "planets": planets.read().await.values().cloned().collect::<Vec<Testplanet>>(),
                    // "player": player.clone(),
                    "players": players.read().await.values().cloned().collect::<Vec<Player>>(),
                });

                if let Err(e) = events.broadcast(&announcement).await {
                    error!("Failed to broadcast event: {}", e);
                }


                // emit plugin event to gameserverplugin using the same EventSystem
                if let Err(e) = events.emit_plugin("gameserverplugin", "send_props", &announcement).await {
                    error!("Failed to emit plugin event to gameserverplugin: {}", e);
                }
            });

            // return immediately to the event system
            Ok(())
        }).await.unwrap();

        let rt_handle_for_new_player = rt_handle.clone();
        let rt_handle_for_position_update = rt_handle.clone();

        // create fresh clones for the spawn_request handler (avoid moving same Arc into multiple closures)
        let boxes50cm_for_spawn = self.boxes50cm.clone();
        let events_for_spawn = events.clone();
        let owned_runtime_for_spawn = owned_runtime.clone();

        events.on_client("props", "spawn_request", move |event: serde_json::Value| {
            // prepare clones/local copies used by the async task so they are moved, not the outer variables
            let events = events_for_spawn.clone();
            let rt = rt_handle_for_new_player.clone();
            let _owned_rt = owned_runtime_for_spawn.clone();

            // clone the event and the boxes Arc for the spawned async task
            let event_task = event.clone();
            let boxes_for_task = boxes50cm_for_spawn.clone();

            rt.spawn(async move {
                // check if event["type"] == "box50cm" or "box4m" or "ship" with match
                match event_task["data"]["type"].as_str().unwrap_or("") {
                    "box50cm" => {
                        println!("SPAWN BOX50CM YEAH");
                        // spawn box50cm
                        // create Box50cm and store it
                        let box50cm = Box50cm::new(
                            Vec3::new(0.0, 0.0, 0.0),
                            Vec3::new(0.0, 0.0, 0.0),
                            "".to_string(),
                        );
                        let box50cm_id = uuid::Uuid::new_v4().to_string();

                        // store box in boxes50cm (use the cloned Arc inside async task)
                        {
                            let mut boxes = boxes_for_task.write().await;
                            boxes.insert(box50cm_id.clone(), box50cm.clone());
                        }

                        let payload = serde_json::json!({
                            "box50cm": box50cm.clone(),
                            "player_uuid": event_task["data"]["player_uuid"].as_str().unwrap_or(""),
                        });

                        if let Err(e) = events.emit_plugin("gameserverplugin", "add_prop", &payload).await {
                            tracing::error!("Failed to emit plugin event to propsplugin, add_prop: {}", e);
                        }
                    },
                    "box4m" => {
                        // spawn box4m
                    },
                    "ship" => {
                        // spawn ship
                    },
                    _ => {
                        error!("Unknown prop type: {}", event_task["type"]);
                    }
                }
            });

            // keep original `event` available for sync logging (we cloned for the task)
            println!("PROP (sync) Receive spawn_request: {:?}", event);
            Ok(())
        }).await.unwrap();


        let events_clone2 = events.clone();
        let owned_runtime_clone2 = owned_runtime.clone();
        // use the separate clone for the second handler
        let rt_handle2 = rt_handle_for_position_update.clone();
        events.on_plugin("propsplugin", "players_position_update", move |event: serde_json::Value| {
 
            // TODO update position and rotation of the player
            // println!("PROP Receive player position update: {:?}", event);
            // search in players the player has uuid of event["player"]["player_id"]
            // let player_id = event["player_id"].as_str().unwrap_or("");
            // let new_position = event["player"]["pos"].as_array().unwrap_or(&vec![]);
            // let new_rotation = event["player"]["rot"].as_array().unwrap_or(&vec![]);
            // if player_id != "" && new_position.len() == 3 && new_rotation.len() == 3 {
            //     let mut players = players_clone.write();
            //     if let Some(player) = players.get_mut(player_id) {
            //         player.position = Vec3::new(
            //             new_position[0],
            //             new_position[1],
            //             new_position[2],
            //         );
            //         player.rotation = Vec3::new(
            //             new_rotation[0],
            //             new_rotation[1],
            //             new_rotation[2],
            //         );
            //         // println!("Updated player position: {:?}", player);
            //     }
            // }


            // broadcast new position to all clients
            let events = events_clone2.clone();
            let rt = rt_handle2.clone();
            let _owned_rt = owned_runtime_clone2.clone();
            rt.spawn(async move {
                let announcement = serde_json::json!({
                    "type": "update_props",
                    "planets": serde_json::json!([]),
                    "players": event["players"],
                });

                if let Err(e) = events.broadcast(&announcement).await {
                    error!("Failed to broadcast event: {}", e);
                }
            });

            Ok(())
        }).await.unwrap();

        // prepare clones for player-disconnected handler (no await in sync closure)
        let players_for_disconnect = self.players.clone();
        let events_for_disconnect = events.clone();
        let rt_handle_for_disconnect = rt_handle_for_position_update.clone();
        let owned_runtime_for_disconnect = owned_runtime.clone();

        events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
            // move clones into the handler
            let players = players_for_disconnect.clone();
            let events = events_for_disconnect.clone();
            let rt = rt_handle_for_disconnect.clone();
            let _owned_rt = owned_runtime_for_disconnect.clone();

            let internal_uuid = event.player_id.clone();

            // spawn async task to use .await inside
            rt.spawn(async move {
                // println!("PROP Player disconnected event: {:?}", event);
                // println!("PROP Player disconnected, list of players {:?}", players.read().await);
                // acquire write lock to remove the player
                let mut players_map = players.write().await;
                // loop on players_map for player have the internal_uuid = internal_uuid
                for (uuid, player) in players_map.iter() {
                    if player.internal_uuid == internal_uuid.to_string() {
                        println!("Found player: {:?}", player);
                        // send to all clients the player disconnected
                        let payload = serde_json::json!({
                            "type": "delete_player",
                            "player_uuid": player.uuid.clone(),
                        });
                        println!("Broadcasting player disconnected: {:?}", payload);
                        if let Err(e) = events.broadcast(&payload).await {
                            error!("Failed to broadcast event: {}", e);
                        }
                    }
                }
            });

            Ok(())
        }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;


        let events_clone3 = events.clone();
        let owned_runtime_clone3 = owned_runtime.clone();
        // use the separate clone for the second handler
        let rt_handle3 = rt_handle_for_position_update.clone();
        events.on_plugin("propsplugin", "props_position_update", move |event: serde_json::Value| {
            let events = events_clone3.clone();
            let rt = rt_handle3.clone();
            let _owned_rt = owned_runtime_clone3.clone();
            rt.spawn(async move {
                let announcement = serde_json::json!({
                    "type": "props_position_update",
                    "props": event["props"],
                });

                if let Err(e) = events.broadcast(&announcement).await {
                    error!("Failed to broadcast event: {}", e);
                }
            });

            Ok(())
        }).await.unwrap();

        info!("🔧 DyingstarPropsPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPropsPlugin: Starting up!",
        );

        // TODO: Add your initialization logic here
        
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

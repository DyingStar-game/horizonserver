
use crate::handlers::{spawn_player, spawn_prop, player_movement, player_action, initial_objects};

use horizon_event_system::{
    ClientConnectionRef, ClientEventWrapper, EventError, EventSystem, GorcObjectId, PlayerId, PluginError, ServerContext, Vec3, events
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};
use std::net::TcpStream;
use websocket::message::OwnedMessage;
use websocket::sender::Writer;
use websocket::receiver::Reader;
use websocket::result::WebSocketError;
use tracing::{info, error, debug, warn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use serde_json::json;
use tokio::sync::mpsc;
use crate::servermanager::ServerInfo;
use std::collections::{HashMap, HashSet};
use fake::{Fake, faker::lorem::en::Word, faker::number::en::NumberWithFormat};

#[derive(Debug)]
enum GameServerMessage {
    PlayerPositions(Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64, Option<String>)>),
    PropPosition(serde_json::Value),
    PropCreate(serde_json::Value),
    PropDelete(serde_json::Value),
    // PlayerOutOfZone(serde_json::Value),
}

#[derive(Debug, Clone, Serialize)]
pub struct Zone {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
    pub min_z: f64,
    pub max_z: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerState {
    Starting,
    Online,
    Offline,
    Running,
    Maintenance,
}

#[derive(Clone)]
pub struct Server {
    pub uuid: String,
    pub address: String,
    pub zone: Arc<RwLock<Zone>>,
    pub state: ServerState,
    pub websocket_sender: Arc<Mutex<Option<Writer<TcpStream>>>>,
    pub websocket_receiver: Arc<Mutex<Option<Reader<TcpStream>>>>,
    pub managed_objects: Arc<Mutex<Vec<String>>>,
    pub managed_players: Arc<Mutex<Vec<String>>>,
    /// Tracks player UUIDs currently being transferred (freeze on source / spawn on destination).
    /// Prevents duplicate out_of_zone events from triggering multiple simultaneous transfers.
    pub transferring_players: Arc<Mutex<HashSet<String>>>,
    pub server_name: String,
}

impl Server {
    pub fn new(address: String, zone: Zone) -> Self {

        // Generate a random word and a 5-digit number
        let word: String = Word().fake();
        let number: String = NumberWithFormat("#####").fake();
        let server_name = format!("{}-{}", word, number);
                
        Server {
            uuid: uuid::Uuid::new_v4().to_string(),
            address,
            zone: Arc::new(RwLock::new(zone)),
            state: ServerState::Offline,
            websocket_sender: Arc::new(Mutex::new(None)),
            websocket_receiver: Arc::new(Mutex::new(None)),
            managed_objects: Arc::new(Mutex::new(Vec::new())),
            managed_players: Arc::new(Mutex::new(Vec::new())),
            transferring_players: Arc::new(Mutex::new(HashSet::new())),
            server_name,
        }
    }

    pub async fn register_handlers(
        &mut self,
        context: Arc<dyn ServerContext>,
    ) -> Result<(), PluginError> {

		let Some(gorc_instances) = context.clone().events().get_gorc_instances() else {
			error!("🎮 GORC: ❌ No GORC instances manager available");
			return Ok(()); // Not a fatal error, just log and continue
		};

        let websocket_sender = Arc::clone(&self.websocket_sender);
        let events = context.clone().events();
        let zone = self.zone.read().unwrap().clone();
        let tokio_handle_spawnobj = context.clone().tokio_handle();
        let gorc_instances = gorc_instances.clone();
        events.on_plugin("gameserverplugin", "spawn_object", move |event: serde_json::Value| {
            debug!("🔧 DsGameServerPlugin: Adding prop with event: {:?}", event.clone());

            let websocket_sender = Arc::clone(&websocket_sender);

            let event_clone = event.clone();
            let gorc_instances = gorc_instances.clone();
            tokio_handle_spawnobj.spawn(async move {
                if let Ok(parent_gorc_id) = GorcObjectId::from_str(event_clone["object_uuid"].as_str().unwrap_or_default()) {
                    if let Some(global_position) = gorc_instances.get_object_position(parent_gorc_id).await {
                        debug!("🔧 DsGameServerPlugin: Prop global position: {:?}", global_position);
                        // check if the position is in the Zone of the server
                        if global_position.x < zone.min_x || global_position.x > zone.max_x ||
                        global_position.y < zone.min_y || global_position.y > zone.max_y ||
                        global_position.z < zone.min_z || global_position.z > zone.max_z {
                            debug!("🔧 DsGameServerPlugin: Prop is outside of server zone, skipping spawn.");
                            return;
                        }
                    } else {
                        debug!("🔧 DsGameServerPlugin: Could not get global position of gorc object, skipping spawn.");
                        return;
                    }
                } else {
                    debug!("🔧 DsGameServerPlugin: Invalid gorc_id format: {}, skipping spawn.", event_clone["object_uuid"]);
                    return;
                }

                let _ = spawn_prop::handle_spawn_prop(
                    event_clone,
                    websocket_sender,
                ).await;
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let websocket_sender = Arc::clone(&self.websocket_sender);
        let managed_players = Arc::clone(&self.managed_players);
        let tokio_handle_newplayer = context.clone().tokio_handle();
        let zone_for_new_player = Arc::clone(&self.zone);
        events.on_plugin("plugingameserver", "new_player", move |event: serde_json::Value| {

            // Check if player position is within this server's zone
            if let Some(object_data) = event.get("object_data") {
                if let Some(position) = object_data.get("_global_position") {
                    let x = position.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let y = position.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let z = position.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);

                    let zone = zone_for_new_player.read().unwrap();
                    if x < zone.min_x || x > zone.max_x ||
                       y < zone.min_y || y > zone.max_y ||
                       z < zone.min_z || z > zone.max_z {
                        debug!("🔧 DsGameServerPlugin: Player position ({}, {}, {}) is outside server zone, skipping", x, y, z);
                        return Ok(());
                    }
                }
            }


            info!("🔧 DsGameServerPlugin: New player event: {:?}", event);
            managed_players.lock().unwrap().push(event["object_uuid"].as_str().unwrap_or_default().to_string());

            let websocket_sender = Arc::clone(&websocket_sender);
            tokio_handle_newplayer.spawn(async move {
                let _ = spawn_player::handle_spawn_player(
                    event.clone(),
                    websocket_sender,
                ).await;
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // Handler when the client moves, we transmit to the server
        let websocket_sender = Arc::clone(&self.websocket_sender);
        let managed_players_movement = Arc::clone(&self.managed_players);
        let tokio_handle_updatevelocity = context.clone().tokio_handle();
        events.on_client("movement", "update_velocity", move |event: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {
            debug!("📝 LoggerPlugin: 🦘 Client movement from player {}", event.player_id);

            // Check if this player is managed by this server
            if !managed_players_movement.lock().unwrap().contains(&event.player_id.to_string()) {
                debug!("🔧 DsGameServerPlugin: Player {} is not managed by this server, skipping movement", event.player_id);
                return Ok(());
            }

            let websocket_sender = Arc::clone(&websocket_sender);
            tokio_handle_updatevelocity.spawn(async move {
                let _ = player_movement::handle_player_movement(
                    event.clone(),
                    websocket_sender,
                ).await;
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // Handler when the client do action (jump, press...), we transmit to the server
        let websocket_sender = Arc::clone(&self.websocket_sender);
        let managed_players_action = Arc::clone(&self.managed_players);
        let tokio_handle_clientaction = context.clone().tokio_handle();
        events.on_client("player", "client_action", move |event: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {

            // Check if this player is managed by this server
            if !managed_players_action.lock().unwrap().contains(&event.player_id.to_string()) {
                debug!("🔧 DsGameServerPlugin: Player {} is not managed by this server, skipping action", event.player_id);
                return Ok(());
            }

            debug!("📝 LoggerPlugin: 🦘 Client action from player {}", event.player_id);

            let websocket_sender = Arc::clone(&websocket_sender);
            tokio_handle_clientaction.spawn(async move {
                let _ = player_action::handle_player_action(
                    event.clone(),
                    websocket_sender,
                ).await;
            });

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        // Handler for receving items when start the server (new zone)
        let server_uuid = self.uuid.clone();
        let managed_objects = Arc::clone(&self.managed_objects);
        let managed_players = Arc::clone(&self.managed_players);
        let websocket_sender = Arc::clone(&self.websocket_sender);
        let tokio_handle_initialobjs = context.clone().tokio_handle();
        let zone_for_initial = Arc::clone(&self.zone);
        events.on_plugin("gameserver", "initial_objects_on_zone", move |event: serde_json::Value| {
            info!("🔧 DsGameServerPlugin: Received initial_objects_on_zone event uuid: {:?} for my server uuid: {:?}", event["server_uuid"], server_uuid.clone());
            if event["server_uuid"] == server_uuid {
                info!("🔧 DsGameServerPlugin: This initial_objects_on_zone event is for me, processing...");

                debug!("🔧 DsGameServerPlugin: Initial objects on zone event: {:?}", event);
                info!("🔧 DsGameServerPlugin: let's go!");
                // TODO send to server the list of items on the zone
                let websocket_sender = Arc::clone(&websocket_sender);
                let managed_objects = Arc::clone(&managed_objects);
                let managed_players = Arc::clone(&managed_players);
                let zone_arc = Arc::clone(&zone_for_initial);
                tokio_handle_initialobjs.spawn(async move {
                    let zone = zone_arc.read().unwrap().clone();
                    let _ = initial_objects::handle_initial_object(
                        event.clone(),
                        websocket_sender,
                        managed_objects,
                        managed_players,
                        &zone,
                    ).await;
                });

            } else if event["split_server_uuid"] == server_uuid {
                info!("🔧 DsGameServerPlugin: This initial_objects_on_zone event is for me as split server, processing...");
                
                debug!("🔧 DsGameServerPlugin: objects on zone event to freeze: {:?}", event);
                info!("🔧 DsGameServerPlugin: let's go!");
                // TODO send to server the list of items on the zone
                let websocket_sender = Arc::clone(&websocket_sender);
                let managed_objects = Arc::clone(&managed_objects);
                let managed_players = Arc::clone(&managed_players);
                let zone_arc = Arc::clone(&zone_for_initial);
                tokio_handle_initialobjs.spawn(async move {
                    let zone = zone_arc.read().unwrap().clone();
                    let _ = initial_objects::handle_freeze_object(
                        event.clone(),
                        websocket_sender,
                        managed_objects,
                        managed_players,
                        &zone,
                    ).await;
                });

            }
            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        let server_uuid = self.uuid.clone();
        let zone_out_of_zone = Arc::clone(&self.zone);
        let managed_objects = Arc::clone(&self.managed_objects);
        let managed_players = Arc::clone(&self.managed_players);
        let websocket_sender = Arc::clone(&self.websocket_sender);
        let transferring_players = Arc::clone(&self.transferring_players);
        let tokio_handle_out_of_zone = context.clone().tokio_handle();
        events.on_plugin("gameserverplugin", "player_out_of_zone", move |event: serde_json::Value| {
            let object_uuid = event["item"]["object_uuid"].as_str().unwrap_or_default().to_string();
            info!("🔧 DsGameServerPlugin: Received player_out_of_zone event uuid: {:?} for my server uuid: {:?}", event["server_uuid"], server_uuid.clone());

            // Deduplication guard: skip if this player is already being transferred on this server
            {
                let transferring = transferring_players.lock().unwrap();
                if transferring.contains(&object_uuid) {
                    debug!("🔧 DsGameServerPlugin: Player {} is already being transferred, skipping duplicate out_of_zone event", object_uuid);
                    return Ok(());
                }
            }

            if event["server_uuid"] == server_uuid {
                debug!("🔧 DsGameServerPlugin: Player out of zone event: {:?}", event);

                // Mark player as transferring and remove from managed_players SYNCHRONOUSLY
                // to stop forwarding movements immediately
                transferring_players.lock().unwrap().insert(object_uuid.clone());
                {
                    let mut players = managed_players.lock().unwrap();
                    if let Some(pos) = players.iter().position(|x| x == &object_uuid) {
                        players.remove(pos);
                        info!("🔧 DsGameServerPlugin: Removed player {} from managed_players on source server before freeze", object_uuid);
                    }
                }

                // Freeze the player on the server (async)
                let websocket_sender = Arc::clone(&websocket_sender);
                let managed_objects = Arc::clone(&managed_objects);
                let managed_players = Arc::clone(&managed_players);
                let transferring_players_clone = Arc::clone(&transferring_players);
                let zone_arc = Arc::clone(&zone_out_of_zone);
                let object_uuid_clone = object_uuid.clone();
                let item_clone = event["item"].clone();
                tokio_handle_out_of_zone.spawn(async move {
                    let zone = zone_arc.read().unwrap().clone();
                    let mut items_map = serde_json::Map::new();
                    items_map.insert(object_uuid_clone.clone(), item_clone);
                    let _ = initial_objects::handle_freeze_object(
                        serde_json::json!({
                            "items": items_map
                        }),
                        websocket_sender,
                        managed_objects,
                        managed_players,
                        &zone,
                    ).await;
                    // Clear the transfer guard after freeze completes
                    transferring_players_clone.lock().unwrap().remove(&object_uuid_clone);
                    info!("🔧 DsGameServerPlugin: Freeze complete, cleared transfer guard for player {}", object_uuid_clone);
                });
            } else {
                debug!("🔧 DsGameServerPlugin: This player_out_of_zone event is not for me, checking position...");
                // check if item in the zone managed by this server
                info!("🔧 DsGameServerPlugin: Checking player position for out_of_zone event: {:?}", event);
                if let Some(position) = event.get("global_position")
                {
                    let x = position.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let y = position.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let z = position.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);

                    let zone = zone_out_of_zone.read().unwrap();
                    if x < zone.min_x || x > zone.max_x ||
                    y < zone.min_y || y > zone.max_y ||
                    z < zone.min_z || z > zone.max_z {
                        info!("Item position ({}, {}, {}) is outside server zone.", x, y, z);
                        info!("The zone is min_x: {}, max_x: {}, min_y: {}, max_y: {}, min_z: {}, max_z: {}", zone.min_x, zone.max_x, zone.min_y, zone.max_y,  zone.min_z, zone.max_z);
                        return Ok(()); // outside of zone
                    } else {
                        info!("Item position ({}, {}, {}) is inside server zone.", x, y, z);

                        // Clamp the spawn position to be safely inside the destination zone.
                        // This prevents Godot physics / orbital motion from pushing a freshly-spawned
                        // player back across the zone boundary within the first few frames.
                        const ZONE_SAFE_MARGIN: f64 = 0.005;
                        let clamped_x = x.max(zone.min_x + ZONE_SAFE_MARGIN).min(zone.max_x - ZONE_SAFE_MARGIN);
                        let clamped_y = y.max(zone.min_y + ZONE_SAFE_MARGIN).min(zone.max_y - ZONE_SAFE_MARGIN);
                        let clamped_z = z.max(zone.min_z + ZONE_SAFE_MARGIN).min(zone.max_z - ZONE_SAFE_MARGIN);
                        info!("[TRANSFER DEBUG] Player {} original spawn: ({:.3}, {:.3}, {:.3}), clamped: ({:.3}, {:.3}, {:.3}), zone: x[{:.3},{:.3}] y[{:.3},{:.3}] z[{:.3},{:.3}]", 
                            object_uuid, x, y, z, clamped_x, clamped_y, clamped_z, zone.min_x, zone.max_x, zone.min_y, zone.max_y, zone.min_z, zone.max_z);
                        drop(zone); // release the read lock

                        let delta_x = clamped_x - x;
                        let delta_y = clamped_y - y;
                        let delta_z = clamped_z - z;

                        // Build the spawn item data, adjusting positions if clamping was needed
                        let spawn_item = if delta_x.abs() > 0.001 || delta_y.abs() > 0.001 || delta_z.abs() > 0.001 {
                            let mut item = event["item"].clone();
                            if let Some(obj_data) = item.get_mut("object_data") {
                                if let Some(gpos) = obj_data.get_mut("_global_position") {
                                    gpos["x"] = json!(clamped_x);
                                    gpos["y"] = json!(clamped_y);
                                    gpos["z"] = json!(clamped_z);
                                }
                                // Adjust the local position by the same delta so Godot places
                                // the entity at the clamped global position
                                if let Some(pos) = obj_data.get_mut("position") {
                                    let lx = pos.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let ly = pos.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    let lz = pos.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                    pos["x"] = json!(lx + delta_x);
                                    pos["y"] = json!(ly + delta_y);
                                    pos["z"] = json!(lz + delta_z);
                                }
                            }
                            info!("[TRANSFER DEBUG] Clamped spawn position for player {} (delta: {:.3}, {:.3}, {:.3})", object_uuid, delta_x, delta_y, delta_z);
                            debug!("[TRANSFER DEBUG] About to call spawn_player for player {} with spawn_item: {:?}", object_uuid, item);
                            item
                        } else {
                            let item = event["item"].clone();
                            debug!("[TRANSFER DEBUG] About to call spawn_player for player {} with spawn_item: {:?}", object_uuid, item);
                            item
                        };

                        // Mark player as transferring to prevent duplicate spawns
                        transferring_players.lock().unwrap().insert(object_uuid.clone());

                        // DON'T add to managed_players yet - wait until Godot confirms spawn
                        // This prevents forwarding movements to Godot before the player entity exists
                        let websocket_sender = Arc::clone(&websocket_sender);
                        let managed_players_clone = Arc::clone(&managed_players);
                        let transferring_players_clone = Arc::clone(&transferring_players);
                        let object_uuid_clone = object_uuid.clone();
                        tokio_handle_out_of_zone.spawn(async move {
                            debug!("[TRANSFER DEBUG] Spawning player {} on Godot server", object_uuid_clone);
                            let result = spawn_player::handle_spawn_player(
                                spawn_item,
                                websocket_sender,
                            ).await;
                            if result.is_ok() {
                                // Only add to managed_players AFTER the spawn message was sent to Godot
                                let mut players = managed_players_clone.lock().unwrap();
                                if !players.contains(&object_uuid_clone) {
                                    players.push(object_uuid_clone.clone());
                                    info!("🔧 DsGameServerPlugin: Added player {} to managed_players after successful spawn", object_uuid_clone);
                                }
                                debug!("[TRANSFER DEBUG] Player {} successfully spawned and added to managed_players", object_uuid_clone);
                            } else {
                                error!("🔧 DsGameServerPlugin: Failed to spawn player {}, not adding to managed_players", object_uuid_clone);
                            }
                            // Clear the transfer guard after spawn completes
                            transferring_players_clone.lock().unwrap().remove(&object_uuid_clone);
                            info!("🔧 DsGameServerPlugin: Spawn complete, cleared transfer guard for player {}", object_uuid_clone);
                        });
                    }
                } else {
                    error!("No position found in player properties");
                }
            }

            Ok(())
        }).await
        .map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        Ok(())
    }

    pub fn connect(&mut self) -> Result<(), WebSocketError> {
        let client = websocket::client::ClientBuilder::new(&self.address).unwrap().connect_insecure()?;
        let (receiver, sender) = client.split().unwrap();
        self.websocket_sender = Arc::new(Mutex::new(Some(sender)));
        self.websocket_receiver = Arc::new(Mutex::new(Some(receiver)));
        self.state = ServerState::Online;
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.state = ServerState::Offline;
        let mut sender_lock = self.websocket_sender.lock().unwrap();
        let mut receiver_lock = self.websocket_receiver.lock().unwrap();
        *sender_lock = None;
        *receiver_lock = None;
    }


    /// Start the server with zone and items
    pub fn start(&mut self, zone: Zone) {
        self.state = ServerState::Starting;
        *self.zone.write().unwrap() = zone;

        // send the zone to the server
        let zone_data = self.zone.read().unwrap().clone(); 

        let message = serde_json::json!({
            "namespace": "server",
            "event": "zone",
            "server_uuid": self.uuid,
            "server_name": self.server_name,
            "data": zone_data,
        });

        let websocket_sender = Arc::clone(&self.websocket_sender);

        let mut ws_guard = match websocket_sender.lock() {
            Ok(g) => g,
            Err(e) => {
                debug!("[send_zone] websocket lock error: {}", e);
                return;
            }
        };
        if ws_guard.is_none() {
            error!("[send_zone] No websocket writer available");
            return;
        }
        if let Some(w) = ws_guard.as_mut() {
            debug!("[send_zone] Sending message to websocket");
            if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                debug!("[send_zone] ERROR sending message to game server: {}", e);
                return;
            } else {
                debug!("[send_zone] Message sent successfully");
            }
        }

    }

    pub fn update_zone(&mut self, zone: Zone) {
        *self.zone.write().unwrap() = zone;

        // send the zone to the server
        let zone_data = self.zone.read().unwrap().clone(); 

        let message = serde_json::json!({
            "namespace": "server",
            "event": "zone",
            "server_uuid": self.uuid,
            "server_name": self.server_name,
            "data": zone_data,
        });

        let websocket_sender = Arc::clone(&self.websocket_sender);

        let mut ws_guard = match websocket_sender.lock() {
            Ok(g) => g,
            Err(e) => {
                debug!("[send_zone] websocket lock error: {}", e);
                return;
            }
        };
        if ws_guard.is_none() {
            error!("[send_zone] No websocket writer available");
            return;
        }
        if let Some(w) = ws_guard.as_mut() {
            debug!("[send_zone] Sending message to websocket");
            if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                debug!("[send_zone] ERROR sending message to game server: {}", e);
                return;
            } else {
                debug!("[send_zone] Message sent successfully");
            }
        }

    }


    async fn received_queue_processing(&mut self, mut rx: UnboundedReceiver<GameServerMessage>, events_processor: Arc<EventSystem>, srvinfo_tx: mpsc::Sender<ServerInfo>) {
        info!("🔧 DsGameServerPlugin: Async processor task started on main runtime");
        let mut message_count = 0u64;
        let mut players: HashMap<String, Vec3> = HashMap::new();

        while let Some(msg) = rx.recv().await {
            message_count += 1;
            if message_count % 100 == 0 {
                debug!("� Async processor: processed {} messages", message_count);
            }
            debug!("🔧 DsGameServerPlugin: Processing message: {:?}", msg);
            match msg {
                GameServerMessage::PlayerPositions(position_updates) => {
                    for (gorc_id, player_id, x, y, z, rotx, roty, rotz, out_of_zone) in position_updates {
                        if let Err(e) = events_processor.emit_plugin("genericprops", "playermove", &serde_json::json!({
                            "object_type": "player",
                            "object_uuid": player_id.clone().to_string(),
                            "object_data": &serde_json::json!({
                                "player_id": player_id.clone(),
                                "position": Vec3::new(x, y, z),
                                "rotation": Vec3::new(rotx, roty, rotz),
                                "out_of_zone": out_of_zone,
                            }),
                        })).await {
                            error!("Failed to emit plugin event to propsplugin: {}", e);
                        }
                    }
                }

                // GameServerMessage::PlayerPositions(position_updates) => {
                //     <for (gorc_id, player_id, x, y, z, rotx, roty, rotz) in position_updates {>
                //         if let Err(e) = events_processor.emit_gorc_instance(
                //             gorc_id,
                //             0,
                //             "move",
                //             &serde_json::json!({
                //                 "player_id": player_id.clone(),
                //                 "new_position": Vec3::new(x, y, z),
                //                 "new_rotation": Vec3::new(rotx, roty, rotz),
                //                 "velocity": { "x": 0.0, "y": 0.0, "z": 0.0 },
                //                 "movement_state": 1,
                //                 "client_timestamp": chrono::Utc::now().to_rfc3339(),
                //             }),
                //             Dest::Both
                //         ).await {
                //             error!("Failed to update player position via EventSystem: {}", e);
                //         }
                //         players.entry(player_id.to_string()).or_insert(Vec3::new(x, y, z));
                //     }
                // }
                GameServerMessage::PropPosition(prop_data) => {
                    if prop_data["type"] == "serverinfo" {
                        info!("🔧 DsGameServerPlugin: Updating server info: {:?}", prop_data);
                        let data = ServerInfo {
                            uuid: self.uuid.clone(),
                            fps: prop_data["fps"].as_u64().unwrap_or_default() as u8,
                            objects_number: prop_data["objects_number"].as_u64().map(|v| v as u32).unwrap_or_default(),
                            players_number: prop_data["players_number"].as_u64().map(|v| v as u16).unwrap_or_default(),
                            scenes_number: prop_data["scenes_number"].as_u64().map(|v| v as u32).unwrap_or_default(),
                            players_positions: players.clone(),
                            server_name: self.server_name.clone(),
                        };
                        // Use try_send to avoid blocking if channel is full - this is not critical data
                        if let Err(e) = srvinfo_tx.try_send(data) {
                            debug!("ServerInfo channel full or closed, dropping update: {}", e);
                        }
                    } else {
                        if let Err(e) = events_processor.emit_plugin("genericprops", "update_object", &serde_json::json!({
                            "object_type": prop_data["type"],
                            "object_uuid": prop_data["uuid"],
                            "object_data": prop_data,
                        })).await {
                            error!("Failed to emit plugin event to propsplugin: {}", e);
                        }
                    }
                }
                GameServerMessage::PropCreate(prop_data) => {
                    if let Err(e) = events_processor.emit_plugin("genericprops", "create_object_from_gameserver", &serde_json::json!({
                        "object_type": prop_data["type"],
                        "object_uuid": prop_data["uuid"],
                        "object_data": prop_data,
                    })).await {
                        error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                // TODO why we have this here???? it's old code
                //     let message = json!({
                //         "namespace": "server",
                //         "event": "add_prop",
                //         "data": {
                //             "object_type": prop_data["type"],
                //             "object_uuid": prop_data["uuid"],
                //             "object_data": prop_data,
                //         }
                //     });
                //     if let Ok(mut ws_guard) = websocket_processor.lock() {
                //         if let Some(w) = ws_guard.as_mut() {
                //             if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                //                 error!("Failed to send websocket message: {}", e);
                //             }
                //         }
                //     }
                }
                GameServerMessage::PropDelete(prop_data) => {
                    if let Err(e) = events_processor.emit_plugin("genericprops", "delete_object", &serde_json::json!({
                        "object_type": prop_data["type"],
                        "object_uuid": prop_data["uuid"],
                        "object_data": prop_data,
                    })).await {
                        error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                }
                GameServerMessage::PropCreate(_) => error!("Received unexpected PropCreate message"),
                // GameServerMessage::PlayerOutOfZone(player_data) => {
                //     if let Err(e) = events_processor.emit_plugin("genericprops", "player_out_of_zone", &serde_json::json!({
                //         "object_type": "player",
                //         "object_uuid": player_data,
                //         "object_data": {
                //             "player_uuid": player_data,
                //             "server_uuid": self.uuid,
                //         },
                //     })).await {
                //         error!("Failed to emit plugin event to propsplugin: {}", e);
                //     }
                // }
            }
        }
        warn!("🔧 DsGameServerPlugin: Async processor channel closed");
    }

    fn receive_ws_to_queue(&mut self, tx: UnboundedSender<GameServerMessage>) {
        let mut receiver_lock = self.websocket_receiver.lock().unwrap();
        if let Some(ref mut receiver) = *receiver_lock {
            for msg in receiver.incoming_messages() {
                match msg {
                    Ok(OwnedMessage::Text(s)) => {
                        debug!("[message][from][gamesever]: {}", s);
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                            if value["namespace"] == "players" && value["event"] == "position" {
                                let mut position_updates: Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64, Option<String>)> = Vec::new();
                                
                                for player_data in value["data"].as_array().unwrap() {
                                    if let Some(uuid_str) = player_data["player_id"].as_str() {
                                        if let (Ok(player_id), Ok(gorc_id)) = (
                                            PlayerId::from_str(uuid_str),
                                            GorcObjectId::from_str(uuid_str)
                                        ) {
                                            if let (Some(x), Some(y), Some(z), Some(rx), Some(ry), Some(rz)) = (
                                                player_data["pos"]["x"].as_f64(),
                                                player_data["pos"]["y"].as_f64(),
                                                player_data["pos"]["z"].as_f64(),
                                                player_data["rot"]["x"].as_f64(),
                                                player_data["rot"]["y"].as_f64(),
                                                player_data["rot"]["z"].as_f64()
                                            ) {
                                                let out_of_zone = player_data.get("out_of_zone")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());
                                                position_updates.push((gorc_id, player_id, x, y, z, rx, ry, rz, out_of_zone));
                                            } else {
                                                error!("Invalid position coordinates in player data: {:?}", player_data["pos"]);
                                            }
                                        } else {
                                            error!("Invalid player_id format: {:?}", player_data["player_id"]);
                                        }
                                    } else {
                                        error!("Missing player_id in player data: {:?}", player_data);
                                    }
                                }
                                
                                // Send to async processor - non-blocking!
                                if !position_updates.is_empty() {
                                    if let Err(e) = tx.send(GameServerMessage::PlayerPositions(position_updates)) {
                                        error!("Failed to send position updates to processor: {}", e);
                                    }
                                }
                            } else if value["namespace"] == "props" && value["event"] == "position" {
                                // Send each prop update to async processor - non-blocking
                                for prop_data in value["data"].as_array().unwrap() {
                                    if let Err(e) = tx.send(GameServerMessage::PropPosition(prop_data.clone())) {
                                        error!("Failed to send prop position to processor: {}", e);
                                    }
                                }
                            } else if value["namespace"] == "props" && value["event"] == "create_object" {
                                debug!("Props creation object received: {:?}", value);
                                for prop_data in value["data"].as_array().unwrap() {
                                    if let Err(e) = tx.send(GameServerMessage::PropCreate(prop_data.clone())) {
                                        error!("Failed to send prop create to processor: {}", e);
                                    }
                                }
                            } else if value["namespace"] == "props" && value["event"] == "delete_object" {
                                debug!("Props delete object received: {:?}", value);
                                for prop_data in value["data"].as_array().unwrap() {
                                    if let Err(e) = tx.send(GameServerMessage::PropDelete(prop_data.clone())) {
                                        error!("Failed to send prop delete to processor: {}", e);
                                    }
                                }
                            } else if value["namespace"] == "players" && value["event"] == "update" {
                                // Get gorc_id from value["uuid"]
                                let _ = if let Some(uuid_str) = value["uuid"].as_str() {
                                    match GorcObjectId::from_str(uuid_str) {
                                        Ok(id) => id,
                                        Err(e) => {
                                            error!("Invalid gorc_id format in update event: {:?}", e);
                                            continue;
                                        }
                                    }
                                } else {
                                    error!("Missing uuid in update event: {:?}", value);
                                    continue;
                                };
                                // TODO review this code for updates
                                // if let Err(e) = events_processor.emit_plugin("pluginplayer", "update", &serde_json::json!(value["data"])).await {
                                //     error!("Failed to update player position via EventSystem: {}", e);
                                // }
                            // } else if value["namespace"] == "players" && value["event"] == "out_of_zone" {
                            //     info!("Player out of zone received: {:?}", value);
                            //     if let Err(e) = tx.send(GameServerMessage::PlayerOutOfZone(value["data"].clone())) {
                            //         error!("Failed to send prop delete to processor: {}", e);
                            //     }
                            }
                        } else {
                            debug!("Failed to parse incoming JSON: {}", s);
                        }
                    }
                    Ok(OwnedMessage::Binary(b)) => {
                        if let Ok(s) = String::from_utf8(b) {
                            debug!("[message][from][gamesever] (binary->text): {}", s);
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&s) {
                                if value["namespace"] == "players" && value["event"] == "position" {
                                    let mut position_updates: Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64, Option<String>)> = Vec::new();
                                    
                                    for player_data in value["data"].as_array().unwrap() {
                                        if let Some(uuid_str) = player_data["player_id"].as_str() {
                                            if let (Ok(player_id), Ok(gorc_id)) = (
                                                PlayerId::from_str(uuid_str),
                                                GorcObjectId::from_str(uuid_str)
                                            ) {
                                                if let (Some(x), Some(y), Some(z), Some(rx), Some(ry), Some(rz)) = (
                                                    player_data["pos"]["x"].as_f64(),
                                                    player_data["pos"]["y"].as_f64(),
                                                    player_data["pos"]["z"].as_f64(),
                                                    player_data["rot"]["x"].as_f64(),
                                                    player_data["rot"]["y"].as_f64(),
                                                    player_data["rot"]["z"].as_f64()
                                                ) {
                                                    let out_of_zone = player_data.get("out_of_zone")
                                                        .and_then(|v| v.as_str())
                                                        .map(|s| s.to_string());
                                                    position_updates.push((gorc_id, player_id, x, y, z, rx, ry, rz, out_of_zone));
                                                }
                                            }
                                        }
                                    }
                                    
                                    if !position_updates.is_empty() {
                                        if let Err(e) = tx.send(GameServerMessage::PlayerPositions(position_updates)) {
                                            error!("Failed to send position updates to processor: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(_) => { /* ignore ping/pong/close frames */ }
                    Err(WebSocketError::NoDataAvailable) => {
                        info!("Server disconnected!");
                        self.state = ServerState::Offline;
                        return;
                    }
                    Err(e) => {
                        error!("WebSocket read error: {:?}", e);
                        self.state = ServerState::Offline;
                    }
                }
            }
        }
    }

    pub fn receive_messages(&mut self, context: Arc<dyn ServerContext>, srvinfo_tx: mpsc::Sender<ServerInfo>) {
        debug!("🔧 DsGameServerPlugin: Setting up WebSocket receiver and processing tasks");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<GameServerMessage>();

        let events = context.clone().events();
        let tokio_handle_queue = context.clone().tokio_handle();
        let tokio_handle_ws = context.clone().tokio_handle();
        let mut server = self.clone();
        tokio_handle_queue.spawn(async move {
            server.received_queue_processing(rx, events, srvinfo_tx).await;
        });

        let mut server = self.clone();
        // Use spawn_blocking for the blocking WebSocket receiver
        tokio_handle_ws.spawn_blocking(move || {
            info!("🔧 DsGameServerPlugin: WebSocket receiver task started on main runtime");
            server.receive_ws_to_queue(tx);
        });
    }
}

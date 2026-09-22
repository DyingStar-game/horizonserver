//! One Godot game server as seen from Horizon: its websocket, the **zones** it owns
//! (space / planets, optionally bounded) and the objects/players it simulates.
//!
//! Zone membership never looks at absolute coordinates any more: every payload
//! ds_genericprops sends carries `object_data["_world"]` (space or planet + local
//! position) and the server owns a list of `Zone`s, see `ds_common::zone`.

use crate::handlers::{
    initial_objects, player_action, player_movement, send_ws, spawn_player, spawn_prop, update_prop, WsWriter,
};
use crate::servermanager::{ManagerMessage, ServerInfo};

use ds_common::events::GenericPropsRequest;
use ds_common::world::{ObjectWorld, Point3};
use ds_common::zone::{is_world_object, zone_containing, zones_contain, zones_label, Zone};
use fake::{faker::lorem::en::Word, faker::number::en::NumberWithFormat, Fake};
use horizon_event_system::{
    ClientConnectionRef, ClientEventWrapper, EventSystem, GorcObjectId, PlayerDisconnectedEvent, PlayerId, PluginError, ServerContext, Vec3,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info, warn};
use websocket::message::OwnedMessage;
use websocket::receiver::Reader;
use websocket::result::WebSocketError;

/// Margin (m) a transferred object is pulled inside its destination bounds, so the
/// first physics frames on the new server cannot push it straight back across.
const BOUNDS_SAFE_MARGIN: f64 = 0.005;
/// A player closer than this (m) to one of our bounds gets the ground prewarmed on
/// the neighbouring server, so the handover does not wait for a chunk build.
pub const PREWARM_DISTANCE: f64 = 300.0;
/// Minimum interval between two prewarms for the same player.
const PREWARM_REPEAT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug)]
enum GameServerMessage {
    PlayerPositions(Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64, Option<String>, Option<String>)>),
    PropPosition(serde_json::Value),
    PropCreate(serde_json::Value),
    PropDelete(serde_json::Value),
    // Unified property replication for any object (player or prop): the game
    // server now sends a single "props/update_object" event for both.
    ObjectUpdate(serde_json::Value),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ServerState {
    /// Zones sent, waiting to become Running.
    Starting,
    /// Connected, idle in the pool (no zones).
    Online,
    /// Not connected.
    Offline,
    /// Connected and simulating its zones.
    Running,
    /// Retired from the pool (its address left DNS); never reconnected.
    Maintenance,
}

#[derive(Clone)]
pub struct Server {
    pub uuid: String,
    pub address: String,
    pub server_name: String,
    pub zones: Arc<RwLock<Vec<Zone>>>,
    pub state: Arc<Mutex<ServerState>>,
    pub websocket_sender: WsWriter,
    pub websocket_receiver: Arc<Mutex<Option<Reader<TcpStream>>>>,
    pub managed_objects: initial_objects::ManagedObjects,
    pub managed_players: initial_objects::ManagedPlayers,
    /// Tracks player UUIDs currently being transferred (freeze on source / spawn on destination).
    /// Prevents duplicate out_of_zone events from triggering multiple simultaneous transfers.
    pub transferring_players: Arc<Mutex<HashSet<String>>>,
    /// Handlers are registered once per server, at its first connection.
    pub handlers_registered: Arc<AtomicBool>,
    /// Direct parent of each player we manage (planet, vehicle, building uuid or ""),
    /// seeded at spawn and updated from the `parent_id` a position packet carries.
    /// Lets us tell when a planet-local position is approaching a zone border.
    pub player_parents: Arc<Mutex<HashMap<String, String>>>,
    /// Last prewarm emitted per player, to send one every `PREWARM_REPEAT` at most.
    pub prewarm_sent: Arc<Mutex<HashMap<String, Instant>>>,
    /// Latest serverinfo sample and when it arrived, for whoever waits on this
    /// server being ready (the manager during a hand-over) without going through
    /// the manager's own channel.
    pub last_info: Arc<Mutex<Option<(ServerInfo, Instant)>>>,
    /// Bumped at every connect(); a reader task only reports the loss of the socket
    /// it was created for (a hung server can be reconnected while its old reader is
    /// still blocked on the dead socket).
    pub connection_generation: Arc<AtomicU64>,
}

impl Server {
    pub fn new(address: String) -> Self {
        // Generate a random word and a 5-digit number
        let word: String = Word().fake();
        let number: String = NumberWithFormat("#####").fake();
        let server_name = format!("{}-{}", word, number);

        Server {
            uuid: uuid::Uuid::new_v4().to_string(),
            address,
            server_name,
            zones: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(Mutex::new(ServerState::Offline)),
            websocket_sender: Arc::new(Mutex::new(None)),
            websocket_receiver: Arc::new(Mutex::new(None)),
            managed_objects: Arc::new(Mutex::new(HashSet::new())),
            managed_players: Arc::new(Mutex::new(Vec::new())),
            transferring_players: Arc::new(Mutex::new(HashSet::new())),
            handlers_registered: Arc::new(AtomicBool::new(false)),
            connection_generation: Arc::new(AtomicU64::new(0)),
            player_parents: Arc::new(Mutex::new(HashMap::new())),
            prewarm_sent: Arc::new(Mutex::new(HashMap::new())),
            last_info: Arc::new(Mutex::new(None)),
        }
    }

    pub fn state(&self) -> ServerState {
        *self.state.lock().unwrap()
    }

    pub fn set_state(&self, state: ServerState) {
        *self.state.lock().unwrap() = state;
    }

    pub fn is_running(&self) -> bool {
        self.state() == ServerState::Running
    }

    /// Connected in any way (idle in the pool, starting or running). World objects
    /// (planets, stars) are sent to every connected server so an idle one has its
    /// planets loaded before it is handed zones — creating 20 planets under a burst
    /// of players is what stalls (and has deadlocked) a cold Godot server.
    pub fn is_connected(&self) -> bool {
        !matches!(self.state(), ServerState::Offline | ServerState::Maintenance)
    }

    pub fn zones(&self) -> Vec<Zone> {
        self.zones.read().unwrap().clone()
    }

    pub fn players_count(&self) -> usize {
        self.managed_players.lock().unwrap().len()
    }

    /// Connects the websocket. Writes INTO the shared Arcs so the handler closures
    /// registered earlier keep talking to the live socket after a reconnection.
    pub fn connect(&mut self) -> Result<(), WebSocketError> {
        let client = websocket::client::ClientBuilder::new(&self.address).unwrap().connect_insecure()?;
        let (receiver, sender) = client.split().unwrap();
        *self.websocket_sender.lock().unwrap() = Some(sender);
        *self.websocket_receiver.lock().unwrap() = Some(receiver);
        self.connection_generation.fetch_add(1, Ordering::SeqCst);
        self.set_state(ServerState::Online);
        Ok(())
    }

    fn send_zones(&self, tag: &str) -> bool {
        let zones = self.zones();
        let message = json!({
            "namespace": "server",
            "event": "zone",
            "server_uuid": self.uuid,
            "server_name": self.server_name,
            "data": { "zones": zones },
        });
        match send_ws(&self.websocket_sender, tag, &message) {
            Ok(()) => {
                info!("[mesh] {} {} ({}) zones=[{}]", tag, self.server_name, self.uuid, zones_label(&zones));
                true
            }
            Err(e) => {
                error!("[mesh] {} {} ({}) failed to send zones: {}", tag, self.server_name, self.uuid, e);
                false
            }
        }
    }

    /// Gives the server its zones and puts it to work.
    pub fn start(&self, zones: Vec<Zone>, context: Arc<dyn ServerContext>) -> bool {
        self.set_state(ServerState::Starting);
        *self.zones.write().unwrap() = zones;
        if !self.send_zones("start") {
            self.set_state(ServerState::Offline);
            return false;
        }
        self.set_state(ServerState::Running);

        let server_uuid = self.uuid.clone();
        let events = context.events();
        crate::plugin_rt().spawn(async move {
            if let Err(e) = events.emit_plugin("ds_game_server", "server_registered", &json!({
                "server_uuid": server_uuid,
            })).await {
                error!("start: failed to emit server_registered: {}", e);
            }
        });
        true
    }

    pub fn update_zones(&self, zones: Vec<Zone>) -> bool {
        *self.zones.write().unwrap() = zones;
        self.send_zones("update_zones")
    }

    /// Takes every zone away and returns the server to the idle pool.
    pub fn release(&self) {
        *self.zones.write().unwrap() = Vec::new();
        let sent = self.send_zones("release");
        self.managed_objects.lock().unwrap().clear();
        self.managed_players.lock().unwrap().clear();
        self.transferring_players.lock().unwrap().clear();
        self.set_state(if sent { ServerState::Online } else { ServerState::Offline });
    }

    /// Takes the server out of the pool for good (its address left DNS while it
    /// was offline): the reconnect loop stops at its next attempt.
    pub fn retire(&self) {
        self.set_state(ServerState::Maintenance);
        *self.websocket_sender.lock().unwrap() = None;
    }

    /// Marks the socket dead and tells the manager. Called by the reader task.
    fn mark_offline(&self, manager_tx: &mpsc::Sender<ManagerMessage>) {
        self.set_state(ServerState::Offline);
        *self.websocket_sender.lock().unwrap() = None;
        if let Err(e) = manager_tx.try_send(ManagerMessage::ServerOffline(self.uuid.clone())) {
            error!("[mesh] could not notify manager that {} went offline: {}", self.uuid, e);
        }
    }

    /// Asks the Godot server to load the ground under `positions` (planet-local) of
    /// `planet_uuid` for a few seconds: players are about to land there.
    pub fn send_prewarm(&self, planet_uuid: &str, positions: &[Point3]) {
        if positions.is_empty() {
            return;
        }
        let message = json!({
            "namespace": "server",
            "event": "prewarm",
            "data": { "planet_uuid": planet_uuid, "positions": positions, "ttl_ms": 15000 },
        });
        if send_ws(&self.websocket_sender, "prewarm", &message).is_ok() {
            info!("[mesh] prewarm {} on {}: {} position(s)", planet_uuid, self.server_name, positions.len());
        }
    }

    /// Remembers the direct parent of a managed player (from its spawn payload).
    fn seed_player_parent(&self, uuid: &str, object_data: &serde_json::Value) {
        let parent = object_data.get("parent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        self.player_parents.lock().unwrap().insert(uuid.to_string(), parent);
    }

    /// For a player standing directly on a planet we own with bounds, returns the
    /// planet and its planet-local position when it is within `PREWARM_DISTANCE`
    /// of one of those bounds (the position packet is planet-local in that case).
    fn near_own_border(&self, player: &str, parent_id: Option<&str>, pos: Point3) -> Option<String> {
        let parent = match parent_id {
            Some(p) => {
                self.player_parents.lock().unwrap().insert(player.to_string(), p.to_string());
                p.to_string()
            }
            None => self.player_parents.lock().unwrap().get(player).cloned().unwrap_or_default(),
        };
        if parent.is_empty() {
            return None;
        }
        let zones = self.zones.read().unwrap();
        let near = zones.iter().any(|z| {
            z.planet_uuid() == Some(parent.as_str())
                && z.bounds.as_ref().map_or(false, |b| b.contains(&pos) && !b.contains_inner(&pos, PREWARM_DISTANCE))
        });
        if !near {
            return None;
        }
        let mut sent = self.prewarm_sent.lock().unwrap();
        let now = Instant::now();
        if sent.get(player).map_or(false, |t| now.duration_since(*t) < PREWARM_REPEAT) {
            return None;
        }
        sent.insert(player.to_string(), now);
        Some(parent)
    }

    fn is_managed_player(&self, uuid: &str) -> bool {
        self.managed_players.lock().unwrap().iter().any(|p| p == uuid)
    }

    /// Tell Godot to remove a player this server manages, then forget it here.
    /// `item` is the genericprops item of the player (`object_uuid` is what
    /// Godot keys on). A player not managed here is skipped, so the same quit
    /// can safely reach this server through several paths.
    fn quit_player(&self, item: serde_json::Value) {
        let object_uuid = item["object_uuid"].as_str().unwrap_or_default().to_string();
        if !self.is_running() || !self.is_managed_player(&object_uuid) {
            debug!("🔧 DsGameServerPlugin: Player {} is not on this server, skipping player_quit.", object_uuid);
            return;
        }
        info!("🔧 DsGameServerPlugin: Received player_quit event: {:?}", item);

        let server = self.clone();
        crate::plugin_rt().spawn(async move {
            let result = spawn_player::handle_player_quit(item, Arc::clone(&server.websocket_sender)).await;
            if result.is_ok() {
                let mut players = server.managed_players.lock().unwrap();
                if let Some(pos) = players.iter().position(|x| x == &object_uuid) {
                    players.remove(pos);
                    info!("🔧 DsGameServerPlugin: Removed player {} from managed_players after quit", object_uuid);
                }
                server.managed_objects.lock().unwrap().remove(&object_uuid);
                player_movement::forget_velocity(&object_uuid);
            } else {
                error!("🔧 DsGameServerPlugin: Failed to send remove_player for {}", object_uuid);
            }
        });
    }

    pub async fn register_handlers(&self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        if self.handlers_registered.swap(true, Ordering::SeqCst) {
            debug!("🔧 DsGameServerPlugin: handlers already registered for {}", self.uuid);
            return Ok(());
        }
        let events = context.events();

        // --- gameserverplugin:spawn_object: a new prop; forward it if it lives in our zones.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_plugin("gameserverplugin", "spawn_object", move |event: serde_json::Value| {
                let object_type = event["object_type"].as_str().unwrap_or_default().to_string();
                let object_uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
                let world_object = is_world_object(&object_type);
                if !(if world_object { server.is_connected() } else { server.is_running() }) {
                    return Ok(());
                }
                debug!("🔧 DsGameServerPlugin: Adding prop with event: {:?}", event);

                if !world_object {
                    let Some(world) = ObjectWorld::from_object_data(&event["object_data"]) else {
                        debug!("🔧 DsGameServerPlugin: spawn_object {} without _world, skipping", object_uuid);
                        return Ok(());
                    };
                    if !zones_contain(&server.zones.read().unwrap(), &world) {
                        debug!("🔧 DsGameServerPlugin: prop {} is outside our zones, skipping spawn.", object_uuid);
                        return Ok(());
                    }
                    server.managed_objects.lock().unwrap().insert(object_uuid);
                }

                let websocket_sender = Arc::clone(&server.websocket_sender);
                let mut wire = event;
                ObjectWorld::strip_internal_keys(&mut wire["object_data"]);
                rt.spawn(async move {
                    let _ = spawn_prop::handle_spawn_prop(wire, websocket_sender).await;
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- gameserverplugin:update_prop: property update; forward it when the object
        //     is in our zones, or when we already simulate it. A managed object that
        //     drove out of our zones (a vehicle) stays ours: the other server has it
        //     frozen and an update_prop does not wake it up on Godot, so freezing it here
        //     would leave it stuck at the border. Prop handover is a follow-up; the
        //     seated player is transferred through player_out_of_zone meanwhile.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_plugin("gameserverplugin", "update_prop", move |event: serde_json::Value| {
                let object_type = event["object_type"].as_str().unwrap_or_default().to_string();
                let object_uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
                let world_object = is_world_object(&object_type);
                if !(if world_object { server.is_connected() } else { server.is_running() }) {
                    return Ok(());
                }
                debug!("🔧 DsGameServerPlugin: Updating prop with event: {:?}", event);

                if !world_object {
                    let in_zones = ObjectWorld::from_object_data(&event["object_data"])
                        .map_or(false, |world| zones_contain(&server.zones.read().unwrap(), &world));
                    let managed = server.managed_objects.lock().unwrap().contains(&object_uuid);
                    if !in_zones && !managed {
                        debug!("🔧 DsGameServerPlugin: prop {} is outside our zones, skipping update.", object_uuid);
                        return Ok(());
                    }
                    if !in_zones {
                        debug!("🔧 DsGameServerPlugin: managed {} {} is outside our zones, still ours until handed over", object_type, object_uuid);
                    }
                }

                let mut wire = event;
                ObjectWorld::strip_internal_keys(&mut wire["object_data"]);
                let websocket_sender = Arc::clone(&server.websocket_sender);
                rt.spawn(async move {
                    let _ = update_prop::handle_update_prop(wire, websocket_sender).await;
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- plugingameserver:new_player: a player just spawned in Horizon.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_plugin("plugingameserver", "new_player", move |event: serde_json::Value| {
                if !server.is_running() {
                    return Ok(());
                }
                let object_uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
                info!("🔧 DsGameServerPlugin: new_player handler FIRED uuid={} on {}", object_uuid, server.server_name);

                let Some(world) = ObjectWorld::from_object_data(&event["object_data"]) else {
                    warn!("🔧 DsGameServerPlugin: new_player {} without _world, skipping", object_uuid);
                    return Ok(());
                };
                let zones = server.zones.read().unwrap().clone();
                if !zones_contain(&zones, &world) {
                    info!(
                        "🔧 DsGameServerPlugin: player {} world={:?} planet={:?} local=({:.1}, {:.1}, {:.1}) is OUTSIDE zones [{}], skipping",
                        object_uuid, world.world, world.planet_name, world.local_position.x, world.local_position.y,
                        world.local_position.z, zones_label(&zones)
                    );
                    return Ok(());
                }
                if server.is_managed_player(&object_uuid) {
                    debug!("🔧 DsGameServerPlugin: player {} already managed, skipping duplicate new_player", object_uuid);
                    return Ok(());
                }

                info!("🔧 DsGameServerPlugin: New player event: {:?}", event);
                server.seed_player_parent(&object_uuid, &event["object_data"]);
                server.managed_players.lock().unwrap().push(object_uuid);

                let websocket_sender = Arc::clone(&server.websocket_sender);
                rt.spawn(async move {
                    let _ = spawn_player::handle_spawn_player(event, websocket_sender).await;
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- client movement: forwarded to the server managing that player.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_client("movement", "update_velocity", move |event: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {
                // Kept whoever manages the player: the client only sends a velocity when
                // it changes, and the server they are transferred to needs the last one.
                player_movement::remember_velocity(&event.player_id.to_string(), &event.data);
                if !server.is_running() || !server.is_managed_player(&event.player_id.to_string()) {
                    debug!("🔧 DsGameServerPlugin: Player {} is not managed by this server, skipping movement", event.player_id);
                    return Ok(());
                }
                debug!("📝 LoggerPlugin: 🦘 Client movement from player {}", event.player_id);
                let websocket_sender = Arc::clone(&server.websocket_sender);
                rt.spawn(async move {
                    let _ = player_movement::handle_player_movement(event, websocket_sender).await;
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- client action (jump, press...): forwarded to the server managing that player.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_client("player", "client_action", move |event: ClientEventWrapper<serde_json::Value>, _player_id: PlayerId, _connection: ClientConnectionRef| {
                if !server.is_running() || !server.is_managed_player(&event.player_id.to_string()) {
                    debug!("🔧 DsGameServerPlugin: Player {} is not managed by this server, skipping action", event.player_id);
                    return Ok(());
                }
                debug!("📝 LoggerPlugin: 🦘 Client action from player {}", event.player_id);
                let websocket_sender = Arc::clone(&server.websocket_sender);
                rt.spawn(async move {
                    let _ = player_action::handle_player_action(event, websocket_sender).await;
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- gameserverplugin:player_quit
        {
            let server = self.clone();
            events.on_plugin("gameserverplugin", "player_quit", move |event: serde_json::Value| {
                server.quit_player(event["item"].clone());
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- core:player_disconnected
        //
        // genericprops turns this core event into the `player_quit` above, but it
        // does so from a task spawned on the luminal pool. When that pool is
        // saturated (2026-09-21: ~480 update_property/s pinned every worker) the
        // task never gets its turn, the player stays in managed_players and every
        // serverinfo tick warns about a client that is gone. Handle the core event
        // here as well: same cleanup, on this plugin's own runtime, and whichever
        // of the two runs second is a no-op (`is_managed_player` is false by then).
        {
            let server = self.clone();
            events.on_core("player_disconnected", move |event: PlayerDisconnectedEvent| {
                server.quit_player(json!({
                    "object_type": "player",
                    "object_uuid": event.player_id.to_string(),
                    "object_data": {},
                    "broadcast_only": null,
                }));
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- gameserverplugin:prewarm: a player of another server approaches a border
        //     we share; load the ground on our side before the handover.
        {
            let server = self.clone();
            events.on_plugin("gameserverplugin", "prewarm", move |event: serde_json::Value| {
                if !server.is_running() || event["server_uuid"] == server.uuid {
                    return Ok(());
                }
                let planet_uuid = event["planet_uuid"].as_str().unwrap_or_default().to_string();
                let Some(pos) = Point3::from_value(&event["position"]) else { return Ok(()) };
                let ours = server.zones.read().unwrap().iter().any(|z| {
                    z.planet_uuid() == Some(planet_uuid.as_str())
                        && z.bounds.as_ref().map_or(true, |b| b.contains_with_margin(&pos, PREWARM_DISTANCE))
                });
                if ours {
                    server.send_prewarm(&planet_uuid, &[pos]);
                }
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- gameserverplugin:object_out_of_zone: a prop (a vehicle) drove out of its
        //     server's zones with everything parented under it (seated players, cargo).
        //     The source freezes the subtree, the server owning the destination zone
        //     spawns the prop then its descendants, parents first.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_plugin("gameserverplugin", "object_out_of_zone", move |event: serde_json::Value| {
                if !server.is_running() {
                    return Ok(());
                }
                let Ok(item) = serde_json::from_value::<GenericPropsRequest>(event["item"].clone()) else {
                    error!("🔧 DsGameServerPlugin: object_out_of_zone without a valid item: {:?}", event);
                    return Ok(());
                };
                let children: Vec<GenericPropsRequest> = serde_json::from_value(event["children"].clone()).unwrap_or_default();
                let source_uuid = event["server_uuid"].as_str().unwrap_or_default().to_string();

                if source_uuid == server.uuid {
                    info!("🔧 DsGameServerPlugin: {} {} left {} with {} descendant(s), freezing them",
                        item.object_type, item.object_uuid, server.server_name, children.len());
                    let mut all = children.clone();
                    all.insert(0, item);
                    for obj in &all {
                        server.managed_objects.lock().unwrap().remove(&obj.object_uuid);
                        if obj.object_type == "player" {
                            let mut players = server.managed_players.lock().unwrap();
                            if let Some(pos) = players.iter().position(|x| x == &obj.object_uuid) {
                                players.remove(pos);
                            }
                            server.player_parents.lock().unwrap().remove(&obj.object_uuid);
                        }
                    }
                    let server = server.clone();
                    rt.spawn(async move {
                        for obj in &all {
                            let _ = send_ws(&server.websocket_sender, "freeze_object", &json!({
                                "namespace": "server",
                                "event": "freeze_object",
                                "data": initial_objects::item_on_wire(obj),
                            }));
                        }
                    });
                    return Ok(());
                }

                let Some(world) = ObjectWorld::from_object_data(&item.object_data) else { return Ok(()) };
                let zones = server.zones.read().unwrap().clone();
                let Some(zone) = zone_containing(&zones, &world) else { return Ok(()) };
                info!("🔧 DsGameServerPlugin: {} {} lands in zone {} of {} with {} descendant(s)",
                    item.object_type, item.object_uuid, zone.label(), server.server_name, children.len());

                server.managed_objects.lock().unwrap().insert(item.object_uuid.clone());
                for child in &children {
                    server.managed_objects.lock().unwrap().insert(child.object_uuid.clone());
                    if child.object_type == "player" {
                        server.transferring_players.lock().unwrap().insert(child.object_uuid.clone());
                        server.seed_player_parent(&child.object_uuid, &child.object_data);
                    }
                }
                let server = server.clone();
                rt.spawn(async move {
                    // The prop itself: add_prop, which the Godot server treats as "adopt" when
                    // it already holds a zone-frozen copy (pose + state re-applied, unfrozen).
                    let _ = spawn_prop::handle_spawn_prop(initial_objects::item_on_wire(&item), Arc::clone(&server.websocket_sender)).await;
                    for child in &children {
                        if child.object_type == "player" {
                            let spawned = spawn_player::handle_spawn_player(serde_json::to_value(child).unwrap_or_default(), Arc::clone(&server.websocket_sender)).await;
                            if spawned.is_ok() {
                                {
                                    let mut players = server.managed_players.lock().unwrap();
                                    if !players.contains(&child.object_uuid) {
                                        players.push(child.object_uuid.clone());
                                    }
                                }
                                let _ = player_movement::replay_velocity(&child.object_uuid, &server.websocket_sender);
                            }
                            server.transferring_players.lock().unwrap().remove(&child.object_uuid);
                        } else {
                            let _ = spawn_prop::handle_spawn_prop(initial_objects::item_on_wire(child), Arc::clone(&server.websocket_sender)).await;
                        }
                    }
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        // --- gameserverplugin:player_out_of_zone: a Godot server reports a player that
        //     left its zones. The source server freezes it; the server whose zones
        //     contain the player's world spawns it.
        {
            let server = self.clone();
            let rt = crate::plugin_rt();
            events.on_plugin("gameserverplugin", "player_out_of_zone", move |event: serde_json::Value| {
                if !server.is_running() {
                    return Ok(());
                }
                let object_uuid = event["item"]["object_uuid"].as_str().unwrap_or_default().to_string();
                let source_uuid = event["server_uuid"].as_str().unwrap_or_default().to_string();
                // What the player carries (parented under them) crosses with them.
                let children: Vec<GenericPropsRequest> = serde_json::from_value(event["children"].clone()).unwrap_or_default();
                info!("🔧 DsGameServerPlugin: player_out_of_zone {} from server {} (I am {}), {} carried object(s)",
                    object_uuid, source_uuid, server.uuid, children.len());

                if server.transferring_players.lock().unwrap().contains(&object_uuid) {
                    debug!("🔧 DsGameServerPlugin: Player {} is already being transferred, skipping duplicate out_of_zone event", object_uuid);
                    return Ok(());
                }

                if source_uuid == server.uuid {
                    // Stop forwarding movements immediately, then freeze on Godot.
                    server.transferring_players.lock().unwrap().insert(object_uuid.clone());
                    {
                        let mut players = server.managed_players.lock().unwrap();
                        if let Some(pos) = players.iter().position(|x| x == &object_uuid) {
                            players.remove(pos);
                            info!("🔧 DsGameServerPlugin: Removed player {} from managed_players on source server before freeze", object_uuid);
                        }
                    }
                    server.managed_objects.lock().unwrap().remove(&object_uuid);
                    for child in &children {
                        server.managed_objects.lock().unwrap().remove(&child.object_uuid);
                    }

                    let server = server.clone();
                    let item = event["item"].clone();
                    rt.spawn(async move {
                        let mut wire = item;
                        ObjectWorld::strip_internal_keys(&mut wire["object_data"]);
                        let _ = send_ws(&server.websocket_sender, "freeze_object", &json!({
                            "namespace": "server",
                            "event": "freeze_object",
                            "data": wire,
                        }));
                        // No freeze for the carried objects: the Godot server released them
                        // with the player (they left the tree with them, see
                        // _release_carried_for_transfer) and a freeze for a uuid it no longer
                        // holds would sit in its pending queue forever.
                        server.transferring_players.lock().unwrap().remove(&object_uuid);
                        info!("🔧 DsGameServerPlugin: Freeze complete, cleared transfer guard for player {}", object_uuid);
                    });
                    return Ok(());
                }

                // Destination side: is the player's world inside one of my zones?
                let Some(world) = ObjectWorld::from_object_data(&event["item"]["object_data"]) else {
                    error!("🔧 DsGameServerPlugin: player_out_of_zone {} without _world", object_uuid);
                    return Ok(());
                };
                let zones = server.zones.read().unwrap().clone();
                let Some(zone) = zone_containing(&zones, &world) else {
                    debug!("🔧 DsGameServerPlugin: player {} world={:?} planet={:?} is not in my zones [{}]",
                        object_uuid, world.world, world.planet_name, zones_label(&zones));
                    return Ok(());
                };
                if server.is_managed_player(&object_uuid) {
                    debug!("🔧 DsGameServerPlugin: player {} already managed here, ignoring out_of_zone", object_uuid);
                    return Ok(());
                }
                info!("🔧 DsGameServerPlugin: player {} lands in zone {} of {}", object_uuid, zone.label(), server.server_name);

                // Clamp the spawn position safely inside the destination bounds so Godot
                // physics cannot push a freshly-spawned player back across the border.
                // Only meaningful when the object's direct parent IS the world (chain of
                // at most the planet itself): `position` is then expressed in the same
                // coordinates as the bounds. Inside a vehicle/building the local frame is
                // rotated and a bounds delta would be meaningless.
                let mut spawn_item = event["item"].clone();
                if let (Some(bounds), true) = (&zone.bounds, world.chain.len() <= 1) {
                    let clamped = bounds.clamp_inside(&world.local_position, BOUNDS_SAFE_MARGIN);
                    let delta = Point3::new(
                        clamped.x - world.local_position.x,
                        clamped.y - world.local_position.y,
                        clamped.z - world.local_position.z,
                    );
                    if delta.x.abs() > 0.001 || delta.y.abs() > 0.001 || delta.z.abs() > 0.001 {
                        if let Some(pos) = spawn_item["object_data"].get_mut("position") {
                            let lx = pos.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let ly = pos.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let lz = pos.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            pos["x"] = json!(lx + delta.x);
                            pos["y"] = json!(ly + delta.y);
                            pos["z"] = json!(lz + delta.z);
                        }
                        info!("[TRANSFER DEBUG] Clamped spawn position for player {} (delta: {:.3}, {:.3}, {:.3})", object_uuid, delta.x, delta.y, delta.z);
                    }
                }

                server.transferring_players.lock().unwrap().insert(object_uuid.clone());
                server.seed_player_parent(&object_uuid, &spawn_item["object_data"]);
                // DON'T add to managed_players yet - wait until the spawn was sent to Godot,
                // so no movement is forwarded before the player entity exists there.
                let server = server.clone();
                rt.spawn(async move {
                    debug!("[TRANSFER DEBUG] Spawning player {} on Godot server", object_uuid);
                    let result = spawn_player::handle_spawn_player(spawn_item, Arc::clone(&server.websocket_sender)).await;
                    if result.is_ok() {
                        {
                            let mut players = server.managed_players.lock().unwrap();
                            if !players.contains(&object_uuid) {
                                players.push(object_uuid.clone());
                                info!("🔧 DsGameServerPlugin: Added player {} to managed_players after successful spawn", object_uuid);
                            }
                        }
                        server.managed_objects.lock().unwrap().insert(object_uuid.clone());
                        // The player was moving when they crossed: give the new server the
                        // velocity the client will not send again until it changes.
                        let _ = player_movement::replay_velocity(&object_uuid, &server.websocket_sender);
                        // Then what they carry, parented under them: the Godot server adopts
                        // its zone-frozen copy and puts it back in the player's hands.
                        for child in &children {
                            server.managed_objects.lock().unwrap().insert(child.object_uuid.clone());
                            let _ = spawn_prop::handle_spawn_prop(initial_objects::item_on_wire(child), Arc::clone(&server.websocket_sender)).await;
                        }
                    } else {
                        error!("🔧 DsGameServerPlugin: Failed to spawn player {}, not adding to managed_players", object_uuid);
                    }
                    server.transferring_players.lock().unwrap().remove(&object_uuid);
                    info!("🔧 DsGameServerPlugin: Spawn complete, cleared transfer guard for player {}", object_uuid);
                });
                Ok(())
            }).await
            .map_err(|e| PluginError::ExecutionError(e.to_string()))?;
        }

        Ok(())
    }

    async fn received_queue_processing(
        &self,
        mut rx: UnboundedReceiver<GameServerMessage>,
        events_processor: Arc<EventSystem>,
        manager_tx: mpsc::Sender<ManagerMessage>,
    ) {
        info!("🔧 DsGameServerPlugin: Async processor task started for {}", self.server_name);
        let mut message_count = 0u64;
        let mut warned_fps_alias = false;

        while let Some(msg) = rx.recv().await {
            message_count += 1;
            if message_count % 100 == 0 {
                debug!("Async processor: processed {} messages", message_count);
            }
            debug!("🔧 DsGameServerPlugin: Processing message: {:?}", msg);
            match msg {
                GameServerMessage::PlayerPositions(position_updates) => {
                    for (_gorc_id, player_id, x, y, z, rotx, roty, rotz, out_of_zone, parent_id) in position_updates {
                        let player_uuid = player_id.to_string();
                        if let Some(planet_uuid) = self.near_own_border(&player_uuid, parent_id.as_deref(), Point3::new(x, y, z)) {
                            if let Err(e) = events_processor.emit_plugin("gameserverplugin", "prewarm", &json!({
                                "server_uuid": self.uuid,
                                "player_uuid": player_uuid,
                                "planet_uuid": planet_uuid,
                                "position": Point3::new(x, y, z),
                            })).await {
                                error!("Failed to emit prewarm: {}", e);
                            }
                        }
                        if let Err(e) = events_processor.emit_plugin("genericprops", "playermove", &json!({
                            "object_type": "player",
                            "object_uuid": player_id.to_string(),
                            "object_data": {
                                "player_id": player_id,
                                "position": Vec3::new(x, y, z),
                                "rotation": Vec3::new(rotx, roty, rotz),
                                "out_of_zone": out_of_zone,
                                "parent_id": parent_id,
                            },
                        })).await {
                            error!("Failed to emit plugin event to propsplugin: {}", e);
                        }
                    }
                }
                GameServerMessage::PropPosition(prop_data) => {
                    if prop_data["type"] == "serverinfo" {
                        debug!("🔧 DsGameServerPlugin: Updating server info: {:?}", prop_data);
                        // `tps` = achieved physics ticks per second. `fps` is the pre-rename
                        // field, accepted during the transition.
                        let tps = match prop_data.get("tps").and_then(|v| v.as_u64()) {
                            Some(tps) => tps,
                            None => {
                                if !warned_fps_alias {
                                    warn!("serverinfo from {} has no `tps` field, falling back to deprecated `fps`", self.server_name);
                                    warned_fps_alias = true;
                                }
                                prop_data["fps"].as_u64().unwrap_or_default()
                            }
                        };
                        let data = ServerInfo {
                            uuid: self.uuid.clone(),
                            tps: tps.min(u8::MAX as u64) as u8,
                            chunks_loading: prop_data["chunks_loading"].as_u64().map(|v| v as u32).unwrap_or_default(),
                            objects_number: prop_data["objects_number"].as_u64().map(|v| v as u32).unwrap_or_default(),
                            players_number: prop_data["players_number"].as_u64().map(|v| v as u16).unwrap_or_default(),
                            scenes_number: prop_data["scenes_number"].as_u64().map(|v| v as u32).unwrap_or_default(),
                            server_name: self.server_name.clone(),
                        };
                        *self.last_info.lock().unwrap() = Some((data.clone(), Instant::now()));
                        // Use try_send to avoid blocking if channel is full - this is not critical data
                        if let Err(e) = manager_tx.try_send(ManagerMessage::ServerInfo(data)) {
                            debug!("ServerInfo channel full or closed, dropping update: {}", e);
                        }
                    } else if let Err(e) = events_processor.emit_plugin("genericprops", "update_object", &json!({
                        "object_type": prop_data["type"],
                        "object_uuid": prop_data["uuid"],
                        "object_data": prop_data,
                    })).await {
                        error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                }
                GameServerMessage::PropCreate(prop_data) => {
                    if let Err(e) = events_processor.emit_plugin("genericprops", "create_object_from_gameserver", &json!({
                        "object_type": prop_data["type"],
                        "object_uuid": prop_data["uuid"],
                        "object_data": prop_data,
                    })).await {
                        error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                }
                GameServerMessage::PropDelete(prop_data) => {
                    if let Err(e) = events_processor.emit_plugin("genericprops", "delete_object", &json!({
                        "object_type": prop_data["type"],
                        "object_uuid": prop_data["uuid"],
                        "object_data": prop_data,
                    })).await {
                        error!("Failed to emit plugin event to propsplugin: {}", e);
                    }
                }
                // Unified property replication: every object (player or prop) sent by
                // the game server on "props/update_object" lands here. The genericprops
                // update_object handler merges the whitelisted properties into the
                // object's GORC instance and broadcasts "update_property" to nearby clients.
                GameServerMessage::ObjectUpdate(object_data) => {
                    if let Err(e) = events_processor.emit_plugin("genericprops", "update_object", &json!({
                        "object_type": object_data["type"],
                        "object_uuid": object_data["uuid"],
                        "object_data": object_data,
                    })).await {
                        error!("Failed to emit object update to genericprops: {}", e);
                    }
                }
            }
        }
        warn!("🔧 DsGameServerPlugin: Async processor channel closed for {}", self.server_name);
    }

    /// Parses one `players/position` message into position updates.
    fn parse_player_positions(value: &serde_json::Value) -> Vec<(GorcObjectId, PlayerId, f64, f64, f64, f64, f64, f64, Option<String>, Option<String>)> {
        let mut position_updates = Vec::new();
        let Some(players) = value["data"].as_array() else {
            error!("players/position without data array: {:?}", value);
            return position_updates;
        };
        for player_data in players {
            let Some(uuid_str) = player_data["player_id"].as_str() else {
                error!("Missing player_id in player data: {:?}", player_data);
                continue;
            };
            let (Ok(player_id), Ok(gorc_id)) = (PlayerId::from_str(uuid_str), GorcObjectId::from_str(uuid_str)) else {
                error!("Invalid player_id format: {:?}", player_data["player_id"]);
                continue;
            };
            let (Some(x), Some(y), Some(z), Some(rx), Some(ry), Some(rz)) = (
                player_data["pos"]["x"].as_f64(),
                player_data["pos"]["y"].as_f64(),
                player_data["pos"]["z"].as_f64(),
                player_data["rot"]["x"].as_f64(),
                player_data["rot"]["y"].as_f64(),
                player_data["rot"]["z"].as_f64(),
            ) else {
                error!("Invalid position coordinates in player data: {:?}", player_data["pos"]);
                continue;
            };
            let out_of_zone = player_data.get("out_of_zone").and_then(|v| v.as_str()).map(|s| s.to_string());
            let parent_id = player_data.get("parent_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            position_updates.push((gorc_id, player_id, x, y, z, rx, ry, rz, out_of_zone, parent_id));
        }
        position_updates
    }

    fn dispatch_text(&self, s: &str, tx: &UnboundedSender<GameServerMessage>) {
        debug!("[message][from][gamesever]: {}", s);
        let Ok(value) = serde_json::from_str::<serde_json::Value>(s) else {
            debug!("Failed to parse incoming JSON: {}", s);
            return;
        };
        let namespace = value["namespace"].as_str().unwrap_or_default();
        let event = value["event"].as_str().unwrap_or_default();
        match (namespace, event) {
            ("players", "position") => {
                let position_updates = Self::parse_player_positions(&value);
                if !position_updates.is_empty() {
                    if let Err(e) = tx.send(GameServerMessage::PlayerPositions(position_updates)) {
                        error!("Failed to send position updates to processor: {}", e);
                    }
                }
            }
            ("props", "position") | ("props", "create_object") | ("props", "delete_object") | ("props", "update_object") => {
                let Some(items) = value["data"].as_array() else {
                    error!("props/{} without data array", event);
                    return;
                };
                for item in items {
                    let msg = match event {
                        "position" => GameServerMessage::PropPosition(item.clone()),
                        "create_object" => GameServerMessage::PropCreate(item.clone()),
                        "delete_object" => GameServerMessage::PropDelete(item.clone()),
                        _ => GameServerMessage::ObjectUpdate(item.clone()),
                    };
                    if let Err(e) = tx.send(msg) {
                        error!("Failed to send props/{} to processor: {}", event, e);
                    }
                }
            }
            _ => {}
        }
    }

    fn receive_ws_to_queue(&self, tx: UnboundedSender<GameServerMessage>, manager_tx: mpsc::Sender<ManagerMessage>) {
        // Take the reader OUT of the mutex: this loop blocks for the life of the
        // socket, and a reconnection must be able to install a new reader meanwhile.
        let generation = self.connection_generation.load(Ordering::SeqCst);
        let Some(mut receiver) = self.websocket_receiver.lock().unwrap().take() else {
            error!("[mesh] no websocket reader for {}", self.server_name);
            return;
        };
        for msg in receiver.incoming_messages() {
            match msg {
                Ok(OwnedMessage::Text(s)) => self.dispatch_text(&s, &tx),
                Ok(OwnedMessage::Binary(b)) => {
                    if let Ok(s) = String::from_utf8(b) {
                        self.dispatch_text(&s, &tx);
                    }
                }
                Ok(_) => { /* ignore ping/pong/close frames */ }
                Err(WebSocketError::NoDataAvailable) => {
                    info!("[mesh] server {} ({}) disconnected!", self.server_name, self.address);
                    break;
                }
                Err(e) => {
                    error!("[mesh] WebSocket read error on {}: {:?}", self.server_name, e);
                    break;
                }
            }
        }
        if self.connection_generation.load(Ordering::SeqCst) == generation {
            self.mark_offline(&manager_tx);
        } else {
            debug!("[mesh] stale reader of {} ended after a reconnection, ignored", self.server_name);
        }
    }

    /// Spawns the reader (blocking) and the processor tasks for the current socket.
    pub fn receive_messages(&self, context: Arc<dyn ServerContext>, manager_tx: mpsc::Sender<ManagerMessage>) {
        debug!("🔧 DsGameServerPlugin: Setting up WebSocket receiver and processing tasks for {}", self.server_name);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<GameServerMessage>();

        let events = context.events();
        let server = self.clone();
        let processor_tx = manager_tx.clone();
        crate::plugin_rt().spawn(async move {
            server.received_queue_processing(rx, events, processor_tx).await;
        });

        let server = self.clone();
        // Use spawn_blocking for the blocking WebSocket receiver
        crate::plugin_rt().spawn_blocking(move || {
            info!("🔧 DsGameServerPlugin: WebSocket receiver task started for {}", server.server_name);
            server.receive_ws_to_queue(tx, manager_tx);
        });
    }
}

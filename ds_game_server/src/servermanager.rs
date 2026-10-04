//! The ServerManager owns the pool of Godot servers and drives the dynamic server
//! meshing: it hands zones (space / planets) to servers, splits an overloaded
//! server's zones onto an idle one, merges sibling servers back when the load
//! drops, and re-homes zones when a running server disappears.
//!
//! The manager loop only does bookkeeping. A zone transition (split, merge,
//! adoption...) takes a minute — props sent, warm-up, players moved — so it runs
//! as a `MeshOp` on its own task (`MeshWorker`) while the loop keeps consuming
//! `serverinfo`: a loop stalled inside a split used to see every other server
//! silent for the whole hand-over and declare it dead. One op at a time.
//!
//! Everything here runs on the plugin-owned runtime (`crate::plugin_rt()`); never
//! `context.tokio_handle()` nor `block_on` (see the notes in lib.rs).

use crate::handlers::initial_objects::{
    handle_activate_objects, handle_dormant_objects, handle_drop_dormant, handle_freeze_object, handle_initial_object, handle_world_objects,
};
use crate::mesh::{plan_split_n, MeshRules, Rule};
use crate::server::{Server, ServerState};

use ds_common::events::GenericPropsRequest;
use ds_common::mesh_load::{PoolLoad, ServerLoad};
use ds_common::world::{ObjectWorld, Point3};
use ds_common::zone::{is_world_object, zones_contain, zones_label, Zone};
use horizon_event_system::{utils, PlayerId, ServerContext};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// One `serverinfo` sample from a Godot server (every second).
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub uuid: String,
    /// Achieved physics ticks per second.
    pub tps: u8,
    /// Terrain collision chunks still being built (0 = nothing left to load).
    pub chunks_loading: u32,
    pub objects_number: u32,
    pub players_number: u16,
    /// Props the server holds, instantiated or asleep in its registry.
    pub scenes_number: u32,
    /// Scenes actually instantiated (in the Godot tree).
    pub scenes_number_actives: u32,
    pub server_name: String,
    /// The server creates hand-over objects asleep (`dormant`) and wakes them with
    /// `activate_object` (see `hand_over_many`).
    pub dormant_spawn: bool,
}

pub type SnapshotItems = HashMap<String, GenericPropsRequest>;

/// Everything the manager loop reacts to.
#[derive(Debug)]
pub enum ManagerMessage {
    ServerInfo(ServerInfo),
    /// A planet object exists in Horizon (from `props:planet`).
    PlanetDiscovered { uuid: String, name: String },
    /// A server's websocket closed.
    ServerOffline(String),
    /// A server that was offline is connected again; `zones` is what it owned before.
    ServerReconnected { uuid: String, zones: Vec<Zone> },
    /// The transition in flight finished (well or not).
    OpDone(OpResult),
}

#[derive(Debug, Default, Clone)]
struct ServerSamples {
    tps: u8,
    players: u16,
    split_hits: u32,
    /// Last serverinfo received; a running server silent for `SERVER_SILENCE_TIMEOUT`
    /// is treated as gone (a hung Godot process keeps its socket open).
    last_seen: Option<Instant>,
    /// Until then the samples describe the layout before a transition (Godot
    /// erases the players it lost one by one after `update_zones`): ignored.
    settle_until: Option<Instant>,
    /// Zones were (re)assigned and no serverinfo came back since: Godot is still
    /// loading them, its main loop blocked. Held to `LOADING_SILENCE_TIMEOUT`.
    loading: bool,
}

/// One split, kept so the pair can be merged back in reverse order.
#[derive(Debug, Clone)]
struct SplitRecord {
    parent_uuid: String,
    child_uuid: String,
    /// Exact zones of the parent before the split, restored on merge.
    parent_zones_before: Vec<Zone>,
    merge_hits: u32,
}

impl SplitRecord {
    fn same_pair(&self, other: &SplitRecord) -> bool {
        self.parent_uuid == other.parent_uuid && self.child_uuid == other.child_uuid
    }
}

/// A zone transition, run off the manager loop by the `MeshWorker`.
enum MeshOp {
    /// `parent` is overloaded: part of its zones go to idle `children` (one, or two
    /// when it holds several zones: the plan decides how many it actually uses).
    Split { parent: Server, children: Vec<Server> },
    /// The pair of a split is under the merge rule: the child gives its zones back.
    Merge { record: SplitRecord, survivor: Server, released: Server },
    /// Zones nobody simulates (first start, owner gone) go to `server`.
    Adopt { server: Server, zones: Vec<Zone> },
    /// `server` gains `gained` on top of its zones: send it what it misses.
    Grant { server: Server, gained: Vec<Zone> },
    /// An idle server came back: send it the world objects (planets) only.
    Reseed { server: Server },
}

impl MeshOp {
    fn label(&self) -> String {
        match self {
            MeshOp::Split { parent, children } => format!(
                "split {} -> {}",
                parent.server_name,
                children.iter().map(|c| c.server_name.as_str()).collect::<Vec<_>>().join(",")
            ),
            MeshOp::Merge { survivor, released, .. } => format!("merge {} into {}", released.server_name, survivor.server_name),
            MeshOp::Adopt { server, zones } => format!("adopt [{}] on {}", zones_label(zones), server.server_name),
            MeshOp::Grant { server, gained } => format!("grant [{}] to {}", zones_label(gained), server.server_name),
            MeshOp::Reseed { server } => format!("reseed {}", server.server_name),
        }
    }

    /// The servers whose main loop the op is about to stall (object bursts): they
    /// are not held to the silence timeout while it runs.
    fn servers(&self) -> Vec<String> {
        match self {
            MeshOp::Split { parent, children } => std::iter::once(parent).chain(children).map(|s| s.uuid.clone()).collect(),
            MeshOp::Merge { survivor, released, .. } => vec![survivor.uuid.clone(), released.uuid.clone()],
            MeshOp::Adopt { server, .. } | MeshOp::Grant { server, .. } | MeshOp::Reseed { server } => vec![server.uuid.clone()],
        }
    }
}

/// What the manager records once an op is over.
#[derive(Debug)]
enum OpOutcome {
    /// The split went through: each pair can be merged back later. A split onto
    /// two children is recorded as two splits in a row, unwound in reverse order.
    Split(Vec<SplitRecord>),
    /// The merge went through.
    Merged(SplitRecord),
    /// The merge failed; its hit counter starts over.
    MergeFailed(SplitRecord),
    /// Nothing to record (adopt, grant, reseed, aborted split).
    Nothing,
}

#[derive(Debug)]
pub struct OpResult {
    /// The `InFlight::id` of the op; set by `start_ready_ops` once it is over.
    op_id: u64,
    outcome: OpOutcome,
    /// Servers the op involved: touched (silence) and settling once it is over.
    servers: Vec<String>,
    /// Servers put back in the idle pool: their samples are stale.
    released: Vec<String>,
    /// Planets seen in the snapshot the op took.
    planets: Vec<(String, String)>,
}

impl OpResult {
    fn nothing(op_servers: &[String]) -> OpResult {
        OpResult { op_id: 0, outcome: OpOutcome::Nothing, servers: op_servers.to_vec(), released: Vec::new(), planets: Vec::new() }
    }
}

/// An op in flight, as the loop sees it.
struct InFlight {
    id: u64,
    label: String,
    servers: Vec<String>,
    started: Instant,
}

const RECONNECT_DELAY: Duration = Duration::from_secs(10);
/// Ops on disjoint sets of servers run side by side, up to this many. One at a
/// time, an overloaded server waited behind the splits of another one and kept
/// filling up (86 players at 30 tps on preprod, 2026-10-04).
const MAX_OPS_IN_FLIGHT: usize = 4;
/// A running server sends serverinfo every second. Silent this long, it is taken
/// for overloaded, not dead: its main loop is stuck in long frames (village
/// spawns, navmesh bakes: 16-40 s each on minikube, 2026-10-02) and the cure is
/// to give part of its zones away. A split only writes to it, it needs no answer.
const OVERLOAD_SILENCE: Duration = Duration::from_secs(60);
/// Silent this long, it is declared dead and its zones are re-homed. Generous on
/// purpose: re-homing a whole universe onto a cold server froze that one too
/// (100-300 s to load), and the zones bounced from server to server without a
/// single split; a real hang only costs the wait.
const SERVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(300);
/// Same, for a server that has not reported since it was handed zones: loading
/// SandBox and its villages took 93 s, the whole universe 305 s (minikube,
/// 2026-10-02).
const LOADING_SILENCE_TIMEOUT: Duration = Duration::from_secs(420);
/// After a transition the servers involved report the previous layout for a
/// while: Godot grants 5 s of grace after a zone change, then erases the players
/// it lost a few per second. Their samples are ignored that long.
const SETTLE_AFTER_OP: Duration = Duration::from_secs(15);
/// A serverinfo older than this (sent once a second) cannot back a merge.
const MERGE_SAMPLE_MAX_AGE: Duration = Duration::from_secs(3);
/// Time allowed for the websocket handshake of a reconnection attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the manager wakes up to check for silent servers when idle.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// How long the manager waits after the first planet of a burst before assigning them.
const PLANET_BURST_WINDOW: Duration = Duration::from_millis(500);
/// How often the pool name is resolved again (see `Discovery`).
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// How often the whole pool is logged as one `[mesh] state` JSON line (players
/// per server as Godot and Horizon count them, zones with their bounds), for the
/// load tests to plot the meshing over time.
const STATE_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// A running server silent longer than this takes no new player (`servers_load`).
const ACCEPT_SILENCE: Duration = Duration::from_secs(10);
/// A server with more messages than this waiting to be written to it is not
/// reading its socket (frozen): it takes no new player.
const OUTBOX_BACKLOG: usize = 1000;
/// Port of a Godot server when `GAME_SERVER_HOST` gives only a host.
const GAME_SERVER_PORT: u16 = 8980;

/// The pool read from DNS: `host` resolves to every Godot server (on kubernetes, a
/// headless service answers with the IP of each ready pod). It is resolved again
/// every `DISCOVERY_INTERVAL`, so a server pool redeployed while Horizon runs is
/// picked up: the new addresses join the pool, the old ones are retired once
/// their socket is gone. A static `game_servers` entry is never retired.
#[derive(Debug, Clone)]
struct Discovery {
    host: String,
    port: u16,
}

impl Discovery {
    /// `game_servers_dns = "godotserver:8980"` from the config, overridden by the
    /// `GAME_SERVER_HOST` env var (a godotserver run outside the cluster, e.g.
    /// `host.minikube.internal`).
    fn from_config(config: &ds_common::config::Config) -> Option<Discovery> {
        let configured = config.get_value("game_servers_dns").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
        let target = match std::env::var("GAME_SERVER_HOST") {
            Ok(host) if !host.trim().is_empty() => host.trim().to_string(),
            _ => configured,
        };
        Discovery::parse(&target)
    }

    /// `host:port`, or `host` alone on port `GAME_SERVER_PORT`; empty = no discovery.
    fn parse(target: &str) -> Option<Discovery> {
        let target = target.trim();
        if target.is_empty() {
            return None;
        }
        let (host, port) = match target.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => (host, port.parse().unwrap()),
            _ => (target, GAME_SERVER_PORT),
        };
        Some(Discovery { host: host.to_string(), port })
    }

    async fn resolve(&self) -> Result<Vec<String>, String> {
        let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port)).await.map_err(|e| e.to_string())?.collect();
        // A dual-stack name lists each server twice: one family is enough.
        let v4: Vec<std::net::SocketAddr> = resolved.iter().copied().filter(|a| a.is_ipv4()).collect();
        let resolved = if v4.is_empty() { resolved } else { v4 };
        let mut addresses: Vec<String> = resolved.iter().map(|a| format!("ws://{}", a)).collect();
        addresses.sort();
        addresses.dedup();
        Ok(addresses)
    }
}

pub struct ServerManager {
    servers: Vec<Server>,
    known_planets: Vec<(String, String)>,
    split_history: Vec<SplitRecord>,
    samples: HashMap<String, ServerSamples>,
    pending_snapshots: Arc<Mutex<HashMap<String, oneshot::Sender<SnapshotItems>>>>,
    /// `None` in single mode: no split, no merge.
    rules: Option<MeshRules>,
    /// Set by `run`; the I/O side of the ops.
    worker: Option<MeshWorker>,
    /// Ops running, on pairwise disjoint sets of servers.
    in_flight: Vec<InFlight>,
    next_op_id: u64,
    /// Ops waiting for a slot, or for an op in flight on one of their servers
    /// (ops on the same server run in queue order). A server in a queued or
    /// running op is busy: no split or merge is decided on it meanwhile.
    queue: VecDeque<MeshOp>,
    tx: mpsc::Sender<ManagerMessage>,
    rx: mpsc::Receiver<ManagerMessage>,
    /// Addresses listed in the config: part of the pool whatever DNS says.
    static_pool: Vec<String>,
    discovery: Option<Discovery>,
    last_discovery: Option<Instant>,
    last_state_log: Option<Instant>,
}

impl ServerManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<ManagerMessage>(1000);
        ServerManager {
            servers: Vec::new(),
            known_planets: Vec::new(),
            split_history: Vec::new(),
            samples: HashMap::new(),
            pending_snapshots: Arc::new(Mutex::new(HashMap::new())),
            rules: None,
            worker: None,
            in_flight: Vec::new(),
            next_op_id: 1,
            queue: VecDeque::new(),
            tx,
            rx,
            static_pool: Vec::new(),
            discovery: None,
            last_discovery: None,
            last_state_log: None,
        }
    }

    fn server(&self, uuid: &str) -> Option<Server> {
        self.servers.iter().find(|s| s.uuid == uuid).cloned()
    }

    fn running_servers(&self) -> Vec<Server> {
        self.servers.iter().filter(|s| s.is_running()).cloned().collect()
    }

    fn idle_server(&self, except: &str) -> Option<Server> {
        self.idle_servers(except, 1).pop()
    }

    /// Idle servers not reserved by a queued or running op (a split's children
    /// stay idle until the op starts them).
    fn idle_servers(&self, except: &str, n: usize) -> Vec<Server> {
        let busy = self.busy_servers();
        self.servers
            .iter()
            .filter(|s| s.state() == ServerState::Online && s.uuid != except && !busy.contains(&s.uuid))
            .take(n)
            .cloned()
            .collect()
    }

    /// Servers of the ops in flight and in the queue.
    fn busy_servers(&self) -> HashSet<String> {
        self.in_flight
            .iter()
            .flat_map(|op| op.servers.iter().cloned())
            .chain(self.queue.iter().flat_map(|op| op.servers()))
            .collect()
    }

    fn running_op_servers(&self) -> HashSet<String> {
        self.in_flight.iter().flat_map(|op| op.servers.iter().cloned()).collect()
    }

    /// The idle servers a split of `parent` may use: two when it holds several
    /// zones (one of them may carry the crowd and be cut between two children,
    /// see `plan_split_n`), else one.
    fn split_children(&self, parent: &Server) -> Vec<Server> {
        let wanted = if parent.zones().len() >= 2 { 2 } else { 1 };
        self.idle_servers(&parent.uuid, wanted)
    }

    /// The zones the first server gets: space plus every planet known so far.
    fn initial_zones(&self) -> Vec<Zone> {
        let mut zones = vec![Zone::space()];
        zones.extend(self.known_planets.iter().map(|(uuid, name)| Zone::planet(uuid, name)));
        zones
    }

    fn worker(&self) -> MeshWorker {
        self.worker.clone().expect("worker is set by run()")
    }

    // ------------------------------------------------------------------ run

    pub async fn run(mut self, context: Arc<dyn ServerContext>) {
        info!("[mesh] ServerManager starting");

        let config = ds_common::config::Config::new("ds_game_server");
        let addresses: Vec<String> = config.get_vector("game_servers");
        let mode = config.get_value("servers_mode").and_then(|v| v.as_str()).unwrap_or("single").to_string();
        self.rules = match mode.as_str() {
            "single" => None,
            "development" => Some(MeshRules::from_config(&config)),
            "production" => {
                warn!("[mesh] servers_mode=production (kubernetes on demand) is not implemented, using the game_servers pool with split/merge rules");
                Some(MeshRules::from_config(&config))
            }
            other => {
                warn!("[mesh] unknown servers_mode `{}`, defaulting to single", other);
                None
            }
        };
        self.discovery = Discovery::from_config(&config);
        self.static_pool = addresses.clone();
        info!("[mesh] mode={} rules={:?} pool={:?} dns={:?}", mode, self.rules, addresses, self.discovery);
        self.worker = Some(MeshWorker {
            pending_snapshots: Arc::clone(&self.pending_snapshots),
            rules: self.rules.clone(),
            context: context.clone(),
        });

        self.register_manager_handlers(context.clone()).await;

        // Connect the whole pool; a server that is not reachable stays Offline and
        // is retried in the background.
        for address in addresses {
            let mut server = Server::new(address);
            match server.connect() {
                Ok(()) => {
                    info!("[mesh] connected to {} as {} ({})", server.address, server.server_name, server.uuid);
                    let _ = server.register_handlers(context.clone()).await;
                    server.receive_messages(context.clone(), self.tx.clone());
                }
                Err(e) => {
                    error!("[ServerManager] Websocket connect() FAILED for {}: {:?}", server.address, e);
                    self.spawn_reconnect(server.clone(), context.clone(), Vec::new());
                }
            }
            self.servers.push(server);
        }
        self.refresh_pool(&context).await;
        if self.servers.is_empty() && self.discovery.is_none() {
            error!("[mesh] no game_servers configured, nothing to manage");
            return;
        }

        // Planets already created in Horizon (they arrive from resourcesdynamic
        // asynchronously, possibly before us).
        match self.worker().request_snapshot().await {
            Ok(items) => {
                self.learn_planets_from(&items);
                for server in self.servers.iter().filter(|s| s.state() == ServerState::Online) {
                    let _ = handle_world_objects(&items, server);
                }
            }
            Err(e) => warn!("[mesh] initial snapshot unavailable ({}), planets will be learned from props:planet", e),
        }

        match self.servers.iter().find(|s| s.state() == ServerState::Online).cloned() {
            Some(first) => {
                // Start on everything known, and send the objects that already exist
                // (Horizon may have loaded the world before the Godot server showed up).
                let zones = self.initial_zones();
                if first.start(zones.clone(), context.clone()) {
                    self.touch_loading(&first.uuid);
                    self.queue.push_back(MeshOp::Adopt { server: first, zones });
                } else {
                    error!("[mesh] could not start {} on [{}]", first.server_name, zones_label(&zones));
                }
            }
            None => warn!("[mesh] no online server in the pool; will start the first one that connects"),
        }

        let mut backlog: Vec<ManagerMessage> = Vec::new();
        loop {
            if self.last_discovery.map_or(true, |at| at.elapsed() >= DISCOVERY_INTERVAL) {
                self.refresh_pool(&context).await;
            }
            self.check_silent_servers(&context).await;
            if self.last_state_log.map_or(true, |at| at.elapsed() >= STATE_LOG_INTERVAL) {
                self.log_state();
                self.publish_load(&context);
            }
            self.start_ready_ops();
            let message = match backlog.pop() {
                Some(message) => message,
                None => match tokio::time::timeout(WATCHDOG_TICK, self.rx.recv()).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(_) => continue,
                },
            };
            match message {
                ManagerMessage::ServerInfo(info) => self.on_server_info(info, &context).await,
                ManagerMessage::PlanetDiscovered { uuid, name } => {
                    // Planets arrive in one burst (resourcesdynamic answers with the whole
                    // system): gather the ones already queued so the owner gets ONE zone
                    // update instead of one per planet. Other messages wait in `backlog`.
                    let mut planets = vec![(uuid, name)];
                    // The emitter (ds_services) goes planet by planet, a few ms apart:
                    // give the rest of the burst time to land before draining.
                    tokio::time::sleep(PLANET_BURST_WINDOW).await;
                    while let Ok(next) = self.rx.try_recv() {
                        match next {
                            ManagerMessage::PlanetDiscovered { uuid, name } => planets.push((uuid, name)),
                            other => backlog.push(other),
                        }
                    }
                    backlog.reverse();
                    self.on_planets_discovered(planets);
                }
                ManagerMessage::ServerOffline(uuid) => self.on_server_offline(uuid, &context),
                ManagerMessage::ServerReconnected { uuid, zones } => self.on_server_reconnected(uuid, zones, &context),
                ManagerMessage::OpDone(result) => self.on_op_done(result),
            }
        }
        warn!("[mesh] ServerManager channel closed, stopping");
    }

    async fn register_manager_handlers(&self, context: Arc<dyn ServerContext>) {
        let events = context.events();

        let tx = self.tx.clone();
        let result = events.on_plugin("props", "planet", move |event: serde_json::Value| {
            let uuid = event["object_uuid"].as_str().unwrap_or_default().to_string();
            let name = event["object_data"]["name"].as_str().unwrap_or_default().to_string();
            if uuid.is_empty() {
                return Ok(());
            }
            if let Err(e) = tx.try_send(ManagerMessage::PlanetDiscovered { uuid, name }) {
                error!("[mesh] could not queue PlanetDiscovered: {}", e);
            }
            Ok(())
        }).await;
        if let Err(e) = result {
            error!("[mesh] failed to register props:planet handler: {}", e);
        }

        let pending = Arc::clone(&self.pending_snapshots);
        let result = events.on_plugin("gameserver", "objects_snapshot", move |event: serde_json::Value| {
            let request_id = event["request_id"].as_str().unwrap_or_default().to_string();
            let Some(sender) = pending.lock().unwrap().remove(&request_id) else {
                warn!("[mesh] objects_snapshot for unknown request {}", request_id);
                return Ok(());
            };
            match serde_json::from_value::<SnapshotItems>(event["items"].clone()) {
                Ok(items) => {
                    let _ = sender.send(items);
                }
                Err(e) => error!("[mesh] objects_snapshot {} unparsable: {}", request_id, e),
            }
            Ok(())
        }).await;
        if let Err(e) = result {
            error!("[mesh] failed to register gameserver:objects_snapshot handler: {}", e);
        }
    }

    fn learn_planets_from(&mut self, items: &SnapshotItems) {
        for (uuid, name) in planets_in(items) {
            self.remember_planet(uuid, name);
        }
    }

    /// Returns true when the planet was not known yet.
    fn remember_planet(&mut self, uuid: String, name: String) -> bool {
        if self.known_planets.iter().any(|(u, _)| *u == uuid) {
            return false;
        }
        info!("[mesh] planet discovered: {} ({})", name, uuid);
        self.known_planets.push((uuid, name));
        true
    }

    // --------------------------------------------------------------- pool

    /// Resolves the pool name again: new addresses are connected and join the
    /// pool, addresses that left DNS are retired once offline. A server whose
    /// address vanished while its socket still works is kept: DNS lags behind the
    /// endpoints, and the socket is the truth about the process.
    async fn refresh_pool(&mut self, context: &Arc<dyn ServerContext>) {
        let Some(discovery) = self.discovery.clone() else { return };
        self.last_discovery = Some(Instant::now());
        let addresses = match discovery.resolve().await {
            Ok(addresses) => addresses,
            Err(e) => {
                warn!("[mesh] cannot resolve {}:{}: {}", discovery.host, discovery.port, e);
                return;
            }
        };

        for address in &addresses {
            if self.servers.iter().any(|s| &s.address == address) {
                continue;
            }
            let server = Server::new(address.clone());
            info!("[mesh] discovered {} as {} ({})", address, server.server_name, server.uuid);
            // It reports through ServerReconnected like a server coming back.
            self.spawn_connect(server.clone(), context.clone(), Vec::new(), Duration::ZERO);
            self.servers.push(server);
        }

        let static_pool = &self.static_pool;
        let mut retired = Vec::new();
        self.servers.retain(|server| {
            let keep = static_pool.contains(&server.address)
                || addresses.contains(&server.address)
                || server.state() != ServerState::Offline;
            if !keep {
                server.retire();
                retired.push(server.clone());
            }
            keep
        });
        for server in retired {
            info!("[mesh] {} ({}) left DNS: retired from the pool", server.server_name, server.address);
            self.samples.remove(&server.uuid);
            self.split_history.retain(|r| r.parent_uuid != server.uuid && r.child_uuid != server.uuid);
        }
    }

    // ---------------------------------------------------------------- ops

    /// Starts, in queue order, every queued op whose servers are free: not in an
    /// op in flight nor in an earlier queued op (ops on one server keep their
    /// order). Each runs on its own task; its outcome comes back as
    /// `ManagerMessage::OpDone`.
    fn start_ready_ops(&mut self) {
        let mut blocked = self.running_op_servers();
        let mut waiting = VecDeque::new();
        while let Some(op) = self.queue.pop_front() {
            let servers = op.servers();
            let free = servers.iter().all(|u| !blocked.contains(u));
            blocked.extend(servers.iter().cloned());
            if !free || self.in_flight.len() >= MAX_OPS_IN_FLIGHT {
                waiting.push_back(op);
                continue;
            }
            self.start_op(op, servers);
        }
        self.queue = waiting;
    }

    fn start_op(&mut self, op: MeshOp, servers: Vec<String>) {
        let id = self.next_op_id;
        self.next_op_id += 1;
        let label = op.label();
        info!("[mesh] op started: {} ({} in flight)", label, self.in_flight.len() + 1);
        self.in_flight.push(InFlight { id, label: label.clone(), servers: servers.clone(), started: Instant::now() });

        let worker = self.worker();
        let tx = self.tx.clone();
        crate::plugin_rt().spawn(async move {
            // A panic inside the op must not leave the manager waiting forever.
            let mut result = match crate::plugin_rt().spawn(worker.run(op)).await {
                Ok(result) => result,
                Err(e) => {
                    error!("[mesh] op `{}` panicked: {}", label, e);
                    OpResult::nothing(&servers)
                }
            };
            result.op_id = id;
            if let Err(e) = tx.send(ManagerMessage::OpDone(result)).await {
                error!("[mesh] could not report the end of `{}`: {}", label, e);
            }
        });
    }

    fn on_op_done(&mut self, result: OpResult) {
        let mut sent_nothing = false;
        if let Some(index) = self.in_flight.iter().position(|op| op.id == result.op_id) {
            let op = self.in_flight.remove(index);
            info!("[mesh] op done: {} after {:?}", op.label, op.started.elapsed());
            // A split skipped or aborted before the child started sent nothing to
            // anyone: no burst to absorb, and the parent's silence must keep
            // counting (a hung server would be "split" every minute forever).
            sent_nothing = op.label.starts_with("split ") && matches!(result.outcome, OpOutcome::Nothing);
        }
        for (uuid, name) in result.planets {
            self.remember_planet(uuid, name);
        }
        // The Godot servers involved just absorbed an object burst and are still
        // shedding what they lost: neither silence nor their numbers mean anything yet.
        for uuid in result.servers.iter().filter(|_| !sent_nothing) {
            self.touch_loading(uuid);
            self.settle(uuid);
        }
        for uuid in &result.released {
            self.samples.remove(uuid);
        }
        match result.outcome {
            OpOutcome::Split(records) => {
                for record in records {
                    let alive = |uuid: &str| self.server(uuid).map_or(false, |s| s.is_running());
                    if alive(&record.parent_uuid) && alive(&record.child_uuid) {
                        self.split_history.push(record);
                    }
                }
            }
            OpOutcome::Merged(record) => self.split_history.retain(|r| !r.same_pair(&record)),
            OpOutcome::MergeFailed(record) => {
                if let Some(r) = self.split_history.iter_mut().find(|r| r.same_pair(&record)) {
                    r.merge_hits = 0;
                }
            }
            OpOutcome::Nothing => {}
        }
    }

    // ----------------------------------------------------------- messages

    fn on_planets_discovered(&mut self, planets: Vec<(String, String)>) {
        let new_zones: Vec<Zone> = planets
            .into_iter()
            .filter(|(uuid, name)| self.remember_planet(uuid.clone(), name.clone()))
            .map(|(uuid, name)| Zone::planet(&uuid, &name))
            .collect();
        if new_zones.is_empty() {
            return;
        }
        // New planets join the server owning unbounded space (it is where they
        // "appear"); otherwise the first running server.
        let owner = self
            .running_servers()
            .into_iter()
            .find(|s| s.zones().iter().any(|z| z.is_space() && z.bounds.is_none()))
            .or_else(|| self.running_servers().into_iter().next());
        match owner {
            Some(server) => self.queue.push_back(MeshOp::Grant { server, gained: new_zones }),
            None => debug!("[mesh] no running server yet, {} planet(s) will be in the initial zones", new_zones.len()),
        }
    }

    async fn on_server_info(&mut self, info: ServerInfo, context: &Arc<dyn ServerContext>) {
        let Some(server) = self.server(&info.uuid) else { return };
        if !server.is_running() {
            // A released or offline server still sends stale numbers for a while.
            return;
        }
        self.send_servers_info_to_clients(context, &info, &server).await;

        let now = Instant::now();
        let samples = self.samples.entry(info.uuid.clone()).or_default();
        samples.tps = info.tps;
        samples.players = info.players_number;
        // Seconds this sample stands for: 1 normally, the whole gap when Godot was
        // stuck in long frames (one sample every 40-50 s under load, 2026-10-02).
        let covers = samples.last_seen.map_or(1, |seen| now.duration_since(seen).as_secs().clamp(1, u32::MAX as u64) as u32);
        samples.last_seen = Some(now);
        // Godot's count lags (stuck at 68 while 91 players were assigned to it,
        // 2026-10-02): judge the split on the larger of the two counts.
        let players = info.players_number.max(server.players_count().min(u16::MAX as usize) as u16);
        samples.loading = false;

        let Some(rules) = self.rules.clone() else { return };
        // While zones move around, every count describes a layout that is about
        // to change: no decision on it, and no run-up of hits either.
        let settling = samples.settle_until.map_or(false, |until| now < until);
        if settling || self.busy_servers().contains(&info.uuid) {
            if let Some(samples) = self.samples.get_mut(&info.uuid) {
                samples.split_hits = 0;
            }
            return;
        }
        let Some(samples) = self.samples.get_mut(&info.uuid) else { return };
        // `split_after` is in seconds of overload, not in samples: counted per
        // sample, 10 hits took 7 minutes on a server reporting twice a minute.
        if rules.split.split_hit(info.tps, players) {
            samples.split_hits = samples.split_hits.saturating_add(covers);
        } else {
            samples.split_hits = 0;
        }
        if samples.split_hits >= rules.split_after {
            samples.split_hits = 0;
            let children = self.split_children(&server);
            match children.is_empty() {
                false => self.queue.push_back(MeshOp::Split { parent: server, children }),
                true => warn!("[mesh] {} is overloaded (rule {:?}) but no idle server is available in the pool", server.server_name, rules.split),
            }
            return;
        }
        self.evaluate_merge(&rules);
    }

    /// One JSON line describing every server of the pool; see `STATE_LOG_INTERVAL`.
    fn log_state(&mut self) {
        self.last_state_log = Some(Instant::now());
        let servers: Vec<serde_json::Value> = self
            .servers
            .iter()
            .map(|s| {
                let samples = self.samples.get(&s.uuid);
                json!({
                    "name": s.server_name,
                    "uuid": s.uuid,
                    "address": s.address,
                    "state": format!("{:?}", s.state()),
                    "tps": samples.map(|x| x.tps),
                    "players_godot": samples.map(|x| x.players),
                    "players_horizon": s.players_count(),
                    "pending": s.pending.load(std::sync::atomic::Ordering::Relaxed),
                    "outbox": s.outbox_pending.load(std::sync::atomic::Ordering::Relaxed),
                    "split_hits": samples.map(|x| x.split_hits),
                    "settling": self.settling(&s.uuid),
                    "loading": samples.map_or(false, |x| x.loading),
                    "last_seen_ms": samples.and_then(|x| x.last_seen).map(|t| t.elapsed().as_millis() as u64),
                    "zones": s.zones(),
                })
            })
            .collect();
        let state = json!({
            "in_flight": self.in_flight.iter().map(|op| op.label.clone()).collect::<Vec<_>>(),
            "queued": self.queue.len(),
            "splits": self.split_history.len(),
            "servers": servers,
        });
        info!("[mesh] state {}", state);
    }

    /// Sends the load of the running servers to genericprops, which places the new
    /// players in the villages of the least loaded server that can take them.
    fn publish_load(&self, context: &Arc<dyn ServerContext>) {
        let servers: Vec<ServerLoad> = self
            .running_servers()
            .into_iter()
            .map(|s| {
                let samples = self.samples.get(&s.uuid);
                let godot = samples.map_or(0, |x| x.players);
                let players = godot.max(s.players_count().min(u16::MAX as usize) as u16);
                let reporting = samples
                    .and_then(|x| x.last_seen)
                    .map_or(false, |seen| seen.elapsed() <= ACCEPT_SILENCE);
                let loading = samples.map_or(false, |x| x.loading);
                // Judged on the placement capacity, not the split rule: a server
                // split early keeps taking players until it is really full.
                let overloaded = match (&self.rules, samples) {
                    (Some(rules), Some(x)) => rules.placement_full(x.tps, players),
                    _ => false,
                };
                ServerLoad {
                    uuid: s.uuid.clone(),
                    name: s.server_name.clone(),
                    players: players as u32,
                    accepting: reporting && !loading && !overloaded
                        && s.outbox_pending.load(std::sync::atomic::Ordering::Relaxed) < OUTBOX_BACKLOG,
                    zones: s.zones(),
                }
            })
            .collect();
        let capacity = self.rules.as_ref().and_then(|r| r.placement_capacity);
        let load = PoolLoad { servers, capacity };
        let events = context.events();
        crate::plugin_rt().spawn(async move {
            if let Err(e) = events.emit_plugin("genericprops", "servers_load", &load).await {
                error!("[mesh] failed to publish servers_load: {}", e);
            }
        });
    }

    /// `uuid` was just handed zones: its silence clock restarts, with the loading
    /// allowance until its first serverinfo. An idle server never reports, so
    /// without this its last sample is minutes old the moment it is started.
    fn touch_loading(&mut self, uuid: &str) {
        let samples = self.samples.entry(uuid.to_string()).or_default();
        samples.last_seen = Some(Instant::now());
        samples.loading = true;
    }

    fn settle(&mut self, uuid: &str) {
        let samples = self.samples.entry(uuid.to_string()).or_default();
        samples.settle_until = Some(Instant::now() + SETTLE_AFTER_OP);
        samples.split_hits = 0;
    }

    fn settling(&self, uuid: &str) -> bool {
        self.samples.get(uuid).and_then(|s| s.settle_until).map_or(false, |until| Instant::now() < until)
    }

    /// Declares dead every running server that stopped reporting: a Godot process
    /// deadlocked keeps its websocket open, so the reader never notices. The servers
    /// of the op in flight are exempt: they are busy instantiating what it sent.
    async fn check_silent_servers(&mut self, context: &Arc<dyn ServerContext>) {
        let now = Instant::now();
        let busy = self.running_op_servers();
        let silent: Vec<Server> = self
            .running_servers()
            .into_iter()
            .filter(|s| !busy.contains(&s.uuid))
            .filter(|s| {
                self.samples.get(&s.uuid).map_or(false, |x| {
                    let timeout = if x.loading { LOADING_SILENCE_TIMEOUT } else { SERVER_SILENCE_TIMEOUT };
                    x.last_seen.map_or(false, |seen| now.duration_since(seen) > timeout)
                })
            })
            .collect();
        // Silent but not dead yet: overloaded, give part of its zones away (one
        // server per pass, only if the pool has room; the op makes it busy meanwhile).
        if self.rules.is_some() {
            let queued = self.busy_servers();
            let overloaded = self
                .running_servers()
                .into_iter()
                .filter(|s| !silent.iter().any(|d| d.uuid == s.uuid))
                .filter(|s| !queued.contains(&s.uuid))
                // Nobody to relieve it of (still loading what it was handed).
                .filter(|s| s.players_count() > 0)
                .find(|s| {
                    self.samples
                        .get(&s.uuid)
                        .and_then(|x| x.last_seen)
                        .map_or(false, |seen| now.duration_since(seen) > OVERLOAD_SILENCE)
                });
            if let Some(server) = overloaded {
                let children = self.split_children(&server);
                match children.is_empty() {
                    false => {
                        warn!(
                            "[mesh] {} sent no serverinfo for {:?}: overloaded, splitting it",
                            server.server_name, OVERLOAD_SILENCE
                        );
                        self.queue.push_back(MeshOp::Split { parent: server, children });
                    }
                    true => debug!("[mesh] {} is silent but no idle server is available in the pool", server.server_name),
                }
            }
        }
        for server in silent {
            error!(
                "[mesh] {} ({}) sent no serverinfo for too long (loading={}): declaring it dead",
                server.server_name, server.address, self.samples.get(&server.uuid).map_or(false, |x| x.loading)
            );
            // Dropping the writer makes every later send fail; the blocked reader ends
            // whenever the socket finally closes and mark_offline is idempotent.
            *server.websocket_sender.lock().unwrap() = None;
            server.set_state(ServerState::Offline);
            self.on_server_offline(server.uuid.clone(), context);
        }
    }

    fn on_server_offline(&mut self, uuid: String, context: &Arc<dyn ServerContext>) {
        let Some(server) = self.server(&uuid) else { return };
        self.samples.remove(&uuid);
        let zones = server.zones();
        let was_running = !zones.is_empty();
        warn!("[mesh] server {} ({}) went offline, zones=[{}]", server.server_name, uuid, zones_label(&zones));

        let events = context.events();
        let server_uuid = uuid.clone();
        crate::plugin_rt().spawn(async move {
            if let Err(e) = events.emit_plugin("ds_game_server", "server_unregistered", &json!({ "server_uuid": server_uuid })).await {
                error!("[mesh] failed to emit server_unregistered: {}", e);
            }
        });

        // Its split records are void: the pair cannot be merged any more.
        self.split_history.retain(|r| r.parent_uuid != uuid && r.child_uuid != uuid);
        *server.zones.write().unwrap() = Vec::new();
        server.managed_objects.lock().unwrap().clear();
        server.managed_players.lock().unwrap().clear();
        crate::ownership::release_all(&server.uuid);

        let mut rehomed = false;
        if was_running {
            match self.idle_server(&uuid) {
                Some(idle) => {
                    error!("[mesh] re-homing the zones of {} onto {}", server.server_name, idle.server_name);
                    // The idle server is reserved now so that a split queued behind
                    // does not pick it up; the objects follow when the op runs.
                    if idle.start(zones.clone(), context.clone()) {
                        // Running from now on: without a fresh clock the next watchdog
                        // pass declared it dead at once, and re-homed onto the next idle
                        // server, through the whole pool in 1 ms (2026-10-02).
                        self.touch_loading(&idle.uuid);
                        self.queue.push_back(MeshOp::Adopt { server: idle, zones: zones.clone() });
                        rehomed = true;
                    } else {
                        error!("[mesh] could not start {} on the orphaned zones", idle.server_name);
                    }
                }
                None => error!("[mesh] no idle server to take over the zones of {}; they wait for a reconnection", server.server_name),
            }
        }
        // Zones re-homed above are not given back on reconnection; only the case
        // where nobody could take them keeps them for the returning server.
        let zones_for_return = if was_running && !rehomed && self.running_servers().is_empty() { zones } else { Vec::new() };
        self.spawn_reconnect(server, context.clone(), zones_for_return);
    }

    fn on_server_reconnected(&mut self, uuid: String, zones: Vec<Zone>, context: &Arc<dyn ServerContext>) {
        let Some(server) = self.server(&uuid) else { return };
        info!("[mesh] server {} ({}) is online", server.server_name, uuid);
        if self.running_servers().is_empty() {
            // Nobody simulates anything: this server takes the world back (its own
            // zones if they were kept for it, else everything). The objects are
            // re-sent since the Godot process may have restarted from scratch.
            // Started right away: a whole pool connecting at once (first start,
            // redeploy) must not elect several servers before the op runs.
            let zones = if zones.is_empty() { self.initial_zones() } else { zones };
            if server.start(zones.clone(), context.clone()) {
                self.touch_loading(&server.uuid);
                self.queue.push_back(MeshOp::Adopt { server, zones });
            } else {
                error!("[mesh] could not start {} on [{}]", server.server_name, zones_label(&zones));
            }
        } else {
            // Back in the pool: keep it warm with the planets. A server declared
            // dead may only have been frozen: it still simulates the zones it had
            // (and their players, now owned by someone else) until told otherwise.
            server.release();
            self.queue.push_back(MeshOp::Reseed { server });
        }
    }

    /// Reconnects an offline server in the background and reports back.
    fn spawn_reconnect(&self, server: Server, context: Arc<dyn ServerContext>, zones: Vec<Zone>) {
        self.spawn_connect(server, context, zones, RECONNECT_DELAY);
    }

    /// Connects `server` in the background, first after `initial_delay` then every
    /// `RECONNECT_DELAY`, until it is connected or leaves the Offline state
    /// (retired). Reports `ServerReconnected` with `zones` once connected.
    fn spawn_connect(&self, server: Server, context: Arc<dyn ServerContext>, zones: Vec<Zone>, initial_delay: Duration) {
        let tx = self.tx.clone();
        crate::plugin_rt().spawn(async move {
            let server = server;
            let mut delay = initial_delay;
            loop {
                tokio::time::sleep(delay).await;
                delay = RECONNECT_DELAY;
                if server.state() != ServerState::Offline {
                    return;
                }
                // connect() blocks (no handshake timeout in the websocket crate): run it
                // on a blocking thread and give up after a while — a hung Godot accepts
                // the TCP connection but never answers the handshake.
                let attempt = server.clone();
                let connected = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    crate::plugin_rt().spawn_blocking(move || {
                        let mut attempt = attempt;
                        attempt.connect().map_err(|e| format!("{:?}", e))
                    }),
                )
                .await;
                let result = match connected {
                    Ok(Ok(result)) => result,
                    Ok(Err(e)) => Err(format!("connect task failed: {}", e)),
                    Err(_) => Err(format!("no websocket handshake within {:?}", CONNECT_TIMEOUT)),
                };
                match result {
                    Ok(()) => {
                        info!("[mesh] reconnected to {} ({})", server.address, server.server_name);
                        let _ = server.register_handlers(context.clone()).await;
                        server.receive_messages(context.clone(), tx.clone());
                        if let Err(e) = tx.send(ManagerMessage::ServerReconnected { uuid: server.uuid.clone(), zones }).await {
                            error!("[mesh] could not report reconnection of {}: {}", server.uuid, e);
                        }
                        return;
                    }
                    Err(e) => debug!("[ServerManager] Websocket connect() FAILED for {}: {}, retrying", server.address, e),
                }
            }
        });
    }

    // ------------------------------------------------------------- merge

    fn evaluate_merge(&mut self, rules: &MeshRules) {
        let busy = self.busy_servers();
        let mut to_merge: Option<SplitRecord> = None;
        for idx in 0..self.split_history.len() {
            let record = self.split_history[idx].clone();
            let (Some(p), Some(c)) = (self.server(&record.parent_uuid), self.server(&record.child_uuid)) else {
                self.split_history[idx].merge_hits = 0;
                continue;
            };
            if !p.is_running() || !c.is_running() || self.settling(&p.uuid) || self.settling(&c.uuid)
                || busy.contains(&p.uuid) || busy.contains(&c.uuid)
            {
                self.split_history[idx].merge_hits = 0;
                continue;
            }
            // Splits unwind in reverse order: a pair is mergeable only while no
            // later split touched either of its servers.
            let touched_later = self.split_history[idx + 1..]
                .iter()
                .any(|r| [&r.parent_uuid, &r.child_uuid].iter().any(|u| **u == p.uuid || **u == c.uuid));
            if touched_later {
                self.split_history[idx].merge_hits = 0;
                continue;
            }
            let (Some(sp), Some(sc)) = (self.samples.get(&p.uuid), self.samples.get(&c.uuid)) else { continue };
            // A stale sample says nothing about now: a Godot server absorbing a
            // hand-over burst stops reporting for tens of seconds.
            let fresh = |s: &ServerSamples| s.last_seen.map_or(false, |at| at.elapsed() < MERGE_SAMPLE_MAX_AGE);
            if !fresh(sp) || !fresh(sc) {
                self.split_history[idx].merge_hits = 0;
                continue;
            }
            // Godot counts a player once it has instantiated it, which lags the
            // hand-over by up to two minutes under a props burst: its count alone
            // reads ~0 right after a split and merged 200+ players back onto one
            // server (preprod, 2026-10-01). Horizon's own count is what was handed over.
            let players = |s: &ServerSamples, server: &Server| s.players.max(server.players_count().min(u16::MAX as usize) as u16);
            let hit = rules.merge.merge_hit((sp.tps, players(sp, &p)), (sc.tps, players(sc, &c)));
            let record = &mut self.split_history[idx];
            record.merge_hits = if hit { record.merge_hits + 1 } else { 0 };
            if record.merge_hits >= rules.merge_after && to_merge.is_none() {
                record.merge_hits = 0;
                to_merge = Some(record.clone());
            }
        }
        if let Some(record) = to_merge {
            let (Some(survivor), Some(released)) = (self.server(&record.parent_uuid), self.server(&record.child_uuid)) else { return };
            self.queue.push_back(MeshOp::Merge { record, survivor, released });
        }
    }

    // ------------------------------------------------------------ clients

    async fn send_servers_info_to_clients(&self, context: &Arc<dyn ServerContext>, serverinfo: &ServerInfo, server: &Server) {
        let events = context.events();
        let sender = events.get_client_response_sender();

        let event = json!({
            "godotserver": {
                "uuid": serverinfo.uuid,
                "tps": serverinfo.tps,
                "objects_number": serverinfo.objects_number,
                "players_number": serverinfo.players_number,
                "scenes_number": serverinfo.scenes_number,
                "scenes_number_actives": serverinfo.scenes_number_actives,
                "zones": server.zones(),
                "name": serverinfo.server_name,
            },
            "universe": {
                // Distinct players: a player listed by two servers (mesh transition) counts once.
                "players_number": crate::ownership::count(),
                "godotservers_number": self.servers.iter().filter(|s| s.is_running()).count(),
            }
        });

        let mut client_event = json!({
            "event_type": "update_property",
            "object_id": serverinfo.uuid, // server uuid
            "object_type": "serverinfo",
            "channel": 0,
            "player_id": "".to_string(),
            "data": event,
            "timestamp": utils::current_timestamp()
        });

        // Collect player IDs before spawning the async task to avoid holding the MutexGuard across await
        let player_ids: Vec<String> = server.managed_players.lock().unwrap().clone();
        crate::plugin_rt().spawn(async move {
            for player_id in player_ids.iter() {
                client_event["player_id"] = player_id.clone().into();
                let data = serde_json::to_vec(&client_event).unwrap_or_default();
                if let Some(sender_arc) = &sender {
                    let obj_player_id = PlayerId::from_str(player_id.as_str()).unwrap_or_else(|_| PlayerId::new());
                    if let Err(e) = sender_arc.send_to_client(obj_player_id, data).await {
                        warn!("Failed to send serverinfo to player {}: {}", player_id, e);
                    }
                } else {
                    warn!("No client response sender available to send event to player {}", player_id);
                }
            }
        });
    }
}

/// Objects the players of `items` are seated in (vehicles): the `parent_id` of a
/// player with a `seat`. A player on foot has a parent too (planet frame,
/// apartment), which must not be held back.
fn ridden_objects(items: &SnapshotItems) -> HashSet<String> {
    let text = |i: &GenericPropsRequest, key: &str| i.object_data.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    items
        .values()
        .filter(|i| i.object_type == "player" && !text(i, "seat").is_empty())
        .map(|i| text(i, "parent_id"))
        .filter(|parent| !parent.is_empty())
        .collect()
}

/// `base` with the objects of `fresh` replacing theirs (and added when new).
fn overlay(mut base: SnapshotItems, fresh: Option<SnapshotItems>) -> SnapshotItems {
    if let Some(fresh) = fresh {
        base.extend(fresh);
    }
    base
}

/// Measure only (2026-10-04): would freezing only what Horizon believes `parent`
/// simulates (`managed_objects` / `managed_players`) miss anything? Logs how many
/// objects the split freezes, how many of those lie in the zones `parent` gives
/// away, and how many of those are NOT in its managed sets — with a sample. If
/// that last count stays ~0, the freeze can be cut down to the managed sets.
/// Must run before the freeze, which empties the sets.
fn log_freeze_coverage(parent: &Server, items: &SnapshotItems, before: &[Zone], keep: &[Zone]) {
    let managed = parent.managed_objects.lock().unwrap().clone();
    let players: HashSet<String> = parent.managed_players.lock().unwrap().iter().cloned().collect();
    let (mut frozen, mut in_lost, mut managed_frozen) = (0usize, 0usize, 0usize);
    let mut missed: Vec<String> = Vec::new();
    for item in items.values().filter(|i| !is_world_object(&i.object_type)) {
        let Some(world) = ObjectWorld::from_object_data(&item.object_data) else { continue };
        if zones_contain(keep, &world) {
            continue;
        }
        frozen += 1;
        let is_managed = managed.contains(&item.object_uuid) || players.contains(&item.object_uuid);
        if is_managed {
            managed_frozen += 1;
        }
        if zones_contain(before, &world) {
            in_lost += 1;
            if !is_managed {
                missed.push(format!("{}:{}", item.object_type, item.object_uuid));
            }
        }
    }
    let sample: Vec<&String> = missed.iter().take(10).collect();
    info!(
        "[mesh] freeze coverage on {}: freezing {} objects, {} managed, {} in the zones given away, {} of those NOT managed (sample {:?})",
        parent.server_name, frozen, managed_frozen, in_lost, missed.len(), sample
    );
}

fn planets_in(items: &SnapshotItems) -> Vec<(String, String)> {
    items
        .values()
        .filter(|i| i.object_type == "planet")
        .map(|i| (i.object_uuid.clone(), i.object_data["name"].as_str().unwrap_or_default().to_string()))
        .collect()
}

// ====================================================================== worker

/// The I/O side of a `MeshOp`: snapshots, object hand-overs, warm-ups. Holds no
/// manager state; whatever it decides comes back to the loop as an `OpResult`.
#[derive(Clone)]
struct MeshWorker {
    pending_snapshots: Arc<Mutex<HashMap<String, oneshot::Sender<SnapshotItems>>>>,
    rules: Option<MeshRules>,
    context: Arc<dyn ServerContext>,
}

impl MeshWorker {
    async fn run(self, op: MeshOp) -> OpResult {
        let servers = op.servers();
        match op {
            MeshOp::Split { parent, children } => self.split(&parent, &children, &servers).await,
            MeshOp::Merge { record, survivor, released } => self.merge(record, &survivor, &released, &servers).await,
            MeshOp::Adopt { server, zones } => self.adopt_zones(&server, zones, &servers).await,
            MeshOp::Grant { server, gained } => self.grant_zones(&server, &gained, &servers).await,
            MeshOp::Reseed { server } => {
                let mut result = OpResult::nothing(&servers);
                if let Ok(items) = self.request_snapshot().await {
                    result.planets = planets_in(&items);
                    let _ = handle_world_objects(&items, &server);
                }
                result
            }
        }
    }

    /// Asks genericprops for every object (with `_world`) and waits for the answer.
    async fn request_snapshot(&self) -> Result<SnapshotItems, String> {
        self.request_snapshot_of(None).await
    }

    /// Same, for the listed objects only (`None`: every object). Building the whole
    /// world took ~2.5 s under load (20k objects): a hand-over refreshing only its
    /// players and their vehicles from it spawned them 2-3 s in the past.
    async fn request_snapshot_of(&self, uuids: Option<Vec<String>>) -> Result<SnapshotItems, String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<SnapshotItems>();
        self.pending_snapshots.lock().unwrap().insert(request_id.clone(), tx);

        let timeout = self.rules.as_ref().map(|r| r.snapshot_timeout).unwrap_or(Duration::from_secs(5));
        let request = match uuids {
            Some(uuids) => json!({ "request_id": request_id, "uuids": uuids }),
            None => json!({ "request_id": request_id }),
        };
        if let Err(e) = self.context.events().emit_plugin("genericprops", "get_objects_snapshot", &request).await {
            self.pending_snapshots.lock().unwrap().remove(&request_id);
            return Err(format!("emit get_objects_snapshot failed: {}", e));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(items)) => Ok(items),
            Ok(Err(_)) => Err("snapshot sender dropped".to_string()),
            Err(_) => {
                self.pending_snapshots.lock().unwrap().remove(&request_id);
                Err(format!("snapshot {} timed out after {:?}", request_id, timeout))
            }
        }
    }

    // ------------------------------------------------------ split / merge

    async fn split(&self, parent: &Server, children: &[Server], op_servers: &[String]) -> OpResult {
        let mut result = OpResult::nothing(op_servers);
        let items = match self.request_snapshot().await {
            Ok(items) => items,
            Err(e) => {
                error!("[mesh] split of {} aborted: {}", parent.server_name, e);
                return result;
            }
        };
        result.planets = planets_in(&items);

        let parent_zones = parent.zones();
        let players: Vec<ObjectWorld> = items
            .values()
            .filter(|i| i.object_type == "player")
            .filter_map(|i| ObjectWorld::from_object_data(&i.object_data))
            .collect();
        let Some(plan) = plan_split_n(&parent_zones, &players, children.len()) else {
            error!("[mesh] {} has no zones to split", parent.server_name);
            return result;
        };
        // Over the players rule with nobody in its zones to give away: the count
        // still describes players it already handed over. Nothing to gain here.
        if plan.give_players() == 0 && matches!(self.rules.as_ref().map(|r| r.split), Some(Rule::Players(_))) {
            warn!(
                "[mesh] split of {} skipped: its zones [{}] hold no player to give away (count not settled yet?)",
                parent.server_name, zones_label(&parent_zones)
            );
            return result;
        }
        // One child per give, in order; an idle server the plan did not need stays idle.
        let used: Vec<(&Server, &Vec<Zone>)> = children.iter().zip(plan.gives.iter().map(|(zones, _)| zones)).collect();
        info!(
            "[mesh] split {} -> {}: keep=[{}] ({} players) {}",
            parent.server_name,
            used.iter().map(|(c, _)| c.server_name.as_str()).collect::<Vec<_>>().join(","),
            zones_label(&plan.keep),
            plan.keep_players,
            used.iter()
                .zip(&plan.gives)
                .map(|((c, _), (zones, n))| format!("give {}=[{}] ({} players)", c.server_name, zones_label(zones), n))
                .collect::<Vec<_>>()
                .join(" ")
        );

        // The children start first and get the ground under the players they will
        // receive loaded; the parent keeps simulating them meanwhile (its zones
        // shrink only after the warm-up, and Godot grants 5 s of grace after a zone
        // change before it reports anyone out of zone).
        for (i, (child, give)) in used.iter().enumerate() {
            if !child.start((*give).clone(), self.context.clone()) {
                error!("[mesh] split aborted: could not start {}", child.server_name);
                for (started, _) in &used[..i] {
                    started.release();
                    result.released.push(started.uuid.clone());
                }
                return result;
            }
        }
        result.servers = std::iter::once(parent).chain(used.iter().map(|(c, _)| *c)).map(|s| s.uuid.clone()).collect();
        // The children get what lies in their zones (props first, then the players
        // once the ground is ready) and manage it; only then does the parent shrink
        // and freeze what it lost — judged on the one snapshot every child's
        // players came from, not the one taken before the warm-up (see hand_over).
        let parent_players = parent.managed_players.lock().unwrap().clone();
        let woken = used.iter().all(|(child, _)| child.dormant_spawn());
        let fresh = self.hand_over_many(&used, &items, self.warmup(), &parent_players).await;
        let mut current = overlay(items, fresh);
        // The children woke the players and their vehicles already (asleep path): the
        // parent lets go of them NOW, before its zones change — Godot applies a zone
        // change (forgetting ~20k objects) in one long frame, and the freezes queued
        // behind it left the players simulated by both servers for 1.5-3 s (preprod).
        // Without the asleep path the children are still creating them: no early freeze.
        if woken {
            let movers: HashSet<String> = ridden_objects(&current).into_iter().chain(
                current.values().filter(|i| i.object_type == "player").map(|i| i.object_uuid.clone()),
            ).collect();
            let early: SnapshotItems = current.iter().filter(|(uuid, _)| movers.contains(*uuid)).map(|(k, v)| (k.clone(), v.clone())).collect();
            if let Err(e) = handle_freeze_object(&early, parent, &plan.keep) {
                error!("[mesh] early freeze on {} failed: {}", parent.server_name, e);
            }
            current.retain(|uuid, _| !movers.contains(uuid));
        }
        if !parent.update_zones(plan.keep.clone()) {
            error!("[mesh] split aborted: could not send the new zones to {}; releasing the children", parent.server_name);
            for (child, _) in &used {
                child.release();
                result.released.push(child.uuid.clone());
            }
            return result;
        }
        log_freeze_coverage(parent, &current, &parent_zones, &plan.keep);
        // Only what the parent simulates, plus every player (one that arrived during
        // the warm-up may be on it without being listed yet). The whole snapshot used
        // to go: ~20k freezes per split, nearly all for objects a child server never
        // had (20137 frozen for 91 managed, preprod 2026-10-04), queued on a Godot
        // server that had just been relieved because it was overloaded.
        let managed = parent.managed_objects.lock().unwrap().clone();
        let current: SnapshotItems = current
            .into_iter()
            .filter(|(uuid, i)| i.object_type == "player" || managed.contains(uuid))
            .collect();
        if let Err(e) = handle_freeze_object(&current, parent, &plan.keep) {
            error!("[mesh] freeze on {} failed: {}", parent.server_name, e);
        }

        // Recorded as successive single splits: the parent gave the last give
        // first, so each merge (latest first) restores the zones it had before.
        let mut records = Vec::new();
        let mut before = parent_zones;
        for (i, (child, _)) in used.iter().enumerate() {
            records.push(SplitRecord {
                parent_uuid: parent.uuid.clone(),
                child_uuid: child.uuid.clone(),
                parent_zones_before: before.clone(),
                merge_hits: 0,
            });
            before = plan.keep.iter().cloned().chain(plan.gives[i + 1..].iter().flat_map(|(z, _)| z.clone())).collect();
        }
        result.outcome = OpOutcome::Split(records);
        result
    }

    async fn merge(&self, record: SplitRecord, survivor: &Server, released: &Server, op_servers: &[String]) -> OpResult {
        let mut result = OpResult::nothing(op_servers);
        let items = match self.request_snapshot().await {
            Ok(items) => items,
            Err(e) => {
                error!("[mesh] merge of {} into {} aborted: {}", released.server_name, survivor.server_name, e);
                result.outcome = OpOutcome::MergeFailed(record);
                return result;
            }
        };
        result.planets = planets_in(&items);

        let released_zones = released.zones();
        let survivor_zones = record.parent_zones_before.clone();
        info!(
            "[mesh] merge {} into {}: zones=[{}] (taking back [{}])",
            released.server_name, survivor.server_name, zones_label(&survivor_zones), zones_label(&released_zones)
        );
        if !survivor.update_zones(survivor_zones.clone()) {
            error!("[mesh] merge aborted: could not send zones to {}", survivor.server_name);
            result.outcome = OpOutcome::MergeFailed(record);
            return result;
        }

        // Objects living in the released zones move to the survivor; the ones it
        // already simulates (its own players, props it never let go) are skipped so
        // Godot does not respawn them.
        let already_managed = survivor.managed_objects.lock().unwrap().clone();
        let already_players = survivor.managed_players.lock().unwrap().clone();
        let to_move: SnapshotItems = items
            .iter()
            .filter(|(_, i)| !is_world_object(&i.object_type))
            .filter(|(_, i)| ObjectWorld::from_object_data(&i.object_data).map_or(false, |w| zones_contain(&released_zones, &w)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let to_spawn: SnapshotItems = to_move
            .iter()
            .filter(|(uuid, i)| !already_managed.contains(*uuid) && !(i.object_type == "player" && already_players.contains(uuid)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        info!("[mesh] merge moves {} objects ({} to spawn on {})", to_move.len(), to_spawn.len(), survivor.server_name);
        // Freeze on the released server what the fresh snapshot (the one the
        // survivor's players were spawned from) still places in its zones: a player
        // who arrived there during the warm-up must leave with the others.
        let released_players = released.managed_players.lock().unwrap().clone();
        let fresh = self.hand_over(survivor, &to_spawn, &survivor_zones, self.warmup(), &released_players).await;
        let to_freeze: SnapshotItems = overlay(to_move, fresh)
            .into_iter()
            .filter(|(_, i)| !is_world_object(&i.object_type))
            .filter(|(_, i)| ObjectWorld::from_object_data(&i.object_data).map_or(false, |w| zones_contain(&released_zones, &w)))
            .collect();
        if let Err(e) = handle_freeze_object(&to_freeze, released, &[]) {
            error!("[mesh] freeze on {} failed: {}", released.server_name, e);
        }
        released.release();
        info!("[mesh] merged: {} back in the pool ({:?})", released.server_name, released.state());
        result.released.push(released.uuid.clone());
        result.outcome = OpOutcome::Merged(record);
        result
    }

    /// Gives `zones` to a server and sends it the objects (first start, or a
    /// running server that disappeared). A server already started on them (the
    /// re-homing reserved it) only gets the objects.
    async fn adopt_zones(&self, server: &Server, zones: Vec<Zone>, op_servers: &[String]) -> OpResult {
        let mut result = OpResult::nothing(op_servers);
        if !server.is_running() && !server.start(zones.clone(), self.context.clone()) {
            error!("[mesh] could not start {} on the orphaned zones", server.server_name);
            return result;
        }
        match self.request_snapshot().await {
            Ok(items) => {
                result.planets = planets_in(&items);
                // Nobody simulates these players any more: no point waiting.
                self.hand_over(server, &items, &zones, Duration::ZERO, &[]).await;
            }
            Err(e) => error!("[mesh] {} started on [{}] but the snapshot failed: {}", server.server_name, zones_label(&zones), e),
        }
        result
    }

    /// Appends `gained` to a running server's zones and sends it the objects living
    /// there that it does not manage yet (the zones are read when the op runs: an
    /// earlier grant may have landed since it was queued). Objects
    /// created before a zone was assigned were never forwarded by the per-server
    /// `spawn_object` handler (their world was outside the zones at that time), so
    /// every zone gain must be followed by this catch-up.
    async fn grant_zones(&self, server: &Server, gained: &[Zone], op_servers: &[String]) -> OpResult {
        let mut result = OpResult::nothing(op_servers);
        let mut zones = server.zones();
        let missing: Vec<Zone> = gained.iter().filter(|g| !zones.contains(g)).cloned().collect();
        zones.extend(missing);
        if !server.update_zones(zones.clone()) {
            return result;
        }
        let items = match self.request_snapshot().await {
            Ok(items) => items,
            Err(e) => {
                error!("[mesh] {} gained zones [{}] but the snapshot failed: {}", server.server_name, zones_label(gained), e);
                return result;
            }
        };
        result.planets = planets_in(&items);
        let managed = server.managed_objects.lock().unwrap().clone();
        let players = server.managed_players.lock().unwrap().clone();
        let to_spawn: SnapshotItems = items
            .into_iter()
            .filter(|(uuid, i)| !is_world_object(&i.object_type) && !managed.contains(uuid) && !players.contains(uuid))
            .filter(|(_, i)| ObjectWorld::from_object_data(&i.object_data).map_or(false, |w| zones_contain(gained, &w)))
            .collect();
        if to_spawn.is_empty() {
            return result;
        }
        info!("[mesh] {} gained [{}]: sending {} objects it did not have", server.server_name, zones_label(gained), to_spawn.len());
        self.hand_over(server, &to_spawn, &zones, Duration::ZERO, &[]).await;
        result
    }

    // --------------------------------------------------------- hand-over

    /// Sends `items` to `server` in two phases: the props first, so the server
    /// instantiates them (a burst that stalls its main loop for seconds) while the
    /// players are still simulated elsewhere; then, after the ground under the
    /// players is prewarmed and `warmup` has elapsed, the players themselves.
    ///
    /// Returns the snapshot the players were taken from when one was re-requested
    /// after the warm-up. The caller MUST freeze from that same snapshot: a player
    /// who crossed into the parent's kept zones during the warm-up (a normal
    /// out_of_zone transfer from a neighbour) is outside `keep` in the first
    /// snapshot and inside it in the fresh one; freezing from the first one erases
    /// them on the parent while the child, working from the fresh one, does not
    /// spawn them — nobody simulates them any more (preprod, 2026-09-20).
    async fn hand_over(&self, server: &Server, items: &SnapshotItems, zones: &[Zone], warmup: Duration, also_players: &[String]) -> Option<SnapshotItems> {
        let zones = zones.to_vec();
        self.hand_over_many(&[(server, &zones)], items, warmup, also_players).await
    }

    /// `hand_over` to several servers at once, each for its own zones: the props
    /// and prewarms go out to all of them, they warm up in parallel, and the
    /// players are sent from ONE fresh snapshot — the one the caller freezes from
    /// (laid over its own with `overlay`).
    ///
    /// The fresh snapshot holds the players and what they ride only: the players
    /// of `items`, `also_players` (those the releasing server simulates now, who
    /// may have arrived during the warm-up) and the objects seated players ride.
    async fn hand_over_many(&self, targets: &[(&Server, &Vec<Zone>)], items: &SnapshotItems, warmup: Duration, also_players: &[String]) -> Option<SnapshotItems> {
        let (players, mut props): (SnapshotItems, SnapshotItems) =
            items.iter().map(|(k, v)| (k.clone(), v.clone())).partition(|(_, i)| i.object_type == "player");
        // A vehicle someone sits in moves with them: sent with the props, it would
        // stand where the first snapshot saw it while its driver, spawned from the
        // fresh one in vehicle-local coordinates, is seated back in it — the whole
        // ride of the warm-up rolled back (preprod, 2026-10-04). It goes with the
        // players instead, from the same snapshot.
        let ridden = ridden_objects(&players);
        props.retain(|uuid, _| !ridden.contains(uuid));
        let first_rides: SnapshotItems = items
            .iter()
            .filter(|(uuid, i)| ridden.contains(*uuid) && i.object_type != "player")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Per target: the players sent to it asleep, when it can take them that way.
        let mut ready: Vec<(&Server, &Vec<Zone>, Option<HashSet<String>>)> = Vec::new();
        for (server, zones) in targets {
            if let Err(e) = handle_initial_object(&props, server, zones) {
                error!("[mesh] initial props to {} failed: {}", server.server_name, e);
                continue;
            }
            self.prewarm_players(server, &players, zones).await;
            // Asleep during the warm-up (see handle_dormant_objects): creating ~60
            // players at the switch froze the new server ~2.4 s, and with it every
            // player handed over. Only with a warm-up (a switch to come) and a server
            // that announced it can.
            let mut asleep = None;
            if !warmup.is_zero() && server.dormant_spawn() {
                match handle_dormant_objects(&first_rides, server, zones).and_then(|_| handle_dormant_objects(&players, server, zones)) {
                    Ok(sent) => asleep = Some(sent),
                    Err(e) => error!("[mesh] asleep players to {} failed: {}", server.server_name, e),
                }
            }
            ready.push((*server, *zones, asleep));
        }
        if ready.is_empty() {
            return None;
        }
        let mut fresh_snapshot = None;
        let mut fresh_players = None;
        if !warmup.is_zero() {
            futures::future::join_all(ready.iter().map(|(server, _, _)| self.wait_until_ready(server, warmup))).await;
            // The players kept walking on their current server while we waited:
            // spawn them where they are NOW, not where the first snapshot saw them.
            let mut wanted: HashSet<String> = players.keys().cloned().collect();
            wanted.extend(also_players.iter().cloned());
            wanted.extend(ridden.iter().cloned());
            match self.request_snapshot_of(Some(wanted.into_iter().collect())).await {
                Ok(fresh) => {
                    fresh_players = Some(fresh.iter().filter(|(_, i)| i.object_type == "player").map(|(k, v)| (k.clone(), v.clone())).collect::<SnapshotItems>());
                    fresh_snapshot = Some(fresh);
                }
                Err(e) => warn!("[mesh] could not refresh player positions before the hand-over: {}", e),
            }
        }
        let source = fresh_players.as_ref().unwrap_or(&players);
        let rides: SnapshotItems = fresh_snapshot
            .as_ref()
            .unwrap_or(items)
            .iter()
            .filter(|(uuid, i)| ridden.contains(*uuid) && i.object_type != "player")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (server, zones, asleep) in ready {
            if !rides.is_empty() {
                // Before the players, who are seated in them on arrival. One the
                // server already has (moved in by an out_of_zone transfer) is skipped.
                let managed = server.managed_objects.lock().unwrap().clone();
                let to_send: SnapshotItems = rides.iter().filter(|(uuid, _)| !managed.contains(*uuid)).map(|(k, v)| (k.clone(), v.clone())).collect();
                let sent = match asleep {
                    Some(_) => handle_activate_objects(&to_send, server, zones),
                    None => handle_initial_object(&to_send, server, zones),
                };
                if let Err(e) = sent {
                    error!("[mesh] vehicles to {} failed: {}", server.server_name, e);
                }
            }
            // Whoever already landed on `server` meanwhile (out_of_zone transfer into
            // its zones, granted at start) is not spawned a second time, and a player
            // outside `zones` is not sent at all: Godot would only spawn then erase it.
            let already_players = server.managed_players.lock().unwrap().clone();
            let to_send: SnapshotItems = if fresh_players.is_some() {
                source
                    .iter()
                    .filter(|(uuid, _)| !already_players.contains(*uuid))
                    .filter(|(_, i)| ObjectWorld::from_object_data(&i.object_data).map_or(false, |w| zones_contain(zones, &w)))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            } else {
                source.clone()
            };
            match asleep {
                Some(asleep) => {
                    // Woken with what they are doing NOW; the ones that arrived during
                    // the warm-up are created the normal way by the same message.
                    if let Err(e) = handle_activate_objects(&to_send, server, zones) {
                        error!("[mesh] players to {} failed: {}", server.server_name, e);
                    }
                    // Asleep there but no longer coming: gone, or out of these zones.
                    // One that crossed in meanwhile is live there, not asleep: not dropped.
                    let gone: Vec<String> = asleep
                        .into_iter()
                        .filter(|uuid| !to_send.contains_key(uuid) && !already_players.contains(uuid) && players.contains_key(uuid))
                        .collect();
                    if let Err(e) = handle_drop_dormant(&players, &gone, server) {
                        error!("[mesh] dropping asleep players on {} failed: {}", server.server_name, e);
                    }
                }
                None => {
                    if let Err(e) = handle_initial_object(&to_send, server, zones) {
                        error!("[mesh] initial players to {} failed: {}", server.server_name, e);
                    }
                }
            }
        }
        fresh_snapshot
    }

    /// Tells `server` where the players of `items` that land in `zones` stand, so
    /// it loads the ground before they arrive.
    async fn prewarm_players(&self, server: &Server, items: &SnapshotItems, zones: &[Zone]) {
        let mut by_planet: HashMap<String, Vec<Point3>> = HashMap::new();
        for item in items.values().filter(|i| i.object_type == "player") {
            let Some(world) = ObjectWorld::from_object_data(&item.object_data) else { continue };
            if !zones_contain(zones, &world) {
                continue;
            }
            if let Some(planet) = &world.planet_uuid {
                by_planet.entry(planet.clone()).or_default().push(world.local_position);
            }
        }
        if by_planet.is_empty() {
            return;
        }
        for (planet, positions) in &by_planet {
            server.send_prewarm(planet, positions);
        }
    }

    /// Waits at least `min_wait` after the props/prewarm were sent, then until the
    /// server reports a fresh serverinfo at `ready_tps` or more with no chunk
    /// loading left — its main loop has absorbed the instantiation burst and the
    /// ground is built. Gives up after `warmup_max`.
    async fn wait_until_ready(&self, server: &Server, min_wait: Duration) {
        let (ready_tps, max_wait) = self
            .rules
            .as_ref()
            .map(|r| (r.ready_tps, r.warmup_max.max(min_wait)))
            .unwrap_or((58, Duration::from_secs(30)));
        let started = Instant::now();
        info!("[mesh] waiting for {} to be ready (min {:?}, max {:?}, tps >= {})", server.server_name, min_wait, max_wait, ready_tps);
        tokio::time::sleep(min_wait).await;
        loop {
            let snapshot = server.last_info.lock().unwrap().clone();
            match snapshot {
                Some((info, at)) if at > started + min_wait / 2 && info.tps >= ready_tps && info.chunks_loading == 0 => {
                    info!("[mesh] {} ready after {:?} (tps={}, chunks_loading=0)", server.server_name, started.elapsed(), info.tps);
                    return;
                }
                Some((info, at)) if started.elapsed() >= max_wait => {
                    warn!("[mesh] {} not ready after {:?} (tps={}, chunks_loading={}, sample {:?} old), handing over anyway",
                        server.server_name, started.elapsed(), info.tps, info.chunks_loading, at.elapsed());
                    return;
                }
                None if started.elapsed() >= max_wait => {
                    warn!("[mesh] {} sent no serverinfo in {:?}, handing over anyway", server.server_name, started.elapsed());
                    return;
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    fn warmup(&self) -> Duration {
        self.rules.as_ref().map(|r| r.warmup).unwrap_or(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn parse_host_and_port() {
        let d = Discovery::parse("godotserver:8981").unwrap();
        assert_eq!((d.host.as_str(), d.port), ("godotserver", 8981));
        let d = Discovery::parse(" host.minikube.internal ").unwrap();
        assert_eq!((d.host.as_str(), d.port), ("host.minikube.internal", GAME_SERVER_PORT));
        assert!(Discovery::parse("").is_none());
        assert!(Discovery::parse("  ").is_none());
    }

    #[tokio::test]
    async fn resolve_gives_ws_addresses() {
        let d = Discovery::parse("localhost:8980").unwrap();
        let addresses = d.resolve().await.unwrap();
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|a| a.starts_with("ws://") && a.ends_with(":8980")), "{:?}", addresses);
    }
}

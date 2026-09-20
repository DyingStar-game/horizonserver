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

use crate::handlers::initial_objects::{handle_freeze_object, handle_initial_object, handle_world_objects};
use crate::mesh::{plan_split, MeshRules, Rule};
use crate::server::{Server, ServerState};

use ds_common::events::GenericPropsRequest;
use ds_common::world::{ObjectWorld, Point3};
use ds_common::zone::{is_world_object, zones_contain, zones_label, Zone};
use horizon_event_system::{utils, PlayerId, ServerContext};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
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
    pub scenes_number: u32,
    pub server_name: String,
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
    /// `parent` is overloaded: part of its zones go to the idle `child`.
    Split { parent: Server, child: Server },
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
            MeshOp::Split { parent, child } => format!("split {} -> {}", parent.server_name, child.server_name),
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
            MeshOp::Split { parent, child } => vec![parent.uuid.clone(), child.uuid.clone()],
            MeshOp::Merge { survivor, released, .. } => vec![survivor.uuid.clone(), released.uuid.clone()],
            MeshOp::Adopt { server, .. } | MeshOp::Grant { server, .. } | MeshOp::Reseed { server } => vec![server.uuid.clone()],
        }
    }
}

/// What the manager records once an op is over.
#[derive(Debug)]
enum OpOutcome {
    /// The split went through: the pair can be merged back later.
    Split(SplitRecord),
    /// The merge went through.
    Merged(SplitRecord),
    /// The merge failed; its hit counter starts over.
    MergeFailed(SplitRecord),
    /// Nothing to record (adopt, grant, reseed, aborted split).
    Nothing,
}

#[derive(Debug)]
pub struct OpResult {
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
        OpResult { outcome: OpOutcome::Nothing, servers: op_servers.to_vec(), released: Vec::new(), planets: Vec::new() }
    }
}

/// The op in flight, as the loop sees it.
struct InFlight {
    label: String,
    servers: Vec<String>,
    started: Instant,
}

const RECONNECT_DELAY: Duration = Duration::from_secs(10);
/// A running server sends serverinfo every second; after this much silence it is
/// declared dead and its zones are re-homed. Generous on purpose: a Godot server
/// that just received zones stalls its main loop for tens of seconds while it
/// creates the objects, and a false positive re-homes its players onto another
/// cold server — a cascade far worse than waiting for a real hang.
const SERVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(60);
/// After a transition the servers involved report the previous layout for a
/// while: Godot grants 5 s of grace after a zone change, then erases the players
/// it lost a few per second. Their samples are ignored that long.
const SETTLE_AFTER_OP: Duration = Duration::from_secs(15);
/// Time allowed for the websocket handshake of a reconnection attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the manager wakes up to check for silent servers when idle.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// How long the manager waits after the first planet of a burst before assigning them.
const PLANET_BURST_WINDOW: Duration = Duration::from_millis(500);

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
    in_flight: Option<InFlight>,
    /// Ops that must happen (re-homing, zone grants) waiting for the one in flight.
    /// A split or merge only gets in while nothing is in flight: its rule re-fires
    /// later if still true.
    queue: VecDeque<MeshOp>,
    tx: mpsc::Sender<ManagerMessage>,
    rx: mpsc::Receiver<ManagerMessage>,
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
            in_flight: None,
            queue: VecDeque::new(),
            tx,
            rx,
        }
    }

    fn server(&self, uuid: &str) -> Option<Server> {
        self.servers.iter().find(|s| s.uuid == uuid).cloned()
    }

    fn running_servers(&self) -> Vec<Server> {
        self.servers.iter().filter(|s| s.is_running()).cloned().collect()
    }

    fn idle_server(&self, except: &str) -> Option<Server> {
        self.servers.iter().find(|s| s.state() == ServerState::Online && s.uuid != except).cloned()
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
        info!("[mesh] mode={} rules={:?} pool={:?}", mode, self.rules, addresses);
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
        if self.servers.is_empty() {
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
                self.queue.push_back(MeshOp::Adopt { server: first, zones });
            }
            None => error!("[mesh] no online server in the pool; will start the first one that reconnects"),
        }

        let mut backlog: Vec<ManagerMessage> = Vec::new();
        loop {
            self.check_silent_servers(&context).await;
            self.start_next_op();
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
                ManagerMessage::ServerReconnected { uuid, zones } => self.on_server_reconnected(uuid, zones),
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

    // ---------------------------------------------------------------- ops

    /// Starts the next queued op when none is in flight. Runs it on its own task;
    /// the outcome comes back as `ManagerMessage::OpDone`.
    fn start_next_op(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(op) = self.queue.pop_front() else { return };
        let servers = op.servers();
        let label = op.label();
        info!("[mesh] op started: {}", label);
        self.in_flight = Some(InFlight { label: label.clone(), servers: servers.clone(), started: Instant::now() });

        let worker = self.worker();
        let tx = self.tx.clone();
        crate::plugin_rt().spawn(async move {
            // A panic inside the op must not leave the manager waiting forever.
            let result = match crate::plugin_rt().spawn(worker.run(op)).await {
                Ok(result) => result,
                Err(e) => {
                    error!("[mesh] op `{}` panicked: {}", label, e);
                    OpResult::nothing(&servers)
                }
            };
            if let Err(e) = tx.send(ManagerMessage::OpDone(result)).await {
                error!("[mesh] could not report the end of `{}`: {}", label, e);
            }
        });
    }

    fn on_op_done(&mut self, result: OpResult) {
        if let Some(op) = self.in_flight.take() {
            info!("[mesh] op done: {} after {:?}", op.label, op.started.elapsed());
        }
        for (uuid, name) in result.planets {
            self.remember_planet(uuid, name);
        }
        // The Godot servers involved just absorbed an object burst and are still
        // shedding what they lost: neither silence nor their numbers mean anything yet.
        for uuid in &result.servers {
            self.touch(uuid);
            self.settle(uuid);
        }
        for uuid in &result.released {
            self.samples.remove(uuid);
        }
        match result.outcome {
            OpOutcome::Split(record) => {
                let alive = |uuid: &str| self.server(uuid).map_or(false, |s| s.is_running());
                if alive(&record.parent_uuid) && alive(&record.child_uuid) {
                    self.split_history.push(record);
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
        samples.last_seen = Some(now);

        let Some(rules) = self.rules.clone() else { return };
        // While zones move around, every count describes a layout that is about
        // to change: no decision on it, and no run-up of hits either.
        let settling = samples.settle_until.map_or(false, |until| now < until);
        if self.in_flight.is_some() || settling {
            samples.split_hits = 0;
            return;
        }
        if rules.split.split_hit(info.tps, info.players_number) {
            samples.split_hits += 1;
        } else {
            samples.split_hits = 0;
        }
        if samples.split_hits >= rules.split_after {
            samples.split_hits = 0;
            match self.idle_server(&server.uuid) {
                Some(child) => self.queue.push_back(MeshOp::Split { parent: server, child }),
                None => warn!("[mesh] {} is overloaded (rule {:?}) but no idle server is available in the pool", server.server_name, rules.split),
            }
            return;
        }
        self.evaluate_merge(&rules);
    }

    fn touch(&mut self, uuid: &str) {
        self.samples.entry(uuid.to_string()).or_default().last_seen = Some(Instant::now());
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
        let busy: Vec<String> = self.in_flight.as_ref().map(|op| op.servers.clone()).unwrap_or_default();
        let silent: Vec<Server> = self
            .running_servers()
            .into_iter()
            .filter(|s| !busy.contains(&s.uuid))
            .filter(|s| {
                self.samples
                    .get(&s.uuid)
                    .and_then(|x| x.last_seen)
                    .map_or(false, |seen| now.duration_since(seen) > SERVER_SILENCE_TIMEOUT)
            })
            .collect();
        for server in silent {
            error!(
                "[mesh] {} ({}) sent no serverinfo for {:?}: declaring it dead",
                server.server_name, server.address, SERVER_SILENCE_TIMEOUT
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

        let mut rehomed = false;
        if was_running {
            match self.idle_server(&uuid) {
                Some(idle) => {
                    error!("[mesh] re-homing the zones of {} onto {}", server.server_name, idle.server_name);
                    // The idle server is reserved now so that a split queued behind
                    // does not pick it up; the objects follow when the op runs.
                    if idle.start(zones.clone(), context.clone()) {
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

    fn on_server_reconnected(&mut self, uuid: String, zones: Vec<Zone>) {
        let Some(server) = self.server(&uuid) else { return };
        info!("[mesh] server {} ({}) is back online", server.server_name, uuid);
        if self.running_servers().is_empty() {
            // Nobody simulates anything: this server takes the world back (its own
            // zones if they were kept for it, else everything). The objects are
            // re-sent since the Godot process may have restarted from scratch.
            let zones = if zones.is_empty() { self.initial_zones() } else { zones };
            self.queue.push_back(MeshOp::Adopt { server, zones });
        } else {
            // Back in the pool: keep it warm with the planets.
            self.queue.push_back(MeshOp::Reseed { server });
        }
    }

    /// Reconnects an offline server in the background and reports back.
    fn spawn_reconnect(&self, server: Server, context: Arc<dyn ServerContext>, zones: Vec<Zone>) {
        let tx = self.tx.clone();
        crate::plugin_rt().spawn(async move {
            let server = server;
            loop {
                tokio::time::sleep(RECONNECT_DELAY).await;
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
        let mut to_merge: Option<SplitRecord> = None;
        for idx in 0..self.split_history.len() {
            let record = self.split_history[idx].clone();
            let (Some(p), Some(c)) = (self.server(&record.parent_uuid), self.server(&record.child_uuid)) else {
                self.split_history[idx].merge_hits = 0;
                continue;
            };
            if !p.is_running() || !c.is_running() || self.settling(&p.uuid) || self.settling(&c.uuid) {
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
            let hit = rules.merge.merge_hit((sp.tps, sp.players), (sc.tps, sc.players));
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
                "zones": server.zones(),
                "name": serverinfo.server_name,
            },
            "universe": {
                "players_number": self.servers.iter().map(|s| s.players_count() as u32).sum::<u32>(),
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
            MeshOp::Split { parent, child } => self.split(&parent, &child, &servers).await,
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
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<SnapshotItems>();
        self.pending_snapshots.lock().unwrap().insert(request_id.clone(), tx);

        let timeout = self.rules.as_ref().map(|r| r.snapshot_timeout).unwrap_or(Duration::from_secs(5));
        if let Err(e) = self.context.events().emit_plugin("genericprops", "get_objects_snapshot", &json!({ "request_id": request_id })).await {
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

    async fn split(&self, parent: &Server, child: &Server, op_servers: &[String]) -> OpResult {
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
        let Some(plan) = plan_split(&parent_zones, &players) else {
            error!("[mesh] {} has no zones to split", parent.server_name);
            return result;
        };
        // Over the players rule with nobody in its zones to give away: the count
        // still describes players it already handed over. Nothing to gain here.
        if plan.give_players == 0 && matches!(self.rules.as_ref().map(|r| r.split), Some(Rule::Players(_))) {
            warn!(
                "[mesh] split of {} skipped: its zones [{}] hold no player to give away (count not settled yet?)",
                parent.server_name, zones_label(&parent_zones)
            );
            return result;
        }
        info!(
            "[mesh] split {} -> {}: keep=[{}] ({} players) give=[{}] ({} players)",
            parent.server_name, child.server_name, zones_label(&plan.keep), plan.keep_players,
            zones_label(&plan.give), plan.give_players
        );

        // The child starts first and gets the ground under the players it will
        // receive loaded; the parent keeps simulating them meanwhile (its zones
        // shrink only after the warm-up, and Godot grants 5 s of grace after a zone
        // change before it reports anyone out of zone).
        if !child.start(plan.give.clone(), self.context.clone()) {
            error!("[mesh] split aborted: could not start {}", child.server_name);
            return result;
        }
        // The child gets the whole world (props first, frozen when outside its
        // zones, then the players once the ground is ready) and manages what is in
        // its zones; only then does the parent shrink and freeze what it lost.
        self.hand_over(child, &items, &plan.give, self.warmup()).await;
        if !parent.update_zones(plan.keep.clone()) {
            error!("[mesh] split aborted: could not send the new zones to {}; releasing {}", parent.server_name, child.server_name);
            child.release();
            result.released.push(child.uuid.clone());
            return result;
        }
        if let Err(e) = handle_freeze_object(&items, parent, &plan.keep) {
            error!("[mesh] freeze on {} failed: {}", parent.server_name, e);
        }

        result.outcome = OpOutcome::Split(SplitRecord {
            parent_uuid: parent.uuid.clone(),
            child_uuid: child.uuid.clone(),
            parent_zones_before: parent_zones,
            merge_hits: 0,
        });
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
        self.hand_over(survivor, &to_spawn, &survivor_zones, self.warmup()).await;
        if let Err(e) = handle_freeze_object(&to_move, released, &[]) {
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
                self.hand_over(server, &items, &zones, Duration::ZERO).await;
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
        self.hand_over(server, &to_spawn, &zones, Duration::ZERO).await;
        result
    }

    // --------------------------------------------------------- hand-over

    /// Sends `items` to `server` in two phases: the props first, so the server
    /// instantiates them (a burst that stalls its main loop for seconds) while the
    /// players are still simulated elsewhere; then, after the ground under the
    /// players is prewarmed and `warmup` has elapsed, the players themselves.
    async fn hand_over(&self, server: &Server, items: &SnapshotItems, zones: &[Zone], warmup: Duration) {
        let (mut players, props): (SnapshotItems, SnapshotItems) =
            items.iter().map(|(k, v)| (k.clone(), v.clone())).partition(|(_, i)| i.object_type == "player");
        if let Err(e) = handle_initial_object(&props, server, zones) {
            error!("[mesh] initial props to {} failed: {}", server.server_name, e);
            return;
        }
        self.prewarm_players(server, &players, zones).await;
        if !warmup.is_zero() {
            self.wait_until_ready(server, warmup).await;
            // The players kept walking on their current server while we waited:
            // spawn them where they are NOW, not where the first snapshot saw them.
            match self.request_snapshot().await {
                Ok(fresh) => {
                    for (uuid, item) in players.iter_mut() {
                        if let Some(current) = fresh.get(uuid) {
                            *item = current.clone();
                        }
                    }
                }
                Err(e) => warn!("[mesh] could not refresh player positions before the hand-over: {}", e),
            }
        }
        if let Err(e) = handle_initial_object(&players, server, zones) {
            error!("[mesh] initial players to {} failed: {}", server.server_name, e);
        }
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

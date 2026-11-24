//! Sizing of the Godot server pool on kubernetes (`servers_mode = "production"`, or
//! the `GAME_SERVER_AUTOSCALE` env var): the manager keeps `min_idle` idle servers
//! ready for the next splits by raising the replicas of the godotserver Deployment,
//! and gives back the idle ones it no longer needs.
//!
//! Only the manager knows which servers hold zones, so it picks the pods to delete
//! itself: an idle server is retired from the pool first, its pod gets the lowest
//! `pod-deletion-cost`, then the replicas go down. A server holding zones is never
//! a candidate. `decide` is pure; the manager builds the `PoolView` from the pool
//! and from the pods the API lists (k8s.rs), and applies the `Action`.

use crate::k8s::K8sClient;
use ds_common::config::Config;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::warn;

/// At most this many servers given back per cycle: the pool shrinks slowly, a burst of
/// arrivals right after finds its servers.
const MAX_DRAIN_PER_CYCLE: u32 = 2;
/// How often the pool is sized (one round trip to the API each time).
pub const AUTOSCALE_INTERVAL: Duration = Duration::from_secs(15);
/// A split that lacked idle servers keeps asking for them this long.
pub const STARVED_WINDOW: Duration = Duration::from_secs(60);
/// The address of a drained pod is not connected again while it is still in DNS
/// (a terminating pod may stay listed): forgotten after this, or once DNS drops it.
pub const DRAIN_FORGET: Duration = Duration::from_secs(600);

/// The autoscaler's side of the manager (see servermanager.rs, `autoscale_*`).
pub struct AutoscaleState {
    pub cfg: AutoscaleConfig,
    pub client: K8sClient,
    pub started: Instant,
    /// A round trip to the API is going on (read, then maybe a change): one at a time.
    pub busy: bool,
    pub last_run: Option<Instant>,
    /// Since when each idle server (uuid) is idle, as seen by the cycles.
    pub idle_since: HashMap<String, Instant>,
    /// Servers (uuids) whose pod is being marked for deletion: reserved, no op takes them.
    pub reserved: HashSet<String>,
    /// Addresses of drained pods, not to be connected again (see DRAIN_FORGET).
    pub draining: HashMap<String, Instant>,
    /// Children the last split lacked, and when.
    pub starved: Option<(u32, Instant)>,
    /// Pods on their way at the last cycle: the pool name is resolved more often meanwhile.
    pub en_route: u32,
    /// Last Hold reason and last API error, logged when they change.
    pub last_hold: String,
    pub last_error: String,
    /// For the `[mesh] state` line.
    pub summary: serde_json::Value,
}

impl AutoscaleState {
    pub fn new(cfg: AutoscaleConfig, client: K8sClient) -> AutoscaleState {
        AutoscaleState {
            cfg,
            client,
            started: Instant::now(),
            busy: false,
            last_run: None,
            idle_since: HashMap::new(),
            reserved: HashSet::new(),
            draining: HashMap::new(),
            starved: None,
            en_route: 0,
            last_hold: String::new(),
            last_error: String::new(),
            summary: serde_json::Value::Null,
        }
    }

    /// Time for the next cycle (the first one waits a full interval: the pool connects).
    pub fn due(&self) -> bool {
        !self.busy && self.last_run.unwrap_or(self.started).elapsed() >= AUTOSCALE_INTERVAL
    }

    pub fn starved_children(&self) -> u32 {
        self.starved.filter(|(_, at)| at.elapsed() < STARVED_WINDOW).map_or(0, |(n, _)| n)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoscaleConfig {
    pub deployment: String,
    pub pod_selector: String,
    /// Idle servers kept ready: a split takes up to 4 children at once and a new pod
    /// needs a while to load before it can take one.
    pub min_idle: u32,
    /// Idle servers are given back only above this (hysteresis with `min_idle`).
    pub max_idle: u32,
    pub min_replicas: u32,
    pub max_replicas: u32,
    /// An idle server is given back only after being idle this long (a merge frees a
    /// server that the next split may want back).
    pub scale_down_after: Duration,
    /// No scale-down this long after Horizon started: the pool is still connecting.
    pub startup_grace: Duration,
    /// A pod unschedulable longer than this is not "on its way" any more: the cluster
    /// is full, asking for more replicas would not help.
    pub unschedulable_after: Duration,
    /// Idle servers on these nodes are given back first (an overflow node, e.g. rented).
    pub drain_first_nodes: Vec<String>,
    /// Decisions are logged, nothing is changed.
    pub dry_run: bool,
}

impl AutoscaleConfig {
    /// `None` when the autoscaler is off: `servers_mode` other than `production`, unless
    /// `GAME_SERVER_AUTOSCALE` says otherwise (`1`/`true`, `dry-run`, `0`/`false`), so one
    /// plugins.toml serves minikube (no RBAC) and preprod.
    pub fn from_config(config: &Config, mode: &str, env: Option<&str>) -> Option<AutoscaleConfig> {
        let text = |key: &str, default: &str| config.get_value(key).and_then(|v| v.as_str()).unwrap_or(default).trim().to_string();
        let int = |key: &str, default: i64| config.get_value(key).and_then(|v| v.as_integer()).unwrap_or(default).max(0) as u32;
        let mut dry_run = config.get_value("autoscale_dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let enabled = match env.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "1" || v == "true" => true,
            Some(v) if v == "dry-run" || v == "dry_run" => {
                dry_run = true;
                true
            }
            Some(v) if v == "0" || v == "false" => false,
            _ => mode == "production",
        };
        if !enabled {
            return None;
        }
        let min_idle = int("autoscale_min_idle", 4);
        let mut max_idle = int("autoscale_max_idle", min_idle as i64 + 2);
        if max_idle < min_idle {
            warn!("[autoscale] autoscale_max_idle {} is below autoscale_min_idle {}: using {}", max_idle, min_idle, min_idle);
            max_idle = min_idle;
        }
        let min_replicas = int("autoscale_min_replicas", 5).max(1);
        let mut max_replicas = int("autoscale_max_replicas", 40);
        if max_replicas < min_replicas {
            warn!("[autoscale] autoscale_max_replicas {} is below autoscale_min_replicas {}: using {}", max_replicas, min_replicas, min_replicas);
            max_replicas = min_replicas;
        }
        Some(AutoscaleConfig {
            deployment: text("autoscale_deployment", "godotserver"),
            pod_selector: text("autoscale_pod_selector", "app.kubernetes.io/name=godotserver"),
            min_idle,
            max_idle,
            min_replicas,
            max_replicas,
            scale_down_after: Duration::from_secs(int("autoscale_scale_down_after_secs", 600) as u64),
            startup_grace: Duration::from_secs(int("autoscale_startup_grace_secs", 300) as u64),
            unschedulable_after: Duration::from_secs(int("autoscale_unschedulable_after_secs", 120) as u64),
            drain_first_nodes: config.get_vector("autoscale_drain_first_nodes"),
            dry_run,
        })
    }
}

/// An idle server, with the pod it runs in.
#[derive(Debug, Clone, PartialEq)]
pub struct IdleServer {
    pub uuid: String,
    /// Empty when no pod has its IP (a static pool entry): never given back.
    pub pod: String,
    pub node: String,
    pub idle_for: Duration,
}

/// The pool as the autoscaler sees it, built by the manager at each cycle.
#[derive(Debug, Clone, Default)]
pub struct PoolView {
    /// `spec.replicas` of the Deployment.
    pub replicas: u32,
    /// A rollout is going on.
    pub rolling: bool,
    /// Idle servers not reserved by an op.
    pub idle: Vec<IdleServer>,
    /// Pods that will become servers: scheduled or about to be, not connected yet.
    pub en_route: u32,
    /// Pods the cluster has had no room for since `unschedulable_after`.
    pub unschedulable: u32,
    /// Ops in flight or queued.
    pub ops: usize,
    /// Children a split recently wanted and the pool did not have.
    pub starved_children: u32,
    /// Since the manager started.
    pub uptime: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Set the replicas to this.
    ScaleTo(u32),
    /// Give back these idle servers (uuids): retire them, mark their pods, replicas − n.
    Drain(Vec<String>),
    /// Nothing to do; the reason is logged when it changes.
    Hold(String),
}

pub fn decide(view: &PoolView, cfg: &AutoscaleConfig) -> Action {
    let idle = view.idle.len() as u32;
    let wanted = cfg.min_idle + view.starved_children;
    let coming = idle + view.en_route;

    if coming < wanted || view.replicas < cfg.min_replicas {
        if view.unschedulable > 0 {
            return Action::Hold(format!("{} pod(s) unschedulable: the cluster is full", view.unschedulable));
        }
        let target = (view.replicas + wanted.saturating_sub(coming)).clamp(cfg.min_replicas, cfg.max_replicas);
        if target > view.replicas {
            return Action::ScaleTo(target);
        }
        return Action::Hold(format!("{} idle + {} coming < {} wanted, but at autoscale_max_replicas {}", idle, view.en_route, wanted, cfg.max_replicas));
    }

    if idle <= cfg.max_idle {
        return Action::Hold(String::new());
    }
    let blocker = if view.uptime < cfg.startup_grace {
        Some("startup grace".to_string())
    } else if view.ops > 0 {
        Some(format!("{} op(s) running or queued", view.ops))
    } else if view.rolling {
        Some("rollout in progress".to_string())
    } else if view.en_route > 0 {
        // The ReplicaSet deletes a pending / not ready pod before any marked one.
        Some(format!("{} pod(s) on their way", view.en_route))
    } else if view.unschedulable > 0 {
        Some(format!("{} pod(s) unschedulable", view.unschedulable))
    } else if view.starved_children > 0 {
        Some("a split recently lacked servers".to_string())
    } else {
        None
    };
    if let Some(blocker) = blocker {
        return Action::Hold(format!("{} idle > {} but {}", idle, cfg.max_idle, blocker));
    }
    let n = (idle - cfg.min_idle).min(view.replicas.saturating_sub(cfg.min_replicas)).min(MAX_DRAIN_PER_CYCLE);
    if n == 0 {
        return Action::Hold(format!("{} idle > {} but at autoscale_min_replicas {}", idle, cfg.max_idle, cfg.min_replicas));
    }
    let mut candidates: Vec<&IdleServer> = view.idle.iter().filter(|s| !s.pod.is_empty() && s.idle_for >= cfg.scale_down_after).collect();
    // Overflow nodes first, then the longest idle.
    candidates.sort_by_key(|s| (!cfg.drain_first_nodes.contains(&s.node), std::cmp::Reverse(s.idle_for)));
    let drained: Vec<String> = candidates.into_iter().take(n as usize).map(|s| s.uuid.clone()).collect();
    if drained.is_empty() {
        return Action::Hold(format!("{} idle > {} but none idle for {:?} yet", idle, cfg.max_idle, cfg.scale_down_after));
    }
    Action::Drain(drained)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AutoscaleConfig {
        AutoscaleConfig {
            deployment: "godotserver".into(),
            pod_selector: "app=godotserver".into(),
            min_idle: 4,
            max_idle: 6,
            min_replicas: 5,
            max_replicas: 40,
            scale_down_after: Duration::from_secs(600),
            startup_grace: Duration::from_secs(300),
            unschedulable_after: Duration::from_secs(120),
            drain_first_nodes: vec!["remote".into()],
            dry_run: false,
        }
    }

    fn idle(n: usize, idle_for: u64) -> Vec<IdleServer> {
        (0..n)
            .map(|i| IdleServer { uuid: format!("s{}", i), pod: format!("pod{}", i), node: "node1".into(), idle_for: Duration::from_secs(idle_for) })
            .collect()
    }

    fn steady(replicas: u32, idle_servers: Vec<IdleServer>) -> PoolView {
        PoolView { replicas, idle: idle_servers, uptime: Duration::from_secs(3600), ..Default::default() }
    }

    #[test]
    fn scales_up_to_keep_the_idle_reserve() {
        assert_eq!(decide(&steady(10, idle(1, 0)), &cfg()), Action::ScaleTo(13));
    }

    #[test]
    fn pods_on_their_way_count_as_idle() {
        let view = PoolView { en_route: 3, ..steady(13, idle(1, 0)) };
        assert!(matches!(decide(&view, &cfg()), Action::Hold(_)));
    }

    #[test]
    fn a_starved_split_asks_for_more() {
        let view = PoolView { starved_children: 3, ..steady(10, idle(0, 0)) };
        assert_eq!(decide(&view, &cfg()), Action::ScaleTo(17));
    }

    #[test]
    fn never_above_max_replicas() {
        assert_eq!(decide(&steady(38, idle(0, 0)), &cfg()), Action::ScaleTo(40));
        assert!(matches!(decide(&steady(40, idle(0, 0)), &cfg()), Action::Hold(_)));
    }

    #[test]
    fn no_scale_up_when_the_cluster_is_full() {
        let view = PoolView { unschedulable: 2, ..steady(10, idle(0, 0)) };
        assert!(matches!(decide(&view, &cfg()), Action::Hold(r) if r.contains("unschedulable")));
    }

    #[test]
    fn raises_to_min_replicas() {
        assert_eq!(decide(&steady(1, idle(4, 0)), &cfg()), Action::ScaleTo(5));
    }

    #[test]
    fn hysteresis_between_min_and_max_idle() {
        assert!(matches!(decide(&steady(30, idle(6, 9999)), &cfg()), Action::Hold(_)));
    }

    #[test]
    fn drains_at_most_two_long_idle_servers() {
        match decide(&steady(30, idle(10, 9999)), &cfg()) {
            Action::Drain(uuids) => assert_eq!(uuids.len(), 2),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn drains_only_servers_idle_long_enough() {
        let mut servers = idle(8, 30);
        servers[5].idle_for = Duration::from_secs(700);
        assert_eq!(decide(&steady(30, servers), &cfg()), Action::Drain(vec!["s5".into()]));
    }

    #[test]
    fn drains_overflow_nodes_first() {
        let mut servers = idle(8, 9999);
        servers[7].node = "remote".into();
        servers[7].idle_for = Duration::from_secs(601);
        match decide(&steady(30, servers), &cfg()) {
            Action::Drain(uuids) => assert_eq!(uuids[0], "s7"),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn never_drains_a_server_without_pod() {
        let mut servers = idle(8, 9999);
        for s in servers.iter_mut() {
            s.pod.clear();
        }
        assert!(matches!(decide(&steady(30, servers), &cfg()), Action::Hold(_)));
    }

    #[test]
    fn keeps_min_replicas() {
        match decide(&steady(6, idle(8, 9999)), &cfg()) {
            Action::Drain(uuids) => assert_eq!(uuids.len(), 1),
            other => panic!("{:?}", other),
        }
        assert!(matches!(decide(&steady(5, idle(8, 9999)), &cfg()), Action::Hold(_)));
    }

    #[test]
    fn no_scale_down_while_busy() {
        let base = steady(30, idle(10, 9999));
        for view in [
            PoolView { ops: 1, ..base.clone() },
            PoolView { rolling: true, ..base.clone() },
            PoolView { en_route: 1, ..base.clone() },
            PoolView { uptime: Duration::from_secs(10), ..base.clone() },
        ] {
            assert!(matches!(decide(&view, &cfg()), Action::Hold(_)), "{:?}", view);
        }
    }
}

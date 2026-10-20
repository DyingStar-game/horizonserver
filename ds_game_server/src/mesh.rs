//! Pure decision logic of the dynamic server meshing: split/merge rules read from
//! `plugins.toml`, and how a server's zones are divided between two servers.
//! No I/O here so everything is unit-testable.

use ds_common::config::Config;
use ds_common::world::{ObjectWorld, Point3};
use ds_common::zone::{Bounds, Zone};
use std::time::Duration;
use tracing::warn;

/// Thresholds on the server metrics, `"tps:20"`, `"players:50"` or both in the
/// config: `"players:80 or tps:45"` for a split (any one over its threshold),
/// `"players:25 and tps:55"` for a merge (all of them under it).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rule {
    pub tps: Option<u8>,
    pub players: Option<u16>,
}

/// How the conditions of a rule combine: `or` for `split_rule`, `and` for `merge_rule`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Joiner {
    Or,
    And,
}

impl Joiner {
    fn word(self) -> &'static str {
        match self {
            Joiner::Or => "or",
            Joiner::And => "and",
        }
    }
}

impl Rule {
    pub const fn tps(n: u8) -> Rule {
        Rule { tps: Some(n), players: None }
    }

    pub const fn players(n: u16) -> Rule {
        Rule { tps: None, players: Some(n) }
    }

    pub fn parse(s: &str, joiner: Joiner) -> Result<Rule, String> {
        let other = match joiner {
            Joiner::Or => Joiner::And,
            Joiner::And => Joiner::Or,
        };
        let words: Vec<&str> = s.split_whitespace().collect();
        if words.contains(&other.word()) {
            return Err(format!("rule `{}` joins its conditions with `{}`, this rule takes `{}`", s, other.word(), joiner.word()));
        }
        let mut rule = Rule::default();
        for cond in words.split(|w| *w == joiner.word()) {
            let cond = cond.join("");
            let (kind, value) = cond.split_once(':').ok_or_else(|| format!("rule `{}` must be `tps:N` or `players:N`", s))?;
            let kind = match kind {
                "fps" => {
                    warn!("rule `{}` uses the deprecated `fps:` prefix, read it as `tps:`", s);
                    "tps"
                }
                kind => kind,
            };
            match kind {
                "tps" if rule.tps.is_none() => rule.tps = Some(value.parse::<u8>().map_err(|e| format!("bad tps in `{}`: {}", s, e))?),
                "players" if rule.players.is_none() => {
                    rule.players = Some(value.parse::<u16>().map_err(|e| format!("bad players in `{}`: {}", s, e))?)
                }
                "tps" | "players" => return Err(format!("rule `{}` sets `{}` twice", s, kind)),
                other => return Err(format!("unknown rule kind `{}` in `{}`", other, s)),
            }
        }
        Ok(rule)
    }

    /// Split when the tps falls under its threshold or the players exceed theirs.
    pub fn split_hit(&self, tps: u8, players: u16) -> bool {
        self.tps.is_some_and(|n| tps < n) || self.players.is_some_and(|n| players > n)
    }

    /// Merge two siblings when both run above the tps threshold and their players
    /// sum stays under its threshold (a condition left out always holds).
    pub fn merge_hit(&self, a: (u8, u16), b: (u8, u16)) -> bool {
        self.tps.map_or(true, |n| a.0 > n && b.0 > n) && self.players.map_or(true, |n| (a.1 as u32 + b.1 as u32) < n as u32)
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let conds: Vec<String> = [self.players.map(|n| format!("players:{}", n)), self.tps.map(|n| format!("tps:{}", n))]
            .into_iter()
            .flatten()
            .collect();
        f.write_str(&conds.join(", "))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeshRules {
    pub split: Rule,
    pub merge: Rule,
    /// Seconds the split rule must hold before acting (a sample counts for the
    /// time since the previous one: Godot reports late when it is overloaded).
    pub split_after: u32,
    pub merge_after: u32,
    pub snapshot_timeout: Duration,
    /// Minimum time given to a server after it received the props and the prewarm,
    /// before the players of a split or merge are moved onto it.
    pub warmup: Duration,
    /// Upper bound of that wait when the server never reports itself ready.
    pub warmup_max: Duration,
    /// The server is ready once it reports at least this tps with no chunk loading.
    pub ready_tps: u8,
    /// Players a server may be given by the placement of new players
    /// (`placement_capacity`). Defaults to the threshold of a `players:` split
    /// rule; set it above that threshold so a server is split before it fills up
    /// while the new players keep arriving on it.
    pub placement_capacity: Option<u32>,
    /// Under this tps a server takes no new player (`placement_min_tps`): the arrivals
    /// wait in genericprops' queue until it recovers or is split.
    pub placement_min_tps: u8,
}

/// Most idle servers one split hands zones to.
pub const MAX_SPLIT_CHILDREN: usize = 4;

impl MeshRules {
    /// Idle servers a split asks for. A server far over the `players:` threshold of the
    /// split rule is cut in as many pieces as it takes for each to fall under it (238
    /// players at players:80 = 3 pieces) instead of being halved again every 35-40 s
    /// while its arrivals kept coming (preprod 2026-10-20: 238 players on one server at
    /// 20 tps). At least a plain split (one child, two with several zones), at most
    /// MAX_SPLIT_CHILDREN.
    pub fn split_children_wanted(&self, zones: usize, players: u32) -> usize {
        let base = if zones >= 2 { 2 } else { 1 };
        let pieces = self.split.players.map_or(1, |n| players as usize / n.max(1) as usize + 1);
        // One zone: the parent keeps a piece. Several: the cut zone's pieces all go to children.
        let wanted = if zones >= 2 { pieces } else { pieces - 1 };
        wanted.clamp(base, MAX_SPLIT_CHILDREN)
    }

    /// True when the placement must stop sending new players to this server.
    pub fn placement_full(&self, tps: u8, players: u16) -> bool {
        let full = match self.placement_capacity {
            Some(n) => players as u32 > n || self.split.tps.is_some_and(|n| tps < n),
            None => self.split.split_hit(tps, players),
        };
        full || tps < self.placement_min_tps
    }
}

impl MeshRules {
    pub fn from_config(config: &Config) -> MeshRules {
        let rule = |key: &str, default: &str, joiner: Joiner| -> Rule {
            let raw = config.get_value(key).and_then(|v| v.as_str()).unwrap_or(default);
            Rule::parse(raw, joiner).unwrap_or_else(|e| {
                warn!("[mesh] {}: {} — using default `{}`", key, e, default);
                Rule::parse(default, joiner).expect("default rule parses")
            })
        };
        let int = |key: &str, default: i64| -> i64 {
            config.get_value(key).and_then(|v| v.as_integer()).unwrap_or(default).max(1)
        };
        let split = rule("split_rule", "tps:20", Joiner::Or);
        let placement_capacity = config
            .get_value("placement_capacity")
            .and_then(|v| v.as_integer())
            .map(|n| n.max(1) as u32)
            .or(split.players.map(u32::from));
        if let (Some(n), Some(capacity)) = (split.players, placement_capacity) {
            if capacity <= n as u32 {
                // The server stops taking players before it is over the rule: never split,
                // its queued arrivals wait until players leave it.
                warn!(
                    "[mesh] placement_capacity {} is not above the split rule players:{}: a full server is never split",
                    capacity, n
                );
            }
        }
        let placement_min_tps = config.get_value("placement_min_tps").and_then(|v| v.as_integer()).unwrap_or(45).clamp(0, 255) as u8;
        if placement_min_tps > split.tps.unwrap_or(0) {
            // Between the two the server takes no player and is not split: genericprops'
            // queue never gives up on a server, its arrivals wait until the tps comes back.
            warn!(
                "[mesh] placement_min_tps {} is above the tps threshold of the split rule `{}`: a server in between takes no player and is never split for it",
                placement_min_tps, split
            );
        }
        MeshRules {
            split,
            merge: rule("merge_rule", "players:10", Joiner::And),
            split_after: int("split_after_samples", 10) as u32,
            merge_after: int("merge_after_samples", 30) as u32,
            snapshot_timeout: Duration::from_secs(int("snapshot_timeout_secs", 5) as u64),
            warmup: Duration::from_secs(int("split_warmup_secs", 5) as u64),
            warmup_max: Duration::from_secs(int("split_warmup_max_secs", 30) as u64),
            ready_tps: int("split_ready_tps", 58).clamp(1, 255) as u8,
            placement_capacity,
            placement_min_tps,
        }
    }
}

/// How one server's zones are divided: `keep` stays on the parent, `give` goes to
/// the new child.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitPlan {
    pub keep: Vec<Zone>,
    pub give: Vec<Zone>,
    pub keep_players: usize,
    pub give_players: usize,
}

/// Divides `zones` between a parent and a child. With several zones, whole zones
/// move (greedy: heaviest first, each to the lighter side, the child getting the
/// heaviest). With a single zone, its bounds are cut through the population (see
/// `split_bounds`). `None` only when there is no zone at all.
pub fn plan_split(zones: &[Zone], players: &[ObjectWorld]) -> Option<SplitPlan> {
    match zones {
        [] => None,
        [zone] => {
            let locals: Vec<Point3> = players.iter().filter(|w| zone.contains(w)).map(|w| w.local_position).collect();
            let base = zone.bounds.clone().unwrap_or_else(Bounds::unbounded);
            let (b1, b2) = split_bounds(&base, &locals);
            let keep = Zone { id: zone.id.clone(), world: zone.world.clone(), bounds: Some(b1) };
            let give = zone.with_new_bounds(b2);
            let keep_players = locals.iter().filter(|p| keep.bounds.as_ref().unwrap().contains(p)).count();
            Some(SplitPlan { keep_players, give_players: locals.len() - keep_players, keep: vec![keep], give: vec![give] })
        }
        _ => {
            let mut counted: Vec<(usize, &Zone)> = zones
                .iter()
                .map(|z| (players.iter().filter(|w| z.contains(w)).count(), z))
                .collect();
            // Heaviest first; stable so equal zones keep their config order.
            counted.sort_by(|a, b| b.0.cmp(&a.0));
            let (mut keep, mut give) = (Vec::new(), Vec::new());
            let (mut keep_players, mut give_players) = (0usize, 0usize);
            for (count, zone) in counted {
                if give.is_empty() || give_players + count <= keep_players {
                    give.push(zone.clone());
                    give_players += count;
                } else {
                    keep.push(zone.clone());
                    keep_players += count;
                }
            }
            if keep.is_empty() {
                // Parent always keeps at least one zone: take the lightest back.
                let zone = give.pop().expect("give has at least two zones here");
                let count = players.iter().filter(|w| zone.contains(w)).count();
                give_players -= count;
                keep_players += count;
                keep.push(zone);
            }
            Some(SplitPlan { keep, give, keep_players, give_players })
        }
    }
}

/// How one server's zones are divided between it and one or more children:
/// `keep` stays on the parent, each entry of `gives` goes to its own child (with
/// the number of players it holds), in the order of the children.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitPlanN {
    pub keep: Vec<Zone>,
    pub keep_players: usize,
    pub gives: Vec<(Vec<Zone>, usize)>,
}

impl From<SplitPlan> for SplitPlanN {
    fn from(plan: SplitPlan) -> Self {
        SplitPlanN { keep: plan.keep, keep_players: plan.keep_players, gives: vec![(plan.give, plan.give_players)] }
    }
}

impl SplitPlanN {
    pub fn give_players(&self) -> usize {
        self.gives.iter().map(|(_, n)| n).sum()
    }
}

/// Divides `zones` between a parent and up to `children` new servers.
///
/// When the parent holds several zones and one of them carries more than half of
/// its players (the first server: space + the planet where everybody spawns),
/// moving that zone whole would only move the overload to the child. The zone is
/// cut through its population instead (`split_bounds_n`): with two children or
/// more, each piece goes to one of them and the parent keeps the other zones; with
/// one child the parent also keeps the first piece. A parent holding a single zone
/// with two children or more keeps one piece of it and gives one to each child.
/// Anything else is `plan_split`.
pub fn plan_split_n(zones: &[Zone], players: &[ObjectWorld], children: usize) -> Option<SplitPlanN> {
    if children == 0 {
        return None;
    }
    let total = players.iter().filter(|w| zones.iter().any(|z| z.contains(w))).count();
    let dominant = match zones.len() {
        0 => None,
        1 if children >= 2 => Some((0, total)),
        1 => None,
        _ => zones
            .iter()
            .enumerate()
            .map(|(i, z)| (i, players.iter().filter(|w| z.contains(w)).count()))
            .max_by_key(|&(_, count)| count)
            .filter(|&(_, count)| count >= 2 && count * 2 > total),
    };
    let Some((index, _)) = dominant else {
        return plan_split(zones, players).map(SplitPlanN::from);
    };
    let zone = &zones[index];
    let mut keep: Vec<Zone> = zones.iter().enumerate().filter(|&(i, _)| i != index).map(|(_, z)| z.clone()).collect();
    let count_in = |zs: &[Zone]| players.iter().filter(|w| zs.iter().any(|z| z.contains(w))).count();
    let locals: Vec<Point3> = players.iter().filter(|w| zone.contains(w)).map(|w| w.local_position).collect();
    let base = zone.bounds.clone().unwrap_or_else(Bounds::unbounded);
    let parent_keeps_a_piece = keep.is_empty() || children == 1;
    let pieces = if parent_keeps_a_piece { children + 1 } else { children };
    let mut cut = split_bounds_n(&base, &locals, pieces).into_iter();
    if parent_keeps_a_piece {
        // The parent keeps the zone's identity on the first piece, like `plan_split`.
        keep.push(Zone { id: zone.id.clone(), world: zone.world.clone(), bounds: cut.next() });
    }
    let gives: Vec<(Vec<Zone>, usize)> = cut
        .map(|bounds| {
            let give = vec![zone.with_new_bounds(bounds)];
            let n = count_in(&give);
            (give, n)
        })
        .collect();
    Some(SplitPlanN { keep_players: count_in(&keep), keep, gives })
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// A gap (m) at least this wide in the middle of the population is preferred to
/// the plain median: players standing right on the cut bounce between the two
/// servers at every step. When the crowd has no such gap the median is used
/// anyway — an overloaded server must be split, ping-pong or not.
pub const MIN_CUT_GAP: f64 = 20.0;

/// Cuts a box in two along the axis where the players are the most spread: in the
/// widest gap of the middle half of the population when one is at least
/// `MIN_CUT_GAP` wide, else at the median. With at most one player the box is
/// simply halved.
pub fn split_bounds(bounds: &Bounds, positions: &[Point3]) -> (Bounds, Bounds) {
    let (axis, mut values, lo, hi) = widest_axis(bounds, positions);

    let mid = if values.len() <= 1 {
        round3((lo + hi) / 2.0)
    } else {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Candidate cuts: between consecutive players in the middle half of the
        // sorted population, so both sides keep a fair share. Take the widest gap.
        let n = values.len();
        let first = (n / 4).min(n - 2);
        let last = ((3 * n) / 4).clamp(first + 1, n - 1);
        let (mut best_gap, mut best_mid) = (0.0, 0.0);
        for i in first..last {
            let gap = values[i + 1] - values[i];
            if gap > best_gap {
                best_gap = gap;
                best_mid = (values[i] + values[i + 1]) / 2.0;
            }
        }
        if best_gap >= MIN_CUT_GAP {
            round3(best_mid)
        } else {
            let m = n / 2;
            round3((values[m - 1] + values[m]) / 2.0)
        }
    };

    (with_axis_range(bounds, axis, lo, mid), with_axis_range(bounds, axis, mid + 0.001, hi))
}

/// The axis (0 = x, 1 = y, 2 = z) along which the players are the most spread, their
/// coordinates on it, and the range of `bounds` on it.
fn widest_axis(bounds: &Bounds, positions: &[Point3]) -> (usize, Vec<f64>, f64, f64) {
    let axis_values = |axis: usize| -> Vec<f64> {
        positions.iter().map(|p| match axis { 0 => p.x, 1 => p.y, _ => p.z }).collect()
    };
    let spread = |values: &[f64]| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        (max - min).abs()
    };
    let (xs, ys, zs) = (axis_values(0), axis_values(1), axis_values(2));
    let (sx, sy, sz) = (spread(&xs), spread(&ys), spread(&zs));
    if sx >= sy && sx >= sz {
        (0, xs, bounds.min_x, bounds.max_x)
    } else if sy >= sx && sy >= sz {
        (1, ys, bounds.min_y, bounds.max_y)
    } else {
        (2, zs, bounds.min_z, bounds.max_z)
    }
}

/// `bounds` with its range on `axis` replaced by `lo..hi`.
fn with_axis_range(bounds: &Bounds, axis: usize, lo: f64, hi: f64) -> Bounds {
    let mut b = bounds.clone();
    match axis {
        0 => { b.min_x = lo; b.max_x = hi; }
        1 => { b.min_y = lo; b.max_y = hi; }
        _ => { b.min_z = lo; b.max_z = hi; }
    }
    b
}

/// Cuts a box into `pieces` boxes of about as many players each, along the axis where
/// the players are the most spread: a cut between the players at every 1/`pieces` of
/// the population. Two pieces: `split_bounds` (it prefers a wide gap to the median).
/// Players stacked on the same coordinate can leave fewer pieces than asked, two at
/// least.
pub fn split_bounds_n(bounds: &Bounds, positions: &[Point3], pieces: usize) -> Vec<Bounds> {
    if pieces <= 1 {
        return vec![bounds.clone()];
    }
    if pieces == 2 || positions.len() < 3 {
        let (first, second) = split_bounds(bounds, positions);
        return vec![first, second];
    }
    let (axis, mut values, lo, hi) = widest_axis(bounds, positions);
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let mut cuts: Vec<f64> = Vec::new();
    for i in 1..pieces {
        let k = (i * n / pieces).clamp(1, n - 1);
        let cut = round3((values[k - 1] + values[k]) / 2.0);
        if cut > lo && cut < hi && cuts.last().map_or(true, |last| cut > last + 0.001) {
            cuts.push(cut);
        }
    }
    if cuts.is_empty() {
        let (first, second) = split_bounds(bounds, positions);
        return vec![first, second];
    }
    let mut boxes = Vec::with_capacity(cuts.len() + 1);
    let mut start = lo;
    for cut in cuts {
        boxes.push(with_axis_range(bounds, axis, start, cut));
        start = cut + 0.001;
    }
    boxes.push(with_axis_range(bounds, axis, start, hi));
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_common::world::{WorldChainEntry, WorldKind};

    fn on_planet(uuid: &str, x: f64, y: f64, z: f64) -> ObjectWorld {
        ObjectWorld {
            chain: vec![WorldChainEntry { uuid: uuid.into(), object_type: "planet".into() }],
            world: WorldKind::Planet,
            planet_uuid: Some(uuid.into()),
            planet_name: Some(uuid.into()),
            local_position: Point3::new(x, y, z),
        }
    }

    fn in_space(x: f64, y: f64, z: f64) -> ObjectWorld {
        ObjectWorld::space_at(Point3::new(x, y, z))
    }

    #[test]
    fn rule_parsing() {
        assert_eq!(Rule::parse("tps:20", Joiner::Or), Ok(Rule::tps(20)));
        assert_eq!(Rule::parse("players:50", Joiner::Or), Ok(Rule::players(50)));
        assert_eq!(Rule::parse("fps:15", Joiner::Or), Ok(Rule::tps(15)));
        assert!(Rule::parse("cpu:3", Joiner::Or).is_err());
        assert!(Rule::parse("tps", Joiner::Or).is_err());
        assert!(Rule::parse("", Joiner::Or).is_err());
        assert!(Rule::tps(20).split_hit(19, 0) && !Rule::tps(20).split_hit(20, 0));
        assert!(Rule::players(3).split_hit(60, 4) && !Rule::players(3).split_hit(60, 3));
        assert!(Rule::players(10).merge_hit((60, 4), (60, 5)) && !Rule::players(10).merge_hit((60, 5), (60, 5)));
        assert!(Rule::tps(50).merge_hit((55, 0), (51, 0)) && !Rule::tps(50).merge_hit((55, 0), (50, 0)));
    }

    #[test]
    fn combined_rules() {
        let split = Rule::parse("players:80 or tps: 45", Joiner::Or).unwrap();
        assert_eq!(split, Rule { tps: Some(45), players: Some(80) });
        assert_eq!(split.to_string(), "players:80, tps:45");
        // 2026-10-20 preprod: 72 players at 30 tps never went over players:80.
        assert!(split.split_hit(30, 72));
        assert!(split.split_hit(60, 81));
        assert!(!split.split_hit(45, 80));
        let merge = Rule::parse("players:25 and tps:55", Joiner::And).unwrap();
        assert!(merge.merge_hit((60, 10), (58, 10)));
        assert!(!merge.merge_hit((60, 10), (50, 10)));
        assert!(!merge.merge_hit((60, 20), (60, 10)));
        // The joiner says how the conditions combine: the wrong one is refused.
        assert!(Rule::parse("players:80 and tps:45", Joiner::Or).is_err());
        assert!(Rule::parse("players:25 or tps:55", Joiner::And).is_err());
        assert!(Rule::parse("tps:20 or tps:30", Joiner::Or).is_err());
        assert!(Rule::parse("players:80 or", Joiner::Or).is_err());
    }

    #[test]
    fn split_bounds_cuts_in_the_widest_gap_on_widest_axis() {
        let b = Bounds::unbounded();
        // two groups 100 m apart on X: the cut lands between them, at 50
        let players = [Point3::new(-10.0, 0.0, 0.0), Point3::new(0.0, 1.0, 0.0), Point3::new(100.0, 2.0, 0.0), Point3::new(110.0, 0.0, 0.0)];
        let (b1, b2) = split_bounds(&b, &players);
        assert_eq!(b1.max_x, 50.0);
        assert_eq!(b2.min_x, 50.001);
        assert_eq!(b1.min_y, b.min_y);
        let (c1, c2) = split_bounds(&b, &[Point3::new(0.0, -40.0, 0.0), Point3::new(0.0, 60.0, 1.0)]);
        assert_eq!(c1.max_y, 10.0);
        assert_eq!(c2.min_y, 10.001);
    }

    #[test]
    fn split_bounds_cuts_a_cluster_at_its_median() {
        let b = Bounds::unbounded();
        // 52 players within 26 m on X: no 20 m gap, so the median (between #25 and #26)
        let crowd: Vec<Point3> = (0..52).map(|i| Point3::new(i as f64 * 0.5, 0.0, -3675750.0 + (i % 7) as f64)).collect();
        let (b1, b2) = split_bounds(&b, &crowd);
        assert_eq!(b1.max_x, 12.75);
        assert_eq!(b2.min_x, 12.751);
        let zones = vec![Zone::planet("p", "x")];
        let worlds: Vec<ObjectWorld> = crowd.iter().map(|p| on_planet("p", p.x, p.y, p.z)).collect();
        let plan = plan_split(&zones, &worlds).unwrap();
        assert_eq!((plan.keep_players, plan.give_players), (26, 26));
    }

    #[test]
    fn split_bounds_falls_back_to_middle() {
        let b = Bounds { min_x: 0.0, max_x: 100.0, min_y: 0.0, max_y: 10.0, min_z: 0.0, max_z: 10.0 };
        let (b1, _) = split_bounds(&b, &[]);
        assert_eq!(b1.max_x, 50.0);
        let (b1, _) = split_bounds(&b, &[Point3::new(90.0, 0.0, 0.0)]);
        assert_eq!(b1.max_x, 50.0);
    }

    #[test]
    fn single_zone_is_cut() {
        let zones = vec![Zone::planet("p", "tarsis_3")];
        let players = [on_planet("p", -100.0, 0.0, 0.0), on_planet("p", 100.0, 0.0, 0.0), in_space(5.0, 5.0, 5.0)];
        // the cut is halfway between the two players on the planet: x = 0
        let plan = plan_split(&zones, &players).unwrap();
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.give.len(), 1);
        assert_eq!(plan.keep[0].id, zones[0].id);
        assert_ne!(plan.give[0].id, zones[0].id);
        assert_eq!(plan.keep[0].bounds.as_ref().unwrap().max_x, 0.0);
        assert_eq!((plan.keep_players, plan.give_players), (1, 1));
        assert!(plan.keep[0].contains(&players[0]) && plan.give[0].contains(&players[1]));
    }

    #[test]
    fn several_zones_move_whole_and_balance() {
        let zones = vec![Zone::space(), Zone::planet("a", "a"), Zone::planet("b", "b"), Zone::planet("c", "c")];
        let mut players = Vec::new();
        for _ in 0..5 { players.push(on_planet("a", 0.0, 0.0, 0.0)); }
        for _ in 0..3 { players.push(on_planet("b", 0.0, 0.0, 0.0)); }
        for _ in 0..2 { players.push(in_space(0.0, 0.0, 0.0)); }
        let plan = plan_split(&zones, &players).unwrap();
        // heaviest (a:5) goes to the child, b:3 and space:2 to the parent, c:0 to the child
        assert_eq!(plan.give_players, 5);
        assert_eq!(plan.keep_players, 5);
        assert_eq!(plan.keep.len() + plan.give.len(), 4);
        assert!(plan.give.iter().any(|z| z.planet_uuid() == Some("a")));
        assert!(plan.keep.iter().any(|z| z.is_space()));
        assert!(plan.give.iter().all(|z| z.bounds.is_none()));
    }

    fn crowd_on(planet: &str, n: usize) -> Vec<ObjectWorld> {
        (0..n).map(|i| on_planet(planet, i as f64 * 0.5, 0.0, (i % 7) as f64)).collect()
    }

    #[test]
    fn dominant_zone_is_cut_between_two_children() {
        let zones = vec![Zone::space(), Zone::planet("a", "a"), Zone::planet("b", "b")];
        let mut players = crowd_on("a", 80);
        players.push(in_space(0.0, 0.0, 0.0));
        players.push(on_planet("b", 0.0, 0.0, 0.0));
        let plan = plan_split_n(&zones, &players, 2).unwrap();
        // the parent keeps space and b whole, each child gets half of a
        assert_eq!(plan.keep.len(), 2);
        assert!(plan.keep.iter().all(|z| z.planet_uuid() != Some("a")));
        assert_eq!(plan.keep_players, 2);
        assert_eq!(plan.gives.len(), 2);
        assert_eq!((plan.gives[0].1, plan.gives[1].1), (40, 40));
        for (give, _) in &plan.gives {
            assert_eq!(give.len(), 1);
            assert_eq!(give[0].planet_uuid(), Some("a"));
            assert!(give[0].bounds.is_some());
            assert!(zones.iter().all(|z| z.id != give[0].id));
        }
        assert_ne!(plan.gives[0].0[0].id, plan.gives[1].0[0].id);
    }

    #[test]
    fn dominant_zone_is_cut_in_place_with_one_child() {
        let zones = vec![Zone::space(), Zone::planet("a", "a")];
        let plan = plan_split_n(&zones, &crowd_on("a", 80), 1).unwrap();
        assert_eq!(plan.gives.len(), 1);
        assert_eq!((plan.keep_players, plan.gives[0].1), (40, 40));
        // the parent keeps space and the first half, under the zone's own id
        assert_eq!(plan.keep.len(), 2);
        assert!(plan.keep.iter().any(|z| z.id == zones[1].id && z.bounds.is_some()));
        assert_ne!(plan.gives[0].0[0].id, zones[1].id);
    }

    #[test]
    fn no_dominant_zone_moves_whole_zones() {
        let zones = vec![Zone::space(), Zone::planet("a", "a"), Zone::planet("b", "b")];
        let mut players = crowd_on("a", 5);
        players.extend(crowd_on("b", 5));
        let plan = plan_split_n(&zones, &players, 2).unwrap();
        assert_eq!(plan.gives.len(), 1);
        assert!(plan.gives[0].0.iter().all(|z| z.bounds.is_none()));
        assert_eq!((plan.keep_players, plan.give_players()), (5, 5));
        // a single zone with one child is still halved between the parent and it
        let single = vec![Zone::planet("a", "a")];
        let plan = plan_split_n(&single, &crowd_on("a", 10), 1).unwrap();
        assert_eq!(plan.gives.len(), 1);
        assert_eq!(plan.keep[0].id, single[0].id);
        assert!(plan_split_n(&zones, &players, 0).is_none());
    }

    #[test]
    fn a_crowded_single_zone_is_cut_in_one_piece_per_server() {
        // preprod 2026-10-20: 238 players on a server holding one zone of a planet
        let single = vec![Zone::planet("a", "a")];
        let plan = plan_split_n(&single, &crowd_on("a", 238), 2).unwrap();
        assert_eq!(plan.gives.len(), 2);
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.keep[0].id, single[0].id, "the parent keeps the zone's id on its piece");
        let counts = [plan.keep_players, plan.gives[0].1, plan.gives[1].1];
        assert_eq!(counts.iter().sum::<usize>(), 238);
        assert!(counts.iter().all(|&n| (78..=81).contains(&n)), "{:?}", counts);
        assert_ne!(plan.gives[0].0[0].id, plan.gives[1].0[0].id);
        // the pieces do not overlap
        let b: Vec<Bounds> = std::iter::once(&plan.keep[0]).chain(plan.gives.iter().map(|(z, _)| &z[0])).map(|z| z.bounds.clone().unwrap()).collect();
        assert!(b[0].max_x < b[1].min_x && b[1].max_x < b[2].min_x);
    }

    #[test]
    fn a_dominant_zone_is_cut_in_one_piece_per_child() {
        let zones = vec![Zone::space(), Zone::planet("a", "a")];
        let mut players = crowd_on("a", 240);
        players.push(in_space(0.0, 0.0, 0.0));
        let plan = plan_split_n(&zones, &players, 3).unwrap();
        assert_eq!(plan.keep.len(), 1, "the parent keeps space");
        assert_eq!(plan.keep_players, 1);
        assert_eq!(plan.gives.iter().map(|(_, n)| *n).collect::<Vec<_>>(), [80, 80, 80]);
    }

    #[test]
    fn split_bounds_n_keeps_two_pieces_at_least_with_stacked_players() {
        let base = Bounds::unbounded();
        let stacked = vec![Point3::new(5.0, 0.0, 0.0); 10];
        assert_eq!(split_bounds_n(&base, &stacked, 4).len(), 2);
        assert_eq!(split_bounds_n(&base, &stacked, 1), vec![base.clone()]);
    }

    #[test]
    fn split_children_follow_the_overload() {
        let mut rules = MeshRules {
            split: Rule::parse("players:80 or tps:45", Joiner::Or).unwrap(),
            merge: Rule::players(25),
            split_after: 10,
            merge_after: 15,
            snapshot_timeout: Duration::from_secs(5),
            warmup: Duration::from_secs(5),
            warmup_max: Duration::from_secs(30),
            ready_tps: 58,
            placement_capacity: Some(95),
            placement_min_tps: 45,
        };
        // one zone: the parent keeps a piece
        assert_eq!(rules.split_children_wanted(1, 81), 1, "just over: halved");
        assert_eq!(rules.split_children_wanted(1, 50), 1, "split on tps: halved");
        assert_eq!(rules.split_children_wanted(1, 238), 2, "238 = 3 pieces of 79");
        assert_eq!(rules.split_children_wanted(1, 2000), MAX_SPLIT_CHILDREN);
        // several zones: the pieces of the cut zone all go to children
        assert_eq!(rules.split_children_wanted(3, 81), 2);
        assert_eq!(rules.split_children_wanted(3, 238), 3);
        // a tps rule alone: a plain split
        rules.split = Rule::tps(20);
        assert_eq!(rules.split_children_wanted(1, 238), 1);
    }

    #[test]
    fn placement_capacity_defaults_to_the_players_rule() {
        let mut rules = MeshRules {
            split: Rule::players(60),
            merge: Rule::players(25),
            split_after: 10,
            merge_after: 15,
            snapshot_timeout: Duration::from_secs(5),
            warmup: Duration::from_secs(5),
            warmup_max: Duration::from_secs(30),
            ready_tps: 58,
            placement_capacity: Some(75),
            placement_min_tps: 0,
        };
        assert!(!rules.placement_full(60, 75) && rules.placement_full(60, 76));
        rules.placement_capacity = None;
        assert!(rules.placement_full(60, 61) && !rules.placement_full(60, 60));
        rules.split = Rule::tps(20);
        rules.placement_capacity = Some(75);
        assert!(rules.placement_full(10, 3) && !rules.placement_full(30, 3));
    }

    #[test]
    fn a_server_under_the_placement_min_tps_is_full() {
        let rules = MeshRules {
            split: Rule::players(60),
            merge: Rule::players(25),
            split_after: 10,
            merge_after: 15,
            snapshot_timeout: Duration::from_secs(5),
            warmup: Duration::from_secs(5),
            warmup_max: Duration::from_secs(30),
            ready_tps: 58,
            placement_capacity: Some(75),
            placement_min_tps: 45,
        };
        // Far under the capacity, but too slow to take more players.
        assert!(rules.placement_full(44, 10));
        assert!(!rules.placement_full(45, 10));
    }

    #[test]
    fn parent_keeps_at_least_one_zone() {
        let zones = vec![Zone::space(), Zone::planet("a", "a")];
        let plan = plan_split(&zones, &[]).unwrap();
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.give.len(), 1);
        assert!(plan_split(&[], &[]).is_none());
    }
}

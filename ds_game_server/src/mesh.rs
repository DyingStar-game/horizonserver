//! Pure decision logic of the dynamic server meshing: split/merge rules read from
//! `plugins.toml`, and how a server's zones are divided between two servers.
//! No I/O here so everything is unit-testable.

use ds_common::config::Config;
use ds_common::world::{ObjectWorld, Point3};
use ds_common::zone::{Bounds, Zone};
use std::time::Duration;
use tracing::warn;

/// A threshold on one server metric, `"tps:20"` or `"players:50"` in the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rule {
    Tps(u8),
    Players(u16),
}

impl Rule {
    pub fn parse(s: &str) -> Result<Rule, String> {
        let (kind, value) = s.trim().split_once(':').ok_or_else(|| format!("rule `{}` must be `tps:N` or `players:N`", s))?;
        match kind.trim() {
            "tps" => value.trim().parse::<u8>().map(Rule::Tps).map_err(|e| format!("bad tps in `{}`: {}", s, e)),
            "fps" => {
                warn!("rule `{}` uses the deprecated `fps:` prefix, read it as `tps:`", s);
                value.trim().parse::<u8>().map(Rule::Tps).map_err(|e| format!("bad tps in `{}`: {}", s, e))
            }
            "players" => value.trim().parse::<u16>().map(Rule::Players).map_err(|e| format!("bad players in `{}`: {}", s, e)),
            other => Err(format!("unknown rule kind `{}` in `{}`", other, s)),
        }
    }

    /// Split when the tps falls under the threshold / the players exceed it.
    pub fn split_hit(&self, tps: u8, players: u16) -> bool {
        match *self {
            Rule::Tps(n) => tps < n,
            Rule::Players(n) => players > n,
        }
    }

    /// Merge two siblings when both run above the tps threshold / their players sum
    /// stays under it.
    pub fn merge_hit(&self, a: (u8, u16), b: (u8, u16)) -> bool {
        match *self {
            Rule::Tps(n) => a.0 > n && b.0 > n,
            Rule::Players(n) => (a.1 as u32 + b.1 as u32) < n as u32,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeshRules {
    pub split: Rule,
    pub merge: Rule,
    /// Consecutive 1 s samples the split rule must hold before acting.
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
}

impl MeshRules {
    pub fn from_config(config: &Config) -> MeshRules {
        let rule = |key: &str, default: &str| -> Rule {
            let raw = config.get_value(key).and_then(|v| v.as_str()).unwrap_or(default);
            Rule::parse(raw).unwrap_or_else(|e| {
                warn!("[mesh] {}: {} — using default `{}`", key, e, default);
                Rule::parse(default).expect("default rule parses")
            })
        };
        let int = |key: &str, default: i64| -> i64 {
            config.get_value(key).and_then(|v| v.as_integer()).unwrap_or(default).max(1)
        };
        MeshRules {
            split: rule("split_rule", "tps:20"),
            merge: rule("merge_rule", "players:10"),
            split_after: int("split_after_samples", 10) as u32,
            merge_after: int("merge_after_samples", 30) as u32,
            snapshot_timeout: Duration::from_secs(int("snapshot_timeout_secs", 5) as u64),
            warmup: Duration::from_secs(int("split_warmup_secs", 5) as u64),
            warmup_max: Duration::from_secs(int("split_warmup_max_secs", 30) as u64),
            ready_tps: int("split_ready_tps", 58).clamp(1, 255) as u8,
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
    let (axis, mut values, lo, hi) = if sx >= sy && sx >= sz {
        (0, xs, bounds.min_x, bounds.max_x)
    } else if sy >= sx && sy >= sz {
        (1, ys, bounds.min_y, bounds.max_y)
    } else {
        (2, zs, bounds.min_z, bounds.max_z)
    };

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

    let (mut first, mut second) = (bounds.clone(), bounds.clone());
    match axis {
        0 => { first.max_x = mid; second.min_x = mid + 0.001; }
        1 => { first.max_y = mid; second.min_y = mid + 0.001; }
        _ => { first.max_z = mid; second.min_z = mid + 0.001; }
    }
    (first, second)
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
        assert_eq!(Rule::parse("tps:20"), Ok(Rule::Tps(20)));
        assert_eq!(Rule::parse("players:50"), Ok(Rule::Players(50)));
        assert_eq!(Rule::parse("fps:15"), Ok(Rule::Tps(15)));
        assert!(Rule::parse("cpu:3").is_err());
        assert!(Rule::parse("tps").is_err());
        assert!(Rule::Tps(20).split_hit(19, 0) && !Rule::Tps(20).split_hit(20, 0));
        assert!(Rule::Players(3).split_hit(60, 4) && !Rule::Players(3).split_hit(60, 3));
        assert!(Rule::Players(10).merge_hit((60, 4), (60, 5)) && !Rule::Players(10).merge_hit((60, 5), (60, 5)));
        assert!(Rule::Tps(50).merge_hit((55, 0), (51, 0)) && !Rule::Tps(50).merge_hit((55, 0), (50, 0)));
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

    #[test]
    fn parent_keeps_at_least_one_zone() {
        let zones = vec![Zone::space(), Zone::planet("a", "a")];
        let plan = plan_split(&zones, &[]).unwrap();
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.give.len(), 1);
        assert!(plan_split(&[], &[]).is_none());
    }
}

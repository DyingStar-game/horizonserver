//! A **zone** is what a Godot server owns: a world (space or one planet) plus
//! optional **bounds** (an AABB restricting the zone to part of that world).
//! Bounds are expressed in the world's own coordinates: planet-local for a planet
//! world, universe coordinates for space. `bounds == None` means the whole world.

use crate::world::{ObjectWorld, Point3, WorldKind};
use serde::{Deserialize, Serialize};

/// Axis-aligned box. Inclusive on every edge, like the historical zone check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
    pub min_z: f64,
    pub max_z: f64,
}

impl Bounds {
    /// Half-extent of the "whole universe" box used before zones existed.
    pub const UNBOUNDED: f64 = 900_000_000_000.0;

    pub fn unbounded() -> Self {
        Bounds {
            min_x: -Self::UNBOUNDED,
            max_x: Self::UNBOUNDED,
            min_y: -Self::UNBOUNDED,
            max_y: Self::UNBOUNDED,
            min_z: -Self::UNBOUNDED,
            max_z: Self::UNBOUNDED,
        }
    }

    pub fn contains(&self, p: &Point3) -> bool {
        self.contains_with_margin(p, 0.0)
    }

    /// `margin` grows the box on every side (Godot uses 0.4 m to absorb physics jitter).
    pub fn contains_with_margin(&self, p: &Point3, margin: f64) -> bool {
        p.x >= self.min_x - margin && p.x <= self.max_x + margin
            && p.y >= self.min_y - margin && p.y <= self.max_y + margin
            && p.z >= self.min_z - margin && p.z <= self.max_z + margin
    }

    /// True when `p` is inside the box AND at least `margin` away from every finite
    /// edge (edges at the unbounded extent never count as near).
    pub fn contains_inner(&self, p: &Point3, margin: f64) -> bool {
        let far = |edge: f64| edge.abs() >= Self::UNBOUNDED;
        let ok = |v: f64, lo: f64, hi: f64| (far(lo) || v >= lo + margin) && (far(hi) || v <= hi - margin);
        self.contains(p) && ok(p.x, self.min_x, self.max_x) && ok(p.y, self.min_y, self.max_y) && ok(p.z, self.min_z, self.max_z)
    }

    /// Pulls a point at least `margin` inside the box, so a freshly transferred
    /// object is not pushed back across the edge by the first physics frames.
    pub fn clamp_inside(&self, p: &Point3, margin: f64) -> Point3 {
        Point3 {
            x: p.x.max(self.min_x + margin).min(self.max_x - margin),
            y: p.y.max(self.min_y + margin).min(self.max_y - margin),
            z: p.z.max(self.min_z + margin).min(self.max_z - margin),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "world", rename_all = "lowercase")]
pub enum ZoneWorld {
    Space,
    Planet { planet_uuid: String, planet_name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Zone {
    /// Identity of the zone, kept when its bounds are cut by a split.
    pub id: String,
    #[serde(flatten)]
    pub world: ZoneWorld,
    pub bounds: Option<Bounds>,
}

impl Zone {
    pub fn space() -> Self {
        Zone { id: uuid::Uuid::new_v4().to_string(), world: ZoneWorld::Space, bounds: None }
    }

    pub fn planet(planet_uuid: &str, planet_name: &str) -> Self {
        Zone {
            id: uuid::Uuid::new_v4().to_string(),
            world: ZoneWorld::Planet {
                planet_uuid: planet_uuid.to_string(),
                planet_name: planet_name.to_string(),
            },
            bounds: None,
        }
    }

    /// Same world, new identity, given bounds — the second half of a cut.
    pub fn with_new_bounds(&self, bounds: Bounds) -> Self {
        Zone { id: uuid::Uuid::new_v4().to_string(), world: self.world.clone(), bounds: Some(bounds) }
    }

    pub fn is_space(&self) -> bool {
        matches!(self.world, ZoneWorld::Space)
    }

    pub fn planet_uuid(&self) -> Option<&str> {
        match &self.world {
            ZoneWorld::Planet { planet_uuid, .. } => Some(planet_uuid),
            ZoneWorld::Space => None,
        }
    }

    pub fn matches_world(&self, w: &ObjectWorld) -> bool {
        match (&self.world, w.world) {
            (ZoneWorld::Space, WorldKind::Space) => true,
            (ZoneWorld::Planet { planet_uuid, .. }, WorldKind::Planet) => {
                w.planet_uuid.as_deref() == Some(planet_uuid.as_str())
            }
            _ => false,
        }
    }

    pub fn contains(&self, w: &ObjectWorld) -> bool {
        self.matches_world(w)
            && self.bounds.as_ref().map_or(true, |b| b.contains(&w.local_position))
    }

    /// Short human label for logs: `space`, `space[x -9e11..1234]`, `planet:tarsis_3`.
    pub fn label(&self) -> String {
        let world = match &self.world {
            ZoneWorld::Space => "space".to_string(),
            ZoneWorld::Planet { planet_name, .. } => format!("planet:{}", planet_name),
        };
        match &self.bounds {
            None => world,
            Some(b) => format!(
                "{}[x {:.0}..{:.0} y {:.0}..{:.0} z {:.0}..{:.0}]",
                world, b.min_x, b.max_x, b.min_y, b.max_y, b.min_z, b.max_z
            ),
        }
    }
}

pub fn zones_contain(zones: &[Zone], w: &ObjectWorld) -> bool {
    zones.iter().any(|z| z.contains(w))
}

pub fn zone_containing<'a>(zones: &'a [Zone], w: &ObjectWorld) -> Option<&'a Zone> {
    zones.iter().find(|z| z.contains(w))
}

/// Planets and stars ARE worlds: every Godot server needs them as reference frames,
/// so they are never zone content (never frozen, never counted as managed).
pub fn is_world_object(object_type: &str) -> bool {
    matches!(object_type, "planet" | "star")
}

pub fn zones_label(zones: &[Zone]) -> String {
    zones.iter().map(|z| z.label()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::WorldChainEntry;

    fn on_planet(uuid: &str, p: Point3) -> ObjectWorld {
        ObjectWorld {
            chain: vec![WorldChainEntry { uuid: uuid.into(), object_type: "planet".into() }],
            world: WorldKind::Planet,
            planet_uuid: Some(uuid.into()),
            planet_name: Some("x".into()),
            local_position: p,
        }
    }

    fn cube(h: f64) -> Bounds {
        Bounds { min_x: -h, max_x: h, min_y: -h, max_y: h, min_z: -h, max_z: h }
    }

    #[test]
    fn space_zone_only_matches_space_objects() {
        let z = Zone::space();
        assert!(z.contains(&ObjectWorld::space_at(Point3::new(1e9, 0.0, 0.0))));
        assert!(!z.contains(&on_planet("p", Point3::default())));
    }

    #[test]
    fn planet_zone_matches_its_planet_only() {
        let z = Zone::planet("p", "tarsis_3");
        assert!(z.contains(&on_planet("p", Point3::new(1e6, 0.0, 0.0))));
        assert!(!z.contains(&on_planet("other", Point3::default())));
        assert!(!z.contains(&ObjectWorld::space_at(Point3::default())));
    }

    #[test]
    fn bounds_are_inclusive_and_margin_grows_them() {
        let b = cube(10.0);
        assert!(b.contains(&Point3::new(10.0, -10.0, 0.0)));
        assert!(!b.contains(&Point3::new(10.001, 0.0, 0.0)));
        assert!(b.contains_with_margin(&Point3::new(10.3, 0.0, 0.0), 0.4));
        let mut z = Zone::planet("p", "x");
        z.bounds = Some(b);
        assert!(z.contains(&on_planet("p", Point3::new(5.0, 5.0, 5.0))));
        assert!(!z.contains(&on_planet("p", Point3::new(50.0, 5.0, 5.0))));
    }

    #[test]
    fn contains_inner_ignores_unbounded_edges() {
        let mut b = Bounds::unbounded();
        b.max_z = 100.0;
        assert!(b.contains_inner(&Point3::new(0.0, 0.0, -500.0), 300.0));
        assert!(!b.contains_inner(&Point3::new(0.0, 0.0, 50.0), 300.0));
        assert!(!b.contains_inner(&Point3::new(0.0, 0.0, 150.0), 300.0));
    }

    #[test]
    fn clamp_pulls_inside_by_margin() {
        let b = cube(10.0);
        let c = b.clamp_inside(&Point3::new(10.0, -20.0, 3.0), 0.5);
        assert_eq!(c, Point3::new(9.5, -9.5, 3.0));
    }

    #[test]
    fn zone_json_is_flat() {
        let mut z = Zone::planet("p", "tarsis_3");
        z.bounds = Some(cube(1.0));
        let v = serde_json::to_value(&z).unwrap();
        assert_eq!(v["world"], "planet");
        assert_eq!(v["planet_uuid"], "p");
        assert_eq!(v["bounds"]["max_x"], 1.0);
        let s = serde_json::to_value(Zone::space()).unwrap();
        assert_eq!(s["world"], "space");
        assert!(s["bounds"].is_null());
        let back: Zone = serde_json::from_value(v).unwrap();
        assert_eq!(back, z);
    }

    #[test]
    fn helpers() {
        let zones = vec![Zone::space(), Zone::planet("p", "x")];
        assert!(zones_contain(&zones, &on_planet("p", Point3::default())));
        assert!(!zones_contain(&zones, &on_planet("q", Point3::default())));
        assert!(is_world_object("planet") && is_world_object("star") && !is_world_object("player"));
    }
}

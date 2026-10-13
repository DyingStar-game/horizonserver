//! The orientation part of a parent's frame, so a child's global position is
//! `parent position + parent basis × child local position`, as Godot places it.
//!
//! Every global position used to be `parent position + child local position`:
//! right only while every parent stands unrotated. A vehicle lift stands on a
//! planet tilted like the ground under it, and its platform runs 180 m down its
//! own -Y, which is the planet's radial there, not the planet's Y: the platform
//! and what it carried were placed up to 190 m from where they were, and a
//! crate riding it left the zone of the player standing next to it (2026-10-13).
//!
//! A planet is the end of a frame chain and counts as unrotated: its spin is the
//! client's alone (the game server's planet never turns), so the positions of
//! what stands on it are planet-local in the planet's own axes.

use horizon_event_system::{GorcInstanceManager, GorcObjectId, Vec3};
use serde_json::Value;

use crate::genericprops::GenericProps;
use crate::handlers::world::MAX_CHAIN_DEPTH;

/// A rotation, row-major.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Basis {
    pub m: [[f64; 3]; 3],
}

impl Basis {
    pub const IDENTITY: Basis = Basis { m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] };

    /// Godot's `Basis.from_euler(rotation)` in its default order (YXZ), which is
    /// how a `Node3D.rotation` — the replicated `rotation` — turns into a basis.
    pub fn from_euler_yxz(rotation: Vec3) -> Basis {
        let (s1, c1) = rotation.x.sin_cos();
        let (s2, c2) = rotation.y.sin_cos();
        let (s3, c3) = rotation.z.sin_cos();
        let x = Basis { m: [[1.0, 0.0, 0.0], [0.0, c1, -s1], [0.0, s1, c1]] };
        let y = Basis { m: [[c2, 0.0, s2], [0.0, 1.0, 0.0], [-s2, 0.0, c2]] };
        let z = Basis { m: [[c3, -s3, 0.0], [s3, c3, 0.0], [0.0, 0.0, 1.0]] };
        y.mul(&x).mul(&z)
    }

    pub fn mul(&self, o: &Basis) -> Basis {
        let mut m = [[0.0; 3]; 3];
        for (r, row) in m.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|k| self.m[r][k] * o.m[k][c]).sum();
            }
        }
        Basis { m }
    }

    pub fn xform(&self, v: Vec3) -> Vec3 {
        Vec3::new(
            self.m[0][0] * v.x + self.m[0][1] * v.y + self.m[0][2] * v.z,
            self.m[1][0] * v.x + self.m[1][1] * v.y + self.m[1][2] * v.z,
            self.m[2][0] * v.x + self.m[2][1] * v.y + self.m[2][2] * v.z,
        )
    }
}

/// `parent_position + parent_basis × local`.
pub fn compose(parent_position: Vec3, parent_basis: &Basis, local: Vec3) -> Vec3 {
    let r = parent_basis.xform(local);
    Vec3::new(parent_position.x + r.x, parent_position.y + r.y, parent_position.z + r.z)
}

/// The object's own rotation relative to its parent, from the first channel
/// carrying `rotation` (identity when none does).
pub fn local_basis(gp: &GenericProps) -> Basis {
    gp.data
        .values()
        .filter_map(|zone_data| zone_data.get("rotation"))
        .filter_map(|v| serde_json::from_value::<Vec3>(v.clone()).ok())
        .next()
        .map(Basis::from_euler_yxz)
        .unwrap_or(Basis::IDENTITY)
}

/// Same, from a payload (a create, a player move) rather than a stored instance.
pub fn basis_of_payload(data: &Value) -> Basis {
    data.get("rotation")
        .and_then(|v| serde_json::from_value::<Vec3>(v.clone()).ok())
        .map(Basis::from_euler_yxz)
        .unwrap_or(Basis::IDENTITY)
}

/// The orientation of `uuid`'s frame in its planet's axes (or in space): the
/// product of the local rotations from the planet down to `uuid` itself. A
/// planet, or an object that cannot be read, is identity.
pub async fn global_basis_of(gorc: &GorcInstanceManager, uuid: &str) -> Basis {
    let mut acc = Basis::IDENTITY;
    let mut next = Some(uuid.to_string());
    let mut seen: Vec<String> = Vec::new();
    while let Some(id) = next.take() {
        if seen.len() >= MAX_CHAIN_DEPTH || seen.contains(&id) {
            break;
        }
        let Ok(gorc_id) = GorcObjectId::from_str(&id) else {
            break;
        };
        let node = gorc
            .with_object_mut(gorc_id, |instance| {
                instance.get_object::<GenericProps>().map(|gp| {
                    (gp.object_def.name == "planet", local_basis(gp), gp.parent_id())
                })
            })
            .await
            .flatten();
        let Some((is_planet, rotation, parent_id)) = node else {
            break;
        };
        if is_planet {
            break;
        }
        acc = rotation.mul(&acc);
        seen.push(id);
        next = parent_id.filter(|p| !p.is_empty());
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9 && (a.z - b.z).abs() < 1e-9
    }

    #[test]
    fn zero_rotation_is_identity() {
        assert_eq!(Basis::from_euler_yxz(Vec3::new(0.0, 0.0, 0.0)), Basis::IDENTITY);
    }

    #[test]
    fn yaw_turns_minus_z_towards_minus_x() {
        // Godot: rotation.y = +90° turns the forward axis (-Z) to -X.
        let b = Basis::from_euler_yxz(Vec3::new(0.0, std::f64::consts::FRAC_PI_2, 0.0));
        assert!(close(b.xform(Vec3::new(0.0, 0.0, -1.0)), Vec3::new(-1.0, 0.0, 0.0)));
    }

    #[test]
    fn yxz_order_matches_godot() {
        // Basis.from_euler(Vector3(0.3, -0.6, 1.1)) * Vector3(1, 2, 3) in Godot 4.7.
        let b = Basis::from_euler_yxz(Vec3::new(0.3, -0.6, 1.1));
        let r = b.xform(Vec3::new(1.0, 2.0, 3.0));
        let expected = {
            let (s1, c1) = 0.3f64.sin_cos();
            let (s2, c2) = (-0.6f64).sin_cos();
            let (s3, c3) = 1.1f64.sin_cos();
            // z first, then x, then y
            let v = Vec3::new(c3 * 1.0 - s3 * 2.0, s3 * 1.0 + c3 * 2.0, 3.0);
            let v = Vec3::new(v.x, c1 * v.y - s1 * v.z, s1 * v.y + c1 * v.z);
            Vec3::new(c2 * v.x + s2 * v.z, v.y, -s2 * v.x + c2 * v.z)
        };
        assert!(close(r, expected));
    }

    #[test]
    fn lift_platform_runs_down_the_lift_axis() {
        // The tarsis_3 lift: its local -Y is the planet's radial there, not the planet's -Y.
        let lift = Basis::from_euler_yxz(Vec3::new(0.111264, -0.638718, -1.134372));
        let down = compose(Vec3::new(0.0, 0.0, 0.0), &lift, Vec3::new(0.0, -180.0, 0.0));
        let radial = Vec3::new(4450769.948, 2672441.488, 3676826.622);
        let n = (radial.x * radial.x + radial.y * radial.y + radial.z * radial.z).sqrt();
        let along = -(down.x * radial.x + down.y * radial.y + down.z * radial.z) / n;
        assert!((along - 180.0).abs() < 0.1, "descent along the radial: {along}");
    }
}

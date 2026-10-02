//! Which Godot server owns each player — one owner at most.
//!
//! Every server decides alone whether a player belongs to it (its zones contain
//! the player's world). During a mesh transition two servers hold the same zones
//! for up to a minute (a split child starts before its parent shrinks; a merge
//! survivor gets every zone before the released server lets go), so a player
//! arriving then was spawned on BOTH Godot servers and listed in both
//! `managed_players`: 199 of 403 players on preprod (2026-10-01), a universe
//! count of 6372 for ~400 players, and merge decisions made on inflated counts.
//!
//! The registry is the single source of truth: a new player goes to the first
//! server that claims it, a hand-over moves it (and drops it from the previous
//! owner's list), and only the owner can let it go.
//!
//! Lock order: the registry first, then a server's `managed_players`. Never
//! call into the registry while holding a `managed_players` lock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::handlers::initial_objects::ManagedPlayers;

struct Owner {
    server_uuid: String,
    players: ManagedPlayers,
}

fn owners() -> &'static Mutex<HashMap<String, Owner>> {
    static OWNERS: OnceLock<Mutex<HashMap<String, Owner>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// The player is now this server's.
    Taken,
    /// It already was.
    AlreadyMine,
    /// Another server owns it.
    Other(String),
}

fn forget_in(players: &ManagedPlayers, player: &str) {
    let mut list = players.lock().unwrap();
    if let Some(pos) = list.iter().position(|p| p == player) {
        list.remove(pos);
    }
}

/// Claims `player` for `server_uuid` if nobody owns it, or if its owner is
/// `handing_over` (the source of a transfer, which may not have let go yet).
pub fn claim(player: &str, server_uuid: &str, players: &ManagedPlayers, handing_over: Option<&str>) -> Claim {
    let mut owners = owners().lock().unwrap();
    if let Some(owner) = owners.get(player) {
        if owner.server_uuid == server_uuid {
            return Claim::AlreadyMine;
        }
        if handing_over != Some(owner.server_uuid.as_str()) {
            return Claim::Other(owner.server_uuid.clone());
        }
        forget_in(&owner.players, player);
    }
    owners.insert(player.to_string(), Owner { server_uuid: server_uuid.to_string(), players: Arc::clone(players) });
    Claim::Taken
}

/// Makes `server_uuid` the owner whoever owned `player` (a hand-over decided by
/// the ServerManager), drops it from the previous owner's list and adds it to
/// `players`.
pub fn take(player: &str, server_uuid: &str, players: &ManagedPlayers) {
    let mut owners = owners().lock().unwrap();
    if let Some(previous) = owners.get(player) {
        if previous.server_uuid != server_uuid {
            forget_in(&previous.players, player);
        }
    }
    owners.insert(player.to_string(), Owner { server_uuid: server_uuid.to_string(), players: Arc::clone(players) });
    let mut list = players.lock().unwrap();
    if !list.iter().any(|p| p == player) {
        list.push(player.to_string());
    }
}

/// Lets `player` go if `server_uuid` owns it.
pub fn release(player: &str, server_uuid: &str) {
    let mut owners = owners().lock().unwrap();
    if owners.get(player).map_or(false, |o| o.server_uuid == server_uuid) {
        owners.remove(player);
    }
}

/// Lets go of every player `server_uuid` owns (released, or gone offline).
pub fn release_all(server_uuid: &str) {
    owners().lock().unwrap().retain(|_, owner| owner.server_uuid != server_uuid);
}

/// Players owned by a server: each counted once.
pub fn count() -> usize {
    owners().lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> ManagedPlayers {
        Arc::new(Mutex::new(Vec::new()))
    }

    #[test]
    fn first_claim_wins_and_hand_over_moves_the_player() {
        let (a, b) = (list(), list());
        let p = "test-player-ownership-1";
        assert_eq!(claim(p, "srv-a", &a, None), Claim::Taken);
        assert_eq!(claim(p, "srv-b", &b, None), Claim::Other("srv-a".into()));
        assert_eq!(claim(p, "srv-a", &a, None), Claim::AlreadyMine);

        a.lock().unwrap().push(p.into());
        // Transfer from srv-a: srv-b may take it, and srv-a's list forgets it.
        assert_eq!(claim(p, "srv-b", &b, Some("srv-a")), Claim::Taken);
        assert!(a.lock().unwrap().is_empty());

        // Only the owner lets go.
        release(p, "srv-a");
        assert_eq!(claim(p, "srv-c", &list(), None), Claim::Other("srv-b".into()));
        release(p, "srv-b");
        assert_eq!(claim(p, "srv-c", &list(), None), Claim::Taken);
        release_all("srv-c");
    }

    #[test]
    fn take_forces_and_cleans_the_previous_owner() {
        let (a, b) = (list(), list());
        let p = "test-player-ownership-2";
        take(p, "srv-x", &a);
        take(p, "srv-y", &b);
        assert!(a.lock().unwrap().is_empty());
        assert_eq!(*b.lock().unwrap(), vec![p.to_string()]);
        release_all("srv-y");
        assert_eq!(claim(p, "srv-x", &a, None), Claim::Taken);
        release_all("srv-x");
    }
}

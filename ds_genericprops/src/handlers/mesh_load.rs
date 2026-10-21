//! The last pool load published by ds_game_server (`genericprops:servers_load`, every
//! 5 s), plus the players placed since: arrivals come faster than the updates (1.2/s
//! in the 2026-10-02 load test) and would all go to the same server in between.

use ds_common::mesh_load::{PoolLoad, ServerLoad};
use ds_common::world::ObjectWorld;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A server that takes no new player may still be asked for village habs at this tps or
/// more: a village spawns in under a second when the server is idle (preprod, 2026-10-21)
/// but takes longer and costs more on a loaded one, so only a server with headroom, never
/// in a split or merge. Its villages are then ready when its split for players hands them
/// over, instead of being released one split at a time while 760 players waited.
pub const HABS_MIN_TPS: u8 = 55;

/// Older than this, the load is ignored and new players are placed as before.
const MAX_AGE: Duration = Duration::from_secs(30);

struct Cache {
    load: PoolLoad,
    received: Instant,
    /// Players placed per server uuid since `load` was received.
    placed: HashMap<String, u32>,
}

fn cache() -> &'static Mutex<Option<Cache>> {
    static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

pub fn set(load: PoolLoad) {
    *cache().lock().unwrap() = Some(Cache { load, received: Instant::now(), placed: HashMap::new() });
}

/// The current load, when fresh enough to place players on.
pub fn view() -> Option<LoadView> {
    let guard = cache().lock().unwrap();
    let cache = guard.as_ref().filter(|c| c.received.elapsed() <= MAX_AGE)?;
    Some(LoadView { load: cache.load.clone(), placed: cache.placed.clone() })
}

/// A player was just given an apartment in a zone of `server_uuid`.
pub fn note_placed(server_uuid: &str) {
    if let Some(cache) = cache().lock().unwrap().as_mut() {
        *cache.placed.entry(server_uuid.to_string()).or_default() += 1;
    }
}

/// How full a server is, for the placement of new players.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    /// 0 = may take players, 1 = loading or over its split rule, 2 = silent or frozen,
    /// 3 = no running server owns the place.
    pub(crate) refused: u8,
    players: u32,
}

#[derive(Debug, Clone)]
pub struct LoadView {
    load: PoolLoad,
    placed: HashMap<String, u32>,
}

impl LoadView {
    /// Take no new player on this server in this view (it had its share of arrivals for now).
    pub fn refuse(&mut self, server_uuid: &str) {
        if let Some(server) = self.load.servers.iter_mut().find(|s| s.uuid == server_uuid) {
            server.accepting = false;
        }
    }

    pub fn owner_of(&self, world: &ObjectWorld) -> Option<&ServerLoad> {
        self.load.owner_of(world)
    }

    /// Players of the server, counting the ones placed since the last update.
    pub fn players(&self, server: &ServerLoad) -> u32 {
        server.players + self.placed.get(&server.uuid).copied().unwrap_or(0)
    }

    /// No server can give anyone a place, now or soon: none takes players or is loading
    /// zones it was just handed, no split or merge is running or queued, and no idle server
    /// is left for one. Never when the publisher does not tell its idle servers.
    pub fn all_full(&self) -> bool {
        self.load.idle_servers == Some(0)
            && self.load.ops_running == 0
            && !self.load.servers.iter().any(|s| s.loading || self.accepts(s))
    }

    /// A split or merge is running or queued, or one will come: a server refuses players
    /// while an idle server is left to take part of them.
    pub fn split_coming(&self) -> bool {
        self.load.ops_running > 0
            || (self.load.idle_servers.is_some_and(|idle| idle > 0)
                && self.load.servers.iter().any(|s| s.responsive && !self.accepts(s)))
    }

    pub fn accepts(&self, server: &ServerLoad) -> bool {
        server.accepting && self.load.capacity.map_or(true, |cap| self.players(server) < cap)
    }

    /// Rank of the server owning `world`; a place no running server owns comes last.
    pub fn rank(&self, world: Option<&ObjectWorld>) -> Rank {
        match world.and_then(|w| self.owner_of(w)) {
            Some(server) => {
                let refused = if self.accepts(server) { 0 } else if server.responsive { 1 } else { 2 };
                Rank { refused, players: self.players(server) }
            }
            None => Rank { refused: 3, players: u32::MAX },
        }
    }

    /// The server owning `world` is silent or frozen. A player placed there waits for it
    /// in the void: 276 players were sent to one frozen 2+ min (preprod, 2026-10-16).
    pub fn owner_unresponsive(&self, world: Option<&ObjectWorld>) -> bool {
        world.and_then(|w| self.owner_of(w)).map_or(false, |s| !s.responsive)
    }

    /// The server owning `world` may be asked for village habs now: neither frozen nor
    /// loading the zones it was just handed, and taking players while some server does.
    /// The habs went to the most loaded server of the pool (275 players, tps 0-16) once the
    /// accepting ones had no village left to spawn, and its fresh villages drew every arrival
    /// (preprod, 2026-10-16). A place no running server owns may.
    /// A server refusing players with HABS_MIN_TPS or more may too. Never one in a split or
    /// merge, even when no server takes players: during a split nobody does, 6 villages went
    /// to its fresh child and their 469 items to the parent at 95 players, down to 14 tps
    /// (preprod, 2026-10-21).
    pub fn owner_takes_habs(&self, world: Option<&ObjectWorld>) -> bool {
        world.and_then(|w| self.owner_of(w)).map_or(true, |s| {
            s.responsive && !s.loading && !s.busy
                && (self.accepts(s) || !self.any_accepting() || s.tps >= HABS_MIN_TPS)
        })
    }

    /// The server owning `world` takes new players (a place no running server owns does not).
    pub fn owner_accepts(&self, world: Option<&ObjectWorld>) -> bool {
        world.and_then(|w| self.owner_of(w)).is_some_and(|s| self.accepts(s))
    }

    /// The server owning `world` answers but takes no new player (full, too slow, in a
    /// split or merge), and is not loading zones it was just handed.
    pub fn owner_refusing(&self, world: Option<&ObjectWorld>) -> bool {
        world.and_then(|w| self.owner_of(w)).is_some_and(|s| s.responsive && !s.loading && !self.accepts(s))
    }

    /// Some running server may take players.
    pub fn any_accepting(&self) -> bool {
        self.load.servers.iter().any(|s| self.accepts(s))
    }
}

#[cfg(test)]
pub(crate) fn view_of(load: PoolLoad) -> LoadView {
    LoadView { load, placed: HashMap::new() }
}

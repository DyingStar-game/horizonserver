//! The last pool load published by ds_game_server (`genericprops:servers_load`, every
//! 5 s), plus the players placed since: arrivals come faster than the updates (1.2/s
//! in the 2026-10-02 load test) and would all go to the same server in between.

use ds_common::mesh_load::{PoolLoad, ServerLoad};
use ds_common::world::ObjectWorld;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    /// 0 = may take players, 1 = silent, loading or over its split rule.
    refused: u8,
    players: u32,
}

#[derive(Debug, Clone)]
pub struct LoadView {
    load: PoolLoad,
    placed: HashMap<String, u32>,
}

impl LoadView {
    pub fn owner_of(&self, world: &ObjectWorld) -> Option<&ServerLoad> {
        self.load.owner_of(world)
    }

    /// Players of the server, counting the ones placed since the last update.
    pub fn players(&self, server: &ServerLoad) -> u32 {
        server.players + self.placed.get(&server.uuid).copied().unwrap_or(0)
    }

    pub fn accepts(&self, server: &ServerLoad) -> bool {
        server.accepting && self.load.capacity.map_or(true, |cap| self.players(server) < cap)
    }

    /// Rank of the server owning `world`; a place no running server owns comes last.
    pub fn rank(&self, world: Option<&ObjectWorld>) -> Rank {
        match world.and_then(|w| self.owner_of(w)) {
            Some(server) => Rank { refused: u8::from(!self.accepts(server)), players: self.players(server) },
            None => Rank { refused: 2, players: u32::MAX },
        }
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

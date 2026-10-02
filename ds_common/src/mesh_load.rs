//! Load of the Godot server pool, published every few seconds by ds_game_server as
//! `genericprops:servers_load` so new players get an apartment where a server has
//! room, instead of all landing on the one whose zone holds the next village in line
//! (minikube load test, 2026-10-02: 228 players on one server while it was split
//! every 80 s, each split taking one village away).

use crate::world::ObjectWorld;
use crate::zone::{zones_contain, Zone};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerLoad {
    pub uuid: String,
    pub name: String,
    /// Larger of the Godot and Horizon counts (Godot's lags).
    pub players: u32,
    /// Reporting, not loading zones it was just handed, and under its split rule:
    /// it may take new players.
    pub accepting: bool,
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PoolLoad {
    /// Running servers only.
    pub servers: Vec<ServerLoad>,
    /// Players a server may hold before it is split (`players:N` split rule);
    /// `None` under a `tps:` rule.
    pub capacity: Option<u32>,
}

impl PoolLoad {
    /// The server whose zones contain `w`.
    pub fn owner_of(&self, w: &ObjectWorld) -> Option<&ServerLoad> {
        self.servers.iter().find(|s| zones_contain(&s.zones, w))
    }
}

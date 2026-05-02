use std::sync::Arc;
use dashmap::DashMap;
use anyhow::Result;
use super::livekit::LiveKitRoom;
use tracing::info;

/// Tracks which players are in each other's zone.
/// `zones[player_id]` = set of player IDs currently in their zone.
pub struct ProximityManager {
    lk: Arc<LiveKitRoom>,
    /// player_id → set of neighbour player_ids currently in their zone
    zones: DashMap<String, dashmap::DashSet<String>>,
}

impl ProximityManager {
    pub fn new(lk: Arc<LiveKitRoom>) -> Self {
        Self {
            lk,
            zones: DashMap::new(),
        }
    }

    /// Call this when your game logic detects that `other` entered `player`'s zone.
    /// This is bidirectional: both players hear each other.
    pub async fn on_zone_enter(&self, player: &str, other: &str) -> Result<()> {
        // Avoid double-subscribing
        let already_in = self
            .zones
            .entry(player.to_string())
            .or_default()
            .insert(other.to_string()); // returns false if already present

        info!("🔊 DyingstarAudioPlugin - alreadyin: player={} other={} already_in={}", player, other, already_in);

        if !already_in {
            return Ok(()); // idempotent
        }

        // player hears other
        self.lk.subscribe(player, other).await?;

        // other hears player (if not already subscribed the other way)
        let already_reversed = self
            .zones
            .entry(other.to_string())
            .or_default()
            .insert(player.to_string());

        if already_reversed {
            self.lk.subscribe(other, player).await?;
        }

        Ok(())
    }

    /// Call this when `other` exits `player`'s zone.
    pub async fn on_zone_exit(&self, player: &str, other: &str) -> Result<()> {
        // Remove from zone set
        if let Some(neighbours) = self.zones.get(player) {
            neighbours.remove(other);
        }

        self.lk.unsubscribe(player, other).await?;

        // Mirror: remove player from other's zone too
        if let Some(neighbours) = self.zones.get(other) {
            neighbours.remove(player);
        }
        self.lk.unsubscribe(other, player).await?;

        Ok(())
    }

    /// Call when a player disconnects entirely (remove from all zones).
    pub async fn on_player_leave(&self, player: &str) {
        // Collect neighbours before removing
        let neighbours: Vec<String> = self
            .zones
            .get(player)
            .map(|n| n.iter().map(|s| s.clone()).collect())
            .unwrap_or_default();

        self.zones.remove(player);

        // Best-effort cleanup: unsubscribe all neighbours from this player
        for neighbour in neighbours {
            if let Some(n_zone) = self.zones.get(&neighbour) {
                n_zone.remove(player);
            }
            // No LiveKit call needed — LiveKit auto-cleans when a participant leaves
        }
    }
}
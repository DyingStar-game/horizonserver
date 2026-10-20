//! Where each player is, saved to persistence: on disconnect, and every
//! `player_save_interval_secs` while connected (a crash of Horizon loses at most that).
//! Sent as `bridge_persistence:update_object`, which persistence merges into the stored
//! player like any update; genericprops' own `update_object` handler never sees it.
//!
//! The respawn does not use it: a returning player always wakes up in its apartment
//! (see new_player::enqueue_returning).

use std::sync::Arc;
use std::time::Duration;

use dashmap::{DashMap, DashSet};
use futures::executor::block_on;
use horizon_event_system::{EventSystem, GorcInstanceManager, GorcObjectId, PlayerId};
use serde_json::{json, Value};
use tracing::{debug, error, info};

use crate::genericprops::GenericProps;
use crate::handlers::world::read_own_local_and_parent;

/// Position and rotation local to its parent, and the parent, as the last movement left
/// them on the player's GORC instance (see player_movement).
pub async fn state_of(gorc_instances: &GorcInstanceManager, gorc_id: GorcObjectId) -> Option<Value> {
    gorc_instances
        .with_object_mut(gorc_id, |instance| {
            let player = instance.get_object::<GenericProps>()?;
            let (position, parent_id) = read_own_local_and_parent(player);
            let rotation = player.data.values().find_map(|v| v.get("rotation")).cloned();
            let mut state = json!({
                "position": position,
                // Empty when it has none (in space): persistence keeps the stored parent otherwise.
                "parent_id": parent_id.unwrap_or_default(),
            });
            if let Some(rotation) = rotation {
                state["rotation"] = rotation;
            }
            Some(state)
        })
        .await
        .flatten()
}

/// Send the player's current state to persistence.
pub async fn save(events: &EventSystem, gorc_instances: &GorcInstanceManager, player_uuid: &str, gorc_id: GorcObjectId) {
    let Some(state) = state_of(gorc_instances, gorc_id).await else {
        debug!("plugin genericprops (player_state): player {} has no GORC instance, nothing to save", player_uuid);
        return;
    };
    if let Err(e) = events
        .emit_plugin(
            "bridge_persistence",
            "update_object",
            &json!({
                "object_type": "player",
                "object_uuid": player_uuid,
                "object_data": state,
            }),
        )
        .await
    {
        error!("plugin genericprops (player_state): failed to save player {}: {}", player_uuid, e);
    }
}

/// Save every connected player every `interval`, on a thread of its own (a luminal task
/// waiting on a timer is re-polled at once, forever: see new_player::start_placement).
pub fn start_periodic(
    events: Arc<EventSystem>,
    props: Arc<DashMap<String, GorcObjectId>>,
    live_players: Arc<DashSet<PlayerId>>,
    interval: Duration,
) {
    if interval.is_zero() {
        info!("plugin genericprops (player_state): periodic save disabled, players saved on disconnect only");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("genericprops-player-save".to_string())
        .spawn(move || loop {
            std::thread::sleep(interval);
            let Some(gorc_instances) = events.get_gorc_instances() else { continue };
            let players: Vec<(String, GorcObjectId)> = live_players
                .iter()
                .filter_map(|player| {
                    let uuid = player.key().to_string();
                    let gorc_id = *props.get(&uuid)?;
                    Some((uuid, gorc_id))
                })
                .collect();
            for (uuid, gorc_id) in &players {
                block_on(save(&events, &gorc_instances, uuid, *gorc_id));
            }
            debug!("plugin genericprops (player_state): saved {} player(s)", players.len());
        });
    match spawned {
        Ok(_) => info!("plugin genericprops (player_state): players saved every {:?}", interval),
        Err(e) => error!("plugin genericprops (player_state): ❌ failed to start the save thread: {}", e),
    }
}

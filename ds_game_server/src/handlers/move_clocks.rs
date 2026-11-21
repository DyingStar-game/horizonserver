//! The players who asked for the clocks of their own moves (client.ini
//! `debug_move_clocks`, sent in their `player/init`): the Godot server managing
//! one of them stamps its positions with `servergodot_ms`, and Horizon adds
//! `horizon_received_ms` / `horizon_sent_ms` to those only. Opt-in per player,
//! so a measurement does not grow the traffic of every player on the mesh.
//!
//! Kept here from the init to the disconnect, whichever server manages the
//! player: every spawn of the player on a Godot server (first one, or a zone
//! transfer) carries `move_clocks: true` in its `object_data`. Shared by every
//! `Server` instance of the process, like the cached velocities.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use tracing::info;

static WANTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Key of the flag in a player's `object_data`, as the Godot server reads it.
pub const KEY: &str = "move_clocks";

/// Records what `player_uuid` asked for at its init.
pub fn set(player_uuid: &str, enabled: bool) {
    let mut wanted = WANTED.lock().unwrap();
    if enabled {
        if wanted.insert(player_uuid.to_string()) {
            info!("[move_clocks] player {} asked for the clocks of its moves", player_uuid);
        }
    } else {
        wanted.remove(player_uuid);
    }
}

/// Whether `player_uuid` asked for them.
pub fn wanted(player_uuid: &str) -> bool {
    WANTED.lock().unwrap().contains(player_uuid)
}

/// Drops the player who left the game.
pub fn forget(player_uuid: &str) {
    WANTED.lock().unwrap().remove(player_uuid);
}

/// Sets the flag in the `object_data` of a player about to be spawned on a Godot
/// server, when that player asked for it.
pub fn mark(object_data: &mut Value, player_uuid: &str) {
    if wanted(player_uuid) {
        if let Some(map) = object_data.as_object_mut() {
            map.insert(KEY.to_string(), json!(true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_player_who_asked_is_marked_until_it_leaves() {
        let (asked, other) = ("move-clocks-asked", "move-clocks-other");
        set(asked, true);
        let mut data = json!({"name": "ann"});
        mark(&mut data, asked);
        assert_eq!(data[KEY], json!(true));
        let mut data = json!({"name": "bob"});
        mark(&mut data, other);
        assert!(data.get(KEY).is_none());

        forget(asked);
        let mut data = json!({"name": "ann"});
        mark(&mut data, asked);
        assert!(data.get(KEY).is_none(), "gone at the disconnect");

        set(asked, true);
        set(asked, false);
        assert!(!wanted(asked), "a reconnection without the flag clears it");
    }
}

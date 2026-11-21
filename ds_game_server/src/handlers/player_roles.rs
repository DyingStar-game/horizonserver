//! The Keycloak realm roles of each connected player (`realm_access.roles` of
//! the access token ds_player_authentication verified at the init, e.g. `admin`,
//! `supervisor`): the Godot server managing the player gates its
//! server-authoritative admin actions on them.
//!
//! Kept here from the init to the disconnect, whichever server manages the
//! player: every spawn of the player on a Godot server (first one, zone
//! transfer, hand-over) carries `roles: [..]` in its `object_data`. Only that
//! copy, sent to the Godot server: never persisted, never sent to a client.
//! Shared by every `Server` instance of the process, like move_clocks.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tracing::info;

static ROLES: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Key of the roles in a player's `object_data`, as the Godot server reads it.
pub const KEY: &str = "roles";

/// Records the roles of `player_uuid`'s token at its init (none: forgotten).
pub fn set(player_uuid: &str, roles: Vec<String>) {
    let mut all = ROLES.lock().unwrap();
    if roles.is_empty() {
        all.remove(player_uuid);
    } else {
        info!("[player_roles] player {} has roles {:?}", player_uuid, roles);
        all.insert(player_uuid.to_string(), roles);
    }
}

/// The roles of `player_uuid`, empty when it has none.
pub fn roles(player_uuid: &str) -> Vec<String> {
    ROLES.lock().unwrap().get(player_uuid).cloned().unwrap_or_default()
}

/// Drops the player who left the game.
pub fn forget(player_uuid: &str) {
    ROLES.lock().unwrap().remove(player_uuid);
}

/// Sets the roles in the `object_data` of a player about to be spawned on a
/// Godot server, when it has at least one. Whatever `roles` the data already
/// carried is dropped first: only the verified token grants them.
pub fn mark(object_data: &mut Value, player_uuid: &str) {
    let Some(map) = object_data.as_object_mut() else {
        return;
    };
    map.remove(KEY);
    let roles = roles(player_uuid);
    if !roles.is_empty() {
        map.insert(KEY.to_string(), json!(roles));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_player_with_roles_is_marked_until_it_leaves() {
        let (admin, other) = ("player-roles-admin", "player-roles-other");
        set(admin, vec!["admin".to_string(), "supervisor".to_string()]);
        set(other, Vec::new());
        let mut data = json!({"name": "ann"});
        mark(&mut data, admin);
        assert_eq!(data[KEY], json!(["admin", "supervisor"]));
        let mut data = json!({"name": "bob"});
        mark(&mut data, other);
        assert!(data.get(KEY).is_none());

        forget(admin);
        let mut data = json!({"name": "ann"});
        mark(&mut data, admin);
        assert!(data.get(KEY).is_none(), "gone at the disconnect");

        set(admin, vec!["admin".to_string()]);
        set(admin, Vec::new());
        assert!(roles(admin).is_empty(), "a reconnection without roles clears them");
    }

    #[test]
    fn roles_already_in_the_data_are_not_trusted() {
        let player = "player-roles-forged";
        let mut data = json!({"name": "eve", "roles": ["supervisor"]});
        mark(&mut data, player);
        assert!(data.get(KEY).is_none());
    }
}

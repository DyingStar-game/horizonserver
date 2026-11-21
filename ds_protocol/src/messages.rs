//! Client events (client → Horizon) and the names of the events Horizon sends.
//!
//! Every type keeps the JSON shape the clients send today, so a JSON client sees no
//! change. Free-form payloads (`player:client_action`, `props:spawn_request`) are
//! [`DynValue`]s.

use serde::{Deserialize, Serialize};

use crate::DynValue;

/// `player:init`: the first event of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct PlayerInit {
    pub token: String,
    /// What the client claims to be. Only honoured with `BYPASS_AUTH=true`: otherwise
    /// the realm role `npc` of the token decides.
    #[serde(default)]
    pub is_npc: bool,
    /// The client asks for the clocks of its own moves (client.ini debug_move_clocks).
    #[serde(default)]
    pub move_clocks: bool,
}

/// A 2D vector as the Godot client writes it: `{x, y}`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct Xy {
    pub x: f64,
    pub y: f64,
}

/// A 3D vector as the Godot client writes it: `{x, y, z}`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct Xyz {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// `movement:update_velocity`: the player's input, forwarded to the Godot server
/// that manages the player. Reliable and ordered: the Godot server applies them in
/// the order they were sent (`seq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct UpdateVelocity {
    /// Move direction.
    pub pos: Xy,
    /// Rotation, local to `frame`.
    pub rot: Xyz,
    pub uuid: String,
    /// Order of the moves of this client (`MoveOrder.number` on the client); a
    /// negative value is always newer.
    #[serde(default = "no_seq")]
    pub seq: i64,
    /// Uuid of the parent the rotation is local to, `""` for the world.
    #[serde(default)]
    pub frame: String,
}

fn no_seq() -> i64 {
    -1
}

/// `player:client_action`: free-form, `action` names it.
pub type ClientAction = DynValue;

/// `props:spawn_request`: free-form.
pub type SpawnRequest = DynValue;

/// Payload of the GORC client event `update` of generic props.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bitcode::Encode, bitcode::Decode)]
pub struct PropUpdateRequest {
    pub object_uuid: String,
    /// A JSON object: the keys to change.
    pub new_data: DynValue,
    pub channel: u8,
}

/// Names of the client events, `namespace:event`.
pub mod client_events {
    pub const PLAYER_INIT: (&str, &str) = ("player", "init");
    pub const UPDATE_VELOCITY: (&str, &str) = ("movement", "update_velocity");
    pub const CLIENT_ACTION: (&str, &str) = ("player", "client_action");
    pub const SPAWN_REQUEST: (&str, &str) = ("props", "spawn_request");
}

/// Names of the events Horizon sends to bitcode clients as `ServerFrame::Event`,
/// with the JSON message of a JSON client as a [`DynValue`] payload. A client can
/// turn the payload back into the JSON object and route it as before.
pub mod server_events {
    pub const INIT_ACK: &str = "player:init_ack";
    pub const ERROR: &str = "player:error";
    pub const QUEUED: &str = "player:queued";
    pub const LIVEKIT_TOKEN: &str = "audio:livekit_token";
    pub const LIVEKIT_SUBSCRIBE: &str = "audio:livekit_subscribe";
    pub const LIVEKIT_UNSUBSCRIBE: &str = "audio:livekit_unsubscribe";
    pub const SERVER_INFO: &str = "serverinfo:update_property";

    /// Every name above, declared at startup for short event ids.
    pub const ALL: &[&str] =
        &[INIT_ACK, ERROR, QUEUED, LIVEKIT_TOKEN, LIVEKIT_SUBSCRIBE, LIVEKIT_UNSUBSCRIBE, SERVER_INFO];
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_velocity_reads_what_the_godot_client_sends() {
        let sent = json!({
            "pos": {"x": 0.0, "y": -1.0},
            "rot": {"x": 0.1, "y": 2.0, "z": 0.0},
            "uuid": "3c0e7d3e-6a0e-4f5e-8d2c-7c9b1a2f4e60",
            "seq": 1730000000123004i64,
            "frame": ""
        });
        let velocity: UpdateVelocity = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(velocity.seq, 1730000000123004);
        assert_eq!(serde_json::to_value(&velocity).unwrap(), sent);
        let decoded: UpdateVelocity = bitcode::decode(&bitcode::encode(&velocity)).unwrap();
        assert_eq!(decoded, velocity);
    }

    #[test]
    fn player_init_defaults_its_flags() {
        let init: PlayerInit = serde_json::from_value(json!({"token": "t"})).unwrap();
        assert!(!init.is_npc && !init.move_clocks);
    }

    #[test]
    fn prop_update_request_keeps_its_json_shape() {
        let sent = json!({"object_uuid": "u", "new_data": {"opened": true}, "channel": 1});
        let request: PropUpdateRequest = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), sent);
        let decoded: PropUpdateRequest = bitcode::decode(&bitcode::encode(&request)).unwrap();
        assert_eq!(decoded.new_data.to_value().unwrap(), json!({"opened": true}));
    }
}

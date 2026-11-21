//! Client events into bitcode frames.
//!
//! The Godot client builds its events as Dictionaries, as it did for JSON. Each event
//! the server knows has its `ds_protocol` type: the Dictionary (as JSON) is read into
//! that type, so a missing or mistyped field fails here, loudly, instead of on the
//! server.

use std::collections::HashMap;

use ds_protocol::messages::client_events;
use ds_protocol::{DynValue, PlayerInit, UpdateVelocity};
use horizon_protocol::{ClientFrame, EventKey, bitcode, encode_client};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// `namespace:event`, the name client events travel under.
pub fn event_name((namespace, event): (&str, &str)) -> String {
    format!("{namespace}:{event}")
}

/// The bitcode payload of client event `name`.
pub fn payload(name: &str, data: Value) -> Result<Vec<u8>, String> {
    let is = |event| name == event_name(event);
    if is(client_events::PLAYER_INIT) {
        Ok(bitcode::encode(&typed::<PlayerInit>(name, data)?))
    } else if is(client_events::UPDATE_VELOCITY) {
        Ok(bitcode::encode(&typed::<UpdateVelocity>(name, data)?))
    } else if is(client_events::CLIENT_ACTION) || is(client_events::SPAWN_REQUEST) {
        Ok(bitcode::encode(&DynValue::from(data)))
    } else {
        Err(format!("{name} is not a client event of ds_protocol"))
    }
}

fn typed<T: DeserializeOwned>(name: &str, data: Value) -> Result<T, String> {
    serde_json::from_value(data).map_err(|e| format!("{name}: {e}"))
}

/// The frame of client event `name`: by its id when the server's table has one.
pub fn event_frame(event_ids: &HashMap<String, u16>, name: &str, payload: Vec<u8>) -> Vec<u8> {
    let key = match event_ids.get(name) {
        Some(id) => EventKey::Id(*id),
        None => EventKey::name(name),
    };
    encode_client(&ClientFrame::Event { key, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_events_take_their_type() {
        let bytes = payload("player:init", json!({"token": "t", "move_clocks": true})).unwrap();
        let init: PlayerInit = bitcode::decode(&bytes).unwrap();
        assert_eq!(
            init,
            PlayerInit {
                token: "t".into(),
                is_npc: false,
                move_clocks: true
            }
        );

        let sent = json!({"pos": {"x": 0.0, "y": -1.0}, "rot": {"x": 0.1, "y": 2, "z": 0}, "uuid": "u",
                          "seq": 1730000000123004i64, "frame": ""});
        let velocity: UpdateVelocity =
            bitcode::decode(&payload("movement:update_velocity", sent).unwrap()).unwrap();
        assert_eq!(velocity.seq, 1730000000123004);
        assert_eq!(velocity.rot.y, 2.0);
    }

    #[test]
    fn actions_are_free_form() {
        let action = json!({"action": "vehicle_input", "throttle": 0.5, "steer": -1, "brake": 0});
        let bytes = payload("player:client_action", action.clone()).unwrap();
        assert_eq!(
            bitcode::decode::<DynValue>(&bytes)
                .unwrap()
                .to_value()
                .unwrap(),
            action
        );
    }

    #[test]
    fn a_bad_or_unknown_event_is_an_error() {
        assert!(
            payload("player:init", json!({"move_clocks": true}))
                .unwrap_err()
                .contains("token")
        );
        assert!(payload("actions:action_pressed", json!({})).is_err());
    }

    #[test]
    fn frames_use_the_server_ids() {
        let ids = HashMap::from([("player:init".to_string(), 2u16)]);
        let frame =
            horizon_protocol::decode_client(&event_frame(&ids, "player:init", vec![1]), 0).unwrap();
        assert_eq!(
            frame,
            ClientFrame::Event {
                key: EventKey::Id(2),
                payload: vec![1]
            }
        );
        let frame =
            horizon_protocol::decode_client(&event_frame(&ids, "player:client_action", vec![]), 0)
                .unwrap();
        assert_eq!(
            frame,
            ClientFrame::Event {
                key: EventKey::name("player:client_action"),
                payload: vec![]
            }
        );
    }
}

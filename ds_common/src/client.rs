//! Messages to one client, in the format of its connection.
//!
//! The messages DyingStar sends outside GORC (`init_ack`, errors, the arrival queue,
//! LiveKit, server info) have JSON shapes the Godot client routes on (`type`,
//! `event_type`). A JSON client still gets exactly those bytes. A bitcode client gets
//! a `ServerFrame::Event` named after the message (`ds_protocol::messages::server_events`),
//! whose payload is the same JSON object as a `DynValue`: the client turns it back
//! into the object and routes it the same way.

use ds_protocol::DynValue;
use horizon_event_system::protocol::{encode_server, ServerFrame};
use horizon_event_system::{ClientConnectionRef, EventSystem, Format, Outbound, PlayerId};

/// Sends `message` to `player_id`: its JSON bytes to a JSON client, a
/// `ServerFrame::Event` named `event` to a bitcode client. Reliable.
pub async fn send_message(
    events: &EventSystem,
    player_id: PlayerId,
    event: &str,
    message: &serde_json::Value,
) -> Result<(), String> {
    let sender = events.get_client_response_sender().ok_or("no client response sender")?;
    let format = sender.connection_profile(player_id).await.map(|profile| profile.format).unwrap_or_default();
    let frame = encode_message(events, format, event, message)?;
    sender.send_frame(player_id, Outbound::reliable(frame)).await
}

/// Answers the client of `connection` with `message`, in its format (see [`send_message`]).
pub async fn respond_message(
    events: &EventSystem,
    connection: &ClientConnectionRef,
    event: &str,
    message: &serde_json::Value,
) -> Result<(), String> {
    let frame = encode_message(events, connection.format(), event, message)?;
    connection.respond(&frame).await.map_err(|e| e.to_string())
}

/// The bytes of `message` for a client of `format` (see [`send_message`]).
pub fn encode_message(
    events: &EventSystem,
    format: Format,
    event: &str,
    message: &serde_json::Value,
) -> Result<bytes::Bytes, String> {
    match format {
        Format::Json => serde_json::to_vec(message).map(Into::into).map_err(|e| e.to_string()),
        Format::Bitcode => Ok(encode_server(&ServerFrame::Event {
            key: events.event_table().key(event),
            payload: horizon_event_system::protocol::bitcode::encode(&DynValue::from(message)),
        })),
    }
}

/// Declares the names of the messages above, so bitcode clients get them as short
/// ids in `HelloAck`. Call before clients connect (plugin `register_handlers`).
pub fn declare_events(events: &EventSystem, names: &[&str]) {
    for name in names {
        events.declare_client_event(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_event_system::protocol::decode_server;
    use serde_json::json;

    #[test]
    fn json_clients_get_the_message_bytes() {
        let events = EventSystem::new();
        let message = json!({"type": "queued", "reason": "village", "position": 3});
        let bytes = encode_message(&events, Format::Json, "player:queued", &message).unwrap();
        assert_eq!(&bytes[..], serde_json::to_vec(&message).unwrap().as_slice());
    }

    #[test]
    fn bitcode_clients_get_an_event_with_the_object() {
        let events = EventSystem::new();
        declare_events(&events, &["player:queued"]);
        let message = json!({"type": "queued", "reason": "village", "position": 3});
        let bytes = encode_message(&events, Format::Bitcode, "player:queued", &message).unwrap();
        let ServerFrame::Event { key, payload } = decode_server(&bytes, usize::MAX).unwrap() else {
            panic!("not an event frame");
        };
        assert_eq!(events.event_table().resolve(&key).unwrap(), "player:queued");
        let decoded: DynValue = horizon_event_system::protocol::bitcode::decode(&payload).unwrap();
        assert_eq!(decoded.to_value().unwrap(), message);
    }
}

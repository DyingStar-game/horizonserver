//! Server frames back into the JSON messages of a JSON client.
//!
//! The Godot client routes on the JSON shapes Horizon has always sent (`type`,
//! `event_type`, `zone_data`, `data`...). Rebuilding exactly those shapes here keeps
//! every handler downstream of `client.gd`'s `_route()` unchanged.
//!
//! What a bitcode client receives differs in three ways, undone here:
//!
//! - objects are named by a per-server `handle`, learnt in their `ZoneEnter`. A GORC
//!   frame can come before it (the plugin's own `gorc_zone_enter` notice of a new
//!   object, a player's first moves): it waits, at most [`ORPHAN_TIMEOUT`], and is
//!   replayed right after the zone entry, where a JSON client read the object's id
//!   and type from the frame itself;
//! - a GORC update comes as two frames sharing a `seq`: the transform (position,
//!   rotation, trip clocks) as `<event>/transform`, unreliable, and the other keys as
//!   `<event>`, reliable, only when there are any (`ds_protocol::props`);
//! - envelopes carry no timestamp. Rebuilt messages say `0`, which `Globals.sync_clock`
//!   ignores: the clock keeps calibrating on the messages that carry the server's time
//!   in their payload (`serverinfo`, every second).
//!
//! **Transforms and epochs.** A transform is in the frame of the object's parent. Every
//! frame carries the object's epoch, bumped by the server at each change of parent: a
//! transform is applied only under the epoch of the last reliable frame (zone entry or
//! update), and only if its `seq` is newer than the last applied one. A reliable update
//! is delivered joined with the object's latest transform, as a JSON client got both in
//! one message (`player_update` reads `position` next to `parent_id`). When the parent
//! has just changed, no transform of the new frame is known yet: the update waits for
//! the first one (the server repeats the last transform of a version, so one comes),
//! at most [`HOLD_TIMEOUT`], after which it is delivered alone.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ds_protocol::messages::server_events;
use ds_protocol::props::{join, transform_of};
use ds_protocol::{DynValue, PropState, PropUpdate, TransformUpdate};
use horizon_protocol::bitcode;
use horizon_protocol::{EventKey, ServerFrame};
use serde_json::{Map, Value, json};

/// How long a reliable update waits for a transform of its object's new frame.
pub const HOLD_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a GORC frame for a handle not entered yet waits for its `ZoneEnter`.
pub const ORPHAN_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds of that wait: frames per handle, handles.
const ORPHAN_MAX_FRAMES: usize = 256;
const ORPHAN_MAX_HANDLES: usize = 4096;

/// GORC events whose payload is a plain JSON object (a [`DynValue`]), not a
/// [`PropUpdate`]: the plugins' own zone enter / exit notices.
const PLAIN_GORC_EVENTS: &[&str] = &["gorc_zone_enter", "gorc_zone_exit"];

/// Frames dropped, by reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Drops {
    /// A GORC frame for a handle no zone entry introduced within [`ORPHAN_TIMEOUT`].
    pub unknown_handle: u64,
    /// Not a drop: GORC frames that came before their object's zone entry, delivered
    /// after it.
    pub replayed: u64,
    /// A transform under another epoch than the object's current one.
    pub stale_epoch: u64,
    /// A transform older than (or the same as) the last one applied: the sum of the
    /// three below.
    pub stale_seq: u64,
    /// The same `seq` as the last transform applied, of the same event: the server's
    /// repeat of a version already applied (or the wire's duplicate of a datagram).
    pub stale_seq_repeat: u64,
    /// An older `seq` than the last transform applied, of the same event. The server
    /// sends one stream's frames in `seq` order: overtaken on the wire.
    pub stale_seq_late: u64,
    /// An older `seq` than the last transform applied, which was of another event of
    /// the channel (`move` and `update_property` share the channel's `seq`).
    pub stale_seq_other_event: u64,
    /// Not a drop: transforms received (applied or dropped), the base of the ratios.
    pub transforms: u64,
    /// Not a drop: transforms applied right after one of another event of the channel.
    pub applied_after_other_event: u64,
    /// An event id missing from the `HelloAck` table.
    pub unknown_event: u64,
    /// A payload that does not decode.
    pub bad_payload: u64,
}

/// What a frame turned into.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A message in the shape a JSON client gets.
    Message(Value),
    /// The handshake answer.
    HelloAck { protocol_version: u16, codec: u8 },
    /// The server is closing the session.
    Disconnect { code: u16, reason: String },
}

#[derive(Debug)]
struct Object {
    id: String,
    object_type: String,
    epoch: u32,
}

/// What is known of one channel of one object.
#[derive(Debug, Default)]
struct Stream {
    /// GORC `seq` of the last transform applied.
    last_seq: Option<u16>,
    /// Event of the last transform applied (`move`, `update_property`...), to tell the
    /// drops apart.
    last_event: Option<String>,
    /// The last transform applied, while it is in the object's current frame.
    transform: Option<TransformUpdate>,
    /// Whether this channel carries transforms at all (an update of a channel that
    /// never does is never held).
    moves: bool,
    /// A reliable update waiting for a transform of the new frame.
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Orphan {
    since: Instant,
    frame: ServerFrame,
}

#[derive(Debug)]
struct Pending {
    event: String,
    rest: Map<String, Value>,
    since: Instant,
}

/// Rebuilds JSON messages from server frames. One per session.
#[derive(Debug, Default)]
pub struct Reassembler {
    events: HashMap<u16, String>,
    objects: HashMap<u32, Object>,
    streams: HashMap<(u32, u8), Stream>,
    /// GORC frames waiting for their handle's zone entry, in arrival order.
    orphans: HashMap<u32, Vec<Orphan>>,
    my_player_id: String,
    drops: Drops,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames dropped so far.
    pub fn drops(&self) -> Drops {
        self.drops
    }

    /// Turns one frame (a batch included) into what it means, in order.
    pub fn push(&mut self, frame: ServerFrame, now: Instant, out: &mut Vec<Inbound>) {
        match frame {
            ServerFrame::HelloAck {
                protocol_version,
                codec,
                event_table,
                ..
            } => {
                self.events = event_table.into_iter().collect();
                out.push(Inbound::HelloAck {
                    protocol_version,
                    codec,
                });
            }
            ServerFrame::Event { key, payload } => {
                let Some(name) = self.event_name(&key) else {
                    return;
                };
                let Some(value) = self.dyn_value(&payload) else {
                    return;
                };
                if name == server_events::INIT_ACK
                    && let Some(id) = value.get("player_id").and_then(Value::as_str)
                {
                    self.my_player_id = id.to_string();
                }
                out.push(Inbound::Message(value));
            }
            ServerFrame::ZoneEnter {
                handle,
                object_id,
                object_type,
                channel,
                zone_data,
            } => {
                self.zone_enter(handle, object_id, object_type, channel, &zone_data, out);
                for orphan in self.orphans.remove(&handle).unwrap_or_default() {
                    self.drops.replayed += 1;
                    self.push(orphan.frame, now, out);
                }
            }
            ServerFrame::ZoneExit { handle, channel } => self.zone_exit(handle, channel, out),
            ServerFrame::GorcEvent {
                handle,
                channel,
                seq,
                event,
                payload,
            } => {
                self.gorc_event(handle, channel, seq, &event, &payload, now, out);
            }
            ServerFrame::Batch { frames } => {
                for bytes in frames {
                    match horizon_protocol::decode_server(&bytes, usize::MAX) {
                        // A batch never nests (horizon_protocol), but a hostile one must
                        // not recurse forever.
                        Ok(ServerFrame::Batch { .. }) | Err(_) => self.drops.bad_payload += 1,
                        Ok(inner) => self.push(inner, now, out),
                    }
                }
            }
            ServerFrame::Pong { .. } => {}
            ServerFrame::Disconnect { code, reason } => {
                out.push(Inbound::Disconnect { code, reason })
            }
        }
    }

    /// Delivers, alone, the updates that waited [`HOLD_TIMEOUT`] for a transform, and
    /// drops the GORC frames that waited [`ORPHAN_TIMEOUT`] for a zone entry.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<Inbound>) {
        let mut dropped = 0;
        self.orphans.retain(|_, frames| {
            let before = frames.len();
            frames.retain(|o| now.duration_since(o.since) < ORPHAN_TIMEOUT);
            dropped += (before - frames.len()) as u64;
            !frames.is_empty()
        });
        self.drops.unknown_handle += dropped;
        let mut expired = Vec::new();
        for (&(handle, channel), stream) in &mut self.streams {
            if stream
                .pending
                .as_ref()
                .is_some_and(|p| now.duration_since(p.since) >= HOLD_TIMEOUT)
                && let Some(pending) = stream.pending.take()
            {
                expired.push((handle, channel, pending));
            }
        }
        for (handle, channel, pending) in expired {
            if let Some(message) =
                self.gorc_message(handle, channel, &pending.event, Value::Object(pending.rest))
            {
                out.push(Inbound::Message(message));
            }
        }
    }

    fn event_name(&mut self, key: &EventKey) -> Option<String> {
        match key {
            EventKey::Name(name) => Some(name.clone()),
            EventKey::Id(id) => {
                let name = self.events.get(id).cloned();
                if name.is_none() {
                    self.drops.unknown_event += 1;
                }
                name
            }
        }
    }

    fn dyn_value(&mut self, payload: &[u8]) -> Option<Value> {
        match bitcode::decode::<DynValue>(payload)
            .ok()
            .and_then(|v| v.to_value().ok())
        {
            Some(value) => Some(value),
            None => {
                self.drops.bad_payload += 1;
                None
            }
        }
    }

    fn zone_enter(
        &mut self,
        handle: u32,
        object_id: [u8; 16],
        object_type: String,
        channel: u8,
        zone_data: &[u8],
        out: &mut Vec<Inbound>,
    ) {
        // GenericProps sends a PropState; anything else is taken as a plain value.
        let (transform, data) = match bitcode::decode::<PropState>(zone_data) {
            Ok(state) => match state.data.to_value() {
                Ok(Value::Object(rest)) => (
                    Some(state.transform.clone()),
                    Value::Object(join(&state.transform, &rest)),
                ),
                Ok(other) => (Some(state.transform), other),
                Err(_) => {
                    self.drops.bad_payload += 1;
                    (None, Value::Null)
                }
            },
            Err(_) => (None, self.dyn_value(zone_data).unwrap_or(Value::Null)),
        };
        let epoch = transform.as_ref().map_or(0, |t| t.epoch);
        let id = uuid_string(&object_id);
        self.objects.insert(
            handle,
            Object {
                id: id.clone(),
                object_type: object_type.clone(),
                epoch,
            },
        );
        let transform = transform.filter(|t| !t.is_empty()).map(without_extra);
        self.streams.insert(
            (handle, channel),
            Stream {
                last_seq: None,
                last_event: None,
                moves: transform.is_some(),
                transform,
                pending: None,
            },
        );
        out.push(Inbound::Message(json!({
            "channel": channel,
            "object_id": id,
            "object_type": object_type,
            "player_id": self.my_player_id,
            "timestamp": 0,
            "type": "gorc_zone_enter",
            "zone_data": data,
        })));
    }

    fn zone_exit(&mut self, handle: u32, channel: u8, out: &mut Vec<Inbound>) {
        let Some(object) = self.objects.get(&handle) else {
            self.drops.unknown_handle += 1;
            return;
        };
        // The handle stays known: other channels of the object may still be in range,
        // and the plugin's own notices can follow the zone exit.
        self.streams.remove(&(handle, channel));
        out.push(Inbound::Message(json!({
            "channel": channel,
            "object_id": object.id,
            "object_type": object.object_type,
            "player_id": self.my_player_id,
            "timestamp": 0,
            "type": "gorc_zone_exit",
        })));
    }

    #[allow(clippy::too_many_arguments)]
    fn gorc_event(
        &mut self,
        handle: u32,
        channel: u8,
        seq: u16,
        event: &EventKey,
        payload: &[u8],
        now: Instant,
        out: &mut Vec<Inbound>,
    ) {
        let Some(name) = self.event_name(event) else {
            return;
        };
        if !self.objects.contains_key(&handle) {
            self.hold_orphan(handle, channel, seq, event, payload, now);
            return;
        }
        if let Some(base) = transform_of(&name) {
            match bitcode::decode::<TransformUpdate>(payload) {
                Ok(transform) => self.transform(handle, channel, seq, base, transform, out),
                Err(_) => self.drops.bad_payload += 1,
            }
            return;
        }
        if PLAIN_GORC_EVENTS.contains(&name.as_str()) {
            if let Some(value) = self.dyn_value(payload) {
                out.extend(
                    self.gorc_message(handle, channel, &name, value)
                        .map(Inbound::Message),
                );
            }
            return;
        }
        match bitcode::decode::<PropUpdate>(payload) {
            Ok(update) => match update.data.to_value() {
                Ok(Value::Object(rest)) => {
                    self.update(handle, channel, &name, update.epoch, rest, now, out)
                }
                Ok(other) => out.extend(
                    self.gorc_message(handle, channel, &name, other)
                        .map(Inbound::Message),
                ),
                Err(_) => self.drops.bad_payload += 1,
            },
            // Not a PropUpdate: a plain value, as the plugins' other events.
            Err(_) => {
                if let Some(value) = self.dyn_value(payload) {
                    out.extend(
                        self.gorc_message(handle, channel, &name, value)
                            .map(Inbound::Message),
                    );
                }
            }
        }
    }

    /// Keeps a GORC frame of a handle not entered yet, for its zone entry.
    fn hold_orphan(
        &mut self,
        handle: u32,
        channel: u8,
        seq: u16,
        event: &EventKey,
        payload: &[u8],
        now: Instant,
    ) {
        if !self.orphans.contains_key(&handle) && self.orphans.len() >= ORPHAN_MAX_HANDLES {
            self.drops.unknown_handle += 1;
            return;
        }
        let frames = self.orphans.entry(handle).or_default();
        if frames.len() >= ORPHAN_MAX_FRAMES {
            self.drops.unknown_handle += 1;
            return;
        }
        frames.push(Orphan {
            since: now,
            frame: ServerFrame::GorcEvent {
                handle,
                channel,
                seq,
                event: event.clone(),
                payload: payload.to_vec(),
            },
        });
    }

    fn transform(
        &mut self,
        handle: u32,
        channel: u8,
        seq: u16,
        event: &str,
        transform: TransformUpdate,
        out: &mut Vec<Inbound>,
    ) {
        self.drops.transforms += 1;
        let epoch = self.objects.get(&handle).map_or(0, |o| o.epoch);
        if transform.epoch != epoch {
            self.drops.stale_epoch += 1;
            return;
        }
        let stream = self.streams.entry((handle, channel)).or_default();
        let same_event = stream.last_event.as_deref().is_none_or(|last| last == event);
        if let Some(last) = stream.last_seq.filter(|last| !is_newer(seq, *last)) {
            self.drops.stale_seq += 1;
            match (seq == last, same_event) {
                (true, _) => self.drops.stale_seq_repeat += 1,
                (false, true) => self.drops.stale_seq_late += 1,
                (false, false) => self.drops.stale_seq_other_event += 1,
            }
            return;
        }
        if !same_event {
            self.drops.applied_after_other_event += 1;
        }
        if stream.last_event.as_deref() != Some(event) {
            stream.last_event = Some(event.to_string());
        }
        stream.last_seq = Some(seq);
        stream.moves = true;
        stream.transform = Some(without_extra(transform.clone()));
        let pending = stream.pending.take();
        let mut messages = Vec::with_capacity(2);
        match pending {
            Some(pending) if pending.event == event => {
                messages.push((event.to_string(), join(&transform, &pending.rest)));
            }
            Some(pending) => {
                // An update of another event of the channel: delivered with this frame's
                // position, then the transform's own event.
                messages.push((
                    pending.event.clone(),
                    join(&without_extra(transform.clone()), &pending.rest),
                ));
                messages.push((event.to_string(), join(&transform, &Map::new())));
            }
            None => messages.push((event.to_string(), join(&transform, &Map::new()))),
        }
        for (event, data) in messages {
            out.extend(
                self.gorc_message(handle, channel, &event, Value::Object(data))
                    .map(Inbound::Message),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        handle: u32,
        channel: u8,
        event: &str,
        epoch: u32,
        rest: Map<String, Value>,
        now: Instant,
        out: &mut Vec<Inbound>,
    ) {
        // Reliable frames come in order: their epoch is the object's.
        if let Some(object) = self.objects.get_mut(&handle)
            && object.epoch != epoch
        {
            object.epoch = epoch;
            // Every transform known for this object is in the previous frame.
            for ((h, _), stream) in self.streams.iter_mut() {
                if *h == handle {
                    stream.transform = None;
                }
            }
        }
        let stream = self.streams.entry((handle, channel)).or_default();
        let data = match (&stream.transform, stream.moves) {
            (Some(transform), _) => join(transform, &rest),
            (None, true) => {
                // Hold until a transform of the new frame arrives.
                match &mut stream.pending {
                    Some(pending) if pending.event == event => pending.rest.extend(rest),
                    _ => {
                        stream.pending = Some(Pending {
                            event: event.to_string(),
                            rest,
                            since: now,
                        })
                    }
                }
                return;
            }
            (None, false) => rest,
        };
        out.extend(
            self.gorc_message(handle, channel, event, Value::Object(data))
                .map(Inbound::Message),
        );
    }

    /// A GORC event in the JSON envelope (`player_id` historically repeats the object id).
    fn gorc_message(&self, handle: u32, channel: u8, event: &str, data: Value) -> Option<Value> {
        let object = self.objects.get(&handle)?;
        Some(json!({
            "channel": channel,
            "data": data,
            "event_type": event,
            "object_id": object.id,
            "object_type": object.object_type,
            "player_id": object.id,
            "timestamp": 0,
        }))
    }
}

/// Whether wrapping counter `seq` comes after `last`.
fn is_newer(seq: u16, last: u16) -> bool {
    (seq.wrapping_sub(last) as i16) > 0
}

/// The transform without its trip clocks, for messages that re-deliver an older one:
/// the clocks belong to the trip of that transform's own frame.
fn without_extra(mut transform: TransformUpdate) -> TransformUpdate {
    transform.extra = DynValue::default();
    transform
}

/// A 16-byte id as a hyphenated lowercase uuid, as JSON clients get it.
fn uuid_string(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::encode_server;

    const ID: [u8; 16] = [
        0x0b, 0x6f, 0x0a, 0x43, 0x58, 0xa4, 0x4a, 0x48, 0x9a, 0x8f, 0x0d, 0x6e, 0x1f, 0x8f, 0x2c,
        0x3a,
    ];
    const ID_STR: &str = "0b6f0a43-58a4-4a48-9a8f-0d6e1f8f2c3a";

    fn messages(r: &mut Reassembler, frame: ServerFrame) -> Vec<Value> {
        let mut out = Vec::new();
        r.push(frame, Instant::now(), &mut out);
        out.into_iter()
            .filter_map(|i| match i {
                Inbound::Message(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn transform(epoch: u32, x: f64) -> TransformUpdate {
        TransformUpdate {
            epoch,
            position: Some([x, 2.0, 3.0]),
            rotation: Some([0.0, 1.5, 0.0]),
            extra: DynValue::default(),
        }
    }

    fn zone_enter(epoch: u32) -> ServerFrame {
        let state = PropState {
            transform: transform(epoch, 1.0),
            data: DynValue::from(json!({"player_id": ID_STR, "tools": 2})),
        };
        ServerFrame::ZoneEnter {
            handle: 7,
            object_id: ID,
            object_type: "player".into(),
            channel: 0,
            zone_data: bitcode::encode(&state),
        }
    }

    fn gorc(seq: u16, event: &str, payload: Vec<u8>) -> ServerFrame {
        ServerFrame::GorcEvent {
            handle: 7,
            channel: 0,
            seq,
            event: EventKey::name(event),
            payload,
        }
    }

    fn update(epoch: u32, data: Value) -> Vec<u8> {
        bitcode::encode(&PropUpdate {
            epoch,
            data: DynValue::from(data),
        })
    }

    fn entered() -> Reassembler {
        let mut r = Reassembler::new();
        messages(
            &mut r,
            ServerFrame::Event {
                key: EventKey::name(server_events::INIT_ACK),
                payload: bitcode::encode(&DynValue::from(
                    json!({"type": "init_ack", "player_id": "me"}),
                )),
            },
        );
        messages(&mut r, zone_enter(4));
        r
    }

    #[test]
    fn events_are_the_json_object_and_init_ack_names_the_player() {
        let mut r = Reassembler::new();
        let ack = json!({"type": "init_ack", "player_id": "me"});
        let out = messages(
            &mut r,
            ServerFrame::HelloAck {
                protocol_version: 1,
                codec: 1,
                dictionary_id: None,
                event_table: vec![(3, server_events::INIT_ACK.into())],
            },
        );
        assert!(out.is_empty());
        let out = messages(
            &mut r,
            ServerFrame::Event {
                key: EventKey::Id(3),
                payload: bitcode::encode(&DynValue::from(&ack)),
            },
        );
        assert_eq!(out, vec![ack]);
        assert_eq!(r.my_player_id, "me");
    }

    #[test]
    fn zone_enter_is_the_json_envelope_with_the_joined_state() {
        let mut r = Reassembler::new();
        r.my_player_id = "me".into();
        let out = messages(&mut r, zone_enter(0));
        assert_eq!(
            out,
            vec![json!({
                "channel": 0,
                "object_id": ID_STR,
                "object_type": "player",
                "player_id": "me",
                "timestamp": 0,
                "type": "gorc_zone_enter",
                "zone_data": {
                    "player_id": ID_STR,
                    "tools": 2,
                    "position": {"x": 1.0, "y": 2.0, "z": 3.0},
                    "rotation": {"x": 0.0, "y": 1.5, "z": 0.0},
                },
            })]
        );
    }

    #[test]
    fn a_transform_is_a_move_with_its_clocks() {
        let mut r = entered();
        let mut t = transform(4, 10.0);
        t.extra = DynValue::from(json!({"horizon_sent_ms": 5}));
        let out = messages(&mut r, gorc(1, "move/transform", bitcode::encode(&t)));
        assert_eq!(
            out,
            vec![json!({
                "channel": 0,
                "data": {
                    "horizon_sent_ms": 5,
                    "position": {"x": 10.0, "y": 2.0, "z": 3.0},
                    "rotation": {"x": 0.0, "y": 1.5, "z": 0.0},
                },
                "event_type": "move",
                "object_id": ID_STR,
                "object_type": "player",
                "player_id": ID_STR,
                "timestamp": 0,
            })]
        );
    }

    #[test]
    fn late_or_repeated_transforms_are_dropped() {
        let mut r = entered();
        assert_eq!(
            messages(
                &mut r,
                gorc(5, "move/transform", bitcode::encode(&transform(4, 5.0)))
            )
            .len(),
            1
        );
        assert!(
            messages(
                &mut r,
                gorc(5, "move/transform", bitcode::encode(&transform(4, 5.0)))
            )
            .is_empty()
        );
        assert!(
            messages(
                &mut r,
                gorc(3, "move/transform", bitcode::encode(&transform(4, 3.0)))
            )
            .is_empty()
        );
        assert_eq!(r.drops().stale_seq, 2);
        assert_eq!(
            (r.drops().stale_seq_repeat, r.drops().stale_seq_late),
            (1, 1)
        );
        assert_eq!(r.drops().transforms, 3);
        // The counter wraps.
        let mut r = entered();
        assert_eq!(
            messages(
                &mut r,
                gorc(
                    u16::MAX - 1,
                    "move/transform",
                    bitcode::encode(&transform(4, 6.0))
                )
            )
            .len(),
            1
        );
        assert_eq!(
            messages(
                &mut r,
                gorc(0, "move/transform", bitcode::encode(&transform(4, 7.0)))
            )
            .len(),
            1
        );
        assert!(
            messages(
                &mut r,
                gorc(
                    u16::MAX,
                    "move/transform",
                    bitcode::encode(&transform(4, 6.5))
                )
            )
            .is_empty()
        );
    }

    #[test]
    fn a_transform_of_another_event_moves_the_channel_seq() {
        let mut r = entered();
        let mv = |seq, x| gorc(seq, "move/transform", bitcode::encode(&transform(4, x)));
        assert_eq!(messages(&mut r, mv(5, 5.0)).len(), 1);
        // A property change of the same channel carries the position too, under the next seq.
        let property = gorc(
            6,
            "update_property/transform",
            bitcode::encode(&transform(4, 4.0)),
        );
        assert_eq!(messages(&mut r, property).len(), 1);
        assert_eq!(r.drops().applied_after_other_event, 1);
        // The repeat of the move is now older than the channel's last transform.
        assert!(messages(&mut r, mv(5, 5.0)).is_empty());
        let drops = r.drops();
        assert_eq!(
            (
                drops.stale_seq,
                drops.stale_seq_other_event,
                drops.stale_seq_repeat
            ),
            (1, 1, 0)
        );
        assert_eq!(messages(&mut r, mv(7, 6.0)).len(), 1);
        assert_eq!(r.drops().applied_after_other_event, 2);
    }

    #[test]
    fn a_new_parent_waits_for_a_transform_of_its_frame() {
        let mut r = entered();
        // A datagram of the new frame overtakes the reliable update that announces it.
        assert!(
            messages(
                &mut r,
                gorc(2, "move/transform", bitcode::encode(&transform(5, 100.0)))
            )
            .is_empty()
        );
        assert_eq!(r.drops().stale_epoch, 1);
        // The update is held: no position of the new frame yet.
        assert!(
            messages(
                &mut r,
                gorc(2, "move", update(5, json!({"parent_id": "ship"})))
            )
            .is_empty()
        );
        // A transform of the previous frame arrives late: never applied under the new parent.
        assert!(
            messages(
                &mut r,
                gorc(3, "move/transform", bitcode::encode(&transform(4, 1.0)))
            )
            .is_empty()
        );
        // The first transform of the new frame delivers both, as one JSON move.
        let out = messages(
            &mut r,
            gorc(4, "move/transform", bitcode::encode(&transform(5, 101.0))),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["data"]["parent_id"], "ship");
        assert_eq!(out[0]["data"]["position"]["x"], 101.0);
    }

    #[test]
    fn an_update_in_the_same_frame_comes_with_the_latest_transform() {
        let mut r = entered();
        messages(
            &mut r,
            gorc(1, "move/transform", bitcode::encode(&transform(4, 9.0))),
        );
        let out = messages(
            &mut r,
            gorc(1, "update_property", update(4, json!({"seat": 1}))),
        );
        assert_eq!(out[0]["event_type"], "update_property");
        assert_eq!(out[0]["data"]["seat"], 1);
        assert_eq!(out[0]["data"]["position"]["x"], 9.0);
    }

    #[test]
    fn a_held_update_is_delivered_alone_after_the_timeout() {
        let mut r = entered();
        let start = Instant::now();
        let mut out = Vec::new();
        r.push(
            gorc(2, "move", update(5, json!({"parent_id": "ship"}))),
            start,
            &mut out,
        );
        r.expire(start + HOLD_TIMEOUT / 2, &mut out);
        assert!(out.is_empty());
        r.expire(start + HOLD_TIMEOUT, &mut out);
        let [Inbound::Message(message)] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!(message["data"], json!({"parent_id": "ship"}));
    }

    #[test]
    fn channels_without_transforms_deliver_updates_at_once() {
        let mut r = entered();
        let frame = ServerFrame::GorcEvent {
            handle: 7,
            channel: 3,
            seq: 1,
            event: EventKey::name("update_property"),
            payload: update(9, json!({"opened": true})),
        };
        let out = messages(&mut r, frame);
        assert_eq!(out[0]["data"], json!({"opened": true}));
        assert_eq!(out[0]["channel"], 3);
    }

    #[test]
    fn plugin_notices_and_zone_exits_name_the_object() {
        let mut r = entered();
        let out = messages(
            &mut r,
            gorc(
                1,
                "gorc_zone_exit",
                bitcode::encode(&DynValue::from(json!({}))),
            ),
        );
        assert_eq!(out[0]["event_type"], "gorc_zone_exit");
        assert_eq!(out[0]["data"], json!({}));
        let out = messages(
            &mut r,
            ServerFrame::ZoneExit {
                handle: 7,
                channel: 0,
            },
        );
        assert_eq!(
            out,
            vec![json!({
                "channel": 0,
                "object_id": ID_STR,
                "object_type": "player",
                "player_id": "me",
                "timestamp": 0,
                "type": "gorc_zone_exit",
            })]
        );
    }

    #[test]
    fn frames_before_the_zone_entry_are_replayed_after_it() {
        let mut r = Reassembler::new();
        r.my_player_id = "me".into();
        // The plugin's own notice of a new object, then its first move, before Horizon's zone entry.
        let notice = gorc(
            1,
            "gorc_zone_enter",
            bitcode::encode(&DynValue::from(json!({"name": "box"}))),
        );
        assert!(messages(&mut r, notice).is_empty());
        assert!(
            messages(
                &mut r,
                gorc(2, "move/transform", bitcode::encode(&transform(4, 8.0)))
            )
            .is_empty()
        );
        let out = messages(&mut r, zone_enter(4));
        let kinds: Vec<&str> = out
            .iter()
            .map(|m| {
                m.get("type")
                    .or(m.get("event_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(kinds, vec!["gorc_zone_enter", "gorc_zone_enter", "move"]);
        assert_eq!(
            out[1]["object_id"], ID_STR,
            "the notice names its object again"
        );
        assert_eq!(out[2]["data"]["position"]["x"], 8.0);
        assert_eq!(r.drops().replayed, 2);
        assert_eq!(r.drops().unknown_handle, 0);
    }

    #[test]
    fn frames_whose_zone_entry_never_comes_are_dropped() {
        let mut r = Reassembler::new();
        let start = Instant::now();
        let mut out = Vec::new();
        r.push(
            gorc(1, "move/transform", bitcode::encode(&transform(0, 1.0))),
            start,
            &mut out,
        );
        r.expire(start + ORPHAN_TIMEOUT / 2, &mut out);
        assert_eq!(r.drops().unknown_handle, 0);
        r.expire(start + ORPHAN_TIMEOUT, &mut out);
        assert!(out.is_empty());
        assert_eq!(r.drops().unknown_handle, 1);
        assert!(r.orphans.is_empty());
    }

    #[test]
    fn batches_are_unpacked_in_order() {
        let mut r = entered();
        let frames = vec![
            encode_server(&gorc(
                1,
                "move/transform",
                bitcode::encode(&transform(4, 1.0)),
            ))
            .to_vec(),
            encode_server(&gorc(
                2,
                "move/transform",
                bitcode::encode(&transform(4, 2.0)),
            ))
            .to_vec(),
        ];
        let out = messages(&mut r, ServerFrame::Batch { frames });
        let xs: Vec<f64> = out
            .iter()
            .map(|m| m["data"]["position"]["x"].as_f64().unwrap())
            .collect();
        assert_eq!(xs, vec![1.0, 2.0]);
    }

    #[test]
    fn uuids_are_hyphenated() {
        assert_eq!(uuid_string(&ID), ID_STR);
    }
}

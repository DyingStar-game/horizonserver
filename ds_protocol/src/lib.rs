//! DyingStar payloads between Horizon and its clients.
//!
//! Horizon's `horizon_protocol` owns the frames (`ClientFrame` / `ServerFrame`, the
//! JSON envelopes); this crate owns what DyingStar puts inside them. Every type works
//! in both formats: serde gives the JSON shapes the clients use today, the bitcode
//! derives give the binary form. Encode and decode them with
//! `horizon_protocol::payload::{encode_packed, decode_packed}` (serde for JSON, the
//! derive for bitcode).
//!
//! Shared by the plugins, the NPC service and, later, the Godot client's extension:
//! a change here is a protocol change for all of them (the bitcode form is
//! positional, adding a field breaks older peers).

pub mod dyn_value;
pub mod messages;
pub mod props;

pub use dyn_value::{DynValue, MalformedDynValue, Token};
pub use messages::{ClientAction, PlayerInit, PropUpdateRequest, SpawnRequest, UpdateVelocity, Xy, Xyz};
pub use props::{transform_event, transform_of, PropState, PropUpdate, TransformUpdate, TRANSFORM_SUFFIX};

/// Bumped on every change of a bitcode form in this crate.
pub const DS_PROTOCOL_VERSION: u16 = 1;

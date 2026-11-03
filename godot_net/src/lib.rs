//! GDExtension of the DyingStar Godot client: its link to Horizon over WebTransport,
//! frames in bitcode (Horizon Client Transport v2).
//!
//! GDScript sees one class, [`HorizonLink`]: it sends client events as Dictionaries and
//! polls the server's messages as Dictionaries in the very shapes the JSON WebSocket
//! link delivered, so the client's routing does not change.

// Raised on the code the GodotClass derive generates for the `base` field.
#![allow(clippy::redundant_field_names)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use godot::classes::Os;
use godot::global::Error;
use godot::prelude::*;
use horizon_protocol::Codec;

pub mod convert;
pub mod outbound;
pub mod reassemble;
pub mod session;

use session::{CloseInfo, Options, Received, Session};

struct DsNetExtension;

#[gdextension]
unsafe impl ExtensionLibrary for DsNetExtension {}

/// The client's link to Horizon. Not thread-safe: use it from the main thread.
///
/// ```gdscript
/// var link = ClassDB.instantiate("HorizonLink")
/// link.connect_to_url("https://server.dyingstar-game.space:30001", {"compression": "zstd"})
/// # each frame:
/// for message in link.poll():
///     _route(message)
/// if link.get_status() == link.STATUS_OPEN:
///     link.send_event("player", "init", {"token": token})
/// ```
#[derive(GodotClass)]
#[class(init, base = RefCounted)]
pub struct HorizonLink {
    session: Option<Session>,
    #[init(val = HorizonLink::STATUS_CLOSED)]
    status: i64,
    close: Option<CloseInfo>,
    base: Base<RefCounted>,
}

#[godot_api]
impl HorizonLink {
    /// Connecting, or waiting for the server's handshake answer.
    #[constant]
    const STATUS_CONNECTING: i64 = 0;
    /// Handshake done: events can be sent.
    #[constant]
    const STATUS_OPEN: i64 = 1;
    /// Never connected, or the session ended (see `get_close_kind`).
    #[constant]
    const STATUS_CLOSED: i64 = 2;

    /// Starts connecting to `url` (`https://host:port`). Options:
    /// - `compression`: the listener's, `"zstd"` (default), `"lz4"` or `"none"`;
    /// - `tls_verify`: `false` accepts any certificate (local self-signed Horizon), honoured
    ///   by debug builds only;
    /// - `client`: name and version for the server's logs;
    /// - `connect_timeout`: seconds (default 5).
    #[func]
    fn connect_to_url(&mut self, url: GString, options: VarDictionary) -> Error {
        if self.session.is_some() {
            return Error::ERR_ALREADY_IN_USE;
        }
        let compression = match options
            .get("compression")
            .map(|v| v.stringify().to_string())
            .as_deref()
        {
            None | Some("zstd") => Codec::Zstd,
            Some("lz4") => Codec::Lz4,
            Some("none") => Codec::None,
            Some(other) => {
                godot_error!("HorizonLink: unknown compression '{other}' (zstd, lz4 or none)");
                return Error::ERR_INVALID_PARAMETER;
            }
        };
        let tls_verify = match options.get("tls_verify") {
            Some(v) if !v.booleanize() => {
                if Os::singleton().is_debug_build() {
                    false
                } else {
                    godot_warn!("HorizonLink: tls_verify = false ignored by a release build");
                    true
                }
            }
            _ => true,
        };
        let connect_timeout = options
            .get("connect_timeout")
            .and_then(|v| v.try_to::<f64>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(5.0);
        let client = options
            .get("client")
            .map(|v| v.stringify().to_string())
            .unwrap_or_else(|| format!("dyingstar-godot/{}", env!("CARGO_PKG_VERSION")));
        let options = Options {
            url: url.to_string(),
            compression,
            tls_verify,
            client,
            connect_timeout: Duration::from_secs_f64(connect_timeout),
        };
        match Session::start(options) {
            Ok(session) => {
                self.session = Some(session);
                self.close = None;
                self.status = Self::STATUS_CONNECTING;
                Error::OK
            }
            Err(e) => {
                godot_error!("HorizonLink: {e}");
                Error::FAILED
            }
        }
    }

    /// The messages received since the last call, in order, as Dictionaries in the
    /// shapes of the JSON link; at most `max` (0: all). Also moves the status forward.
    #[func]
    fn poll(&mut self, max: i64) -> VarArray {
        let mut messages = VarArray::new();
        let Some(session) = &self.session else {
            return messages;
        };
        let mut closed = None;
        for item in session.drain(max.max(0) as usize) {
            match item {
                Received::Message(value) => messages.push(&convert::to_variant(&value)),
                Received::Opened => self.status = Self::STATUS_OPEN,
                Received::Closed(info) => closed = Some(info),
            }
        }
        if let Some(info) = closed {
            self.status = Self::STATUS_CLOSED;
            self.close = Some(info);
            // The session's threads are done; release them now.
            self.session = None;
        }
        messages
    }

    /// Sends client event `namespace:event` with `data`, reliably and in order.
    /// `ERR_INVALID_PARAMETER` when `data` does not fit the event's type.
    #[func]
    fn send_event(&mut self, namespace: GString, event: GString, data: VarDictionary) -> Error {
        let Some(session) = &self.session else {
            return Error::ERR_UNCONFIGURED;
        };
        if self.status != Self::STATUS_OPEN {
            return Error::ERR_UNCONFIGURED;
        }
        let name = format!("{namespace}:{event}");
        match session.send(&name, convert::to_json(&data.to_variant())) {
            Ok(()) => Error::OK,
            Err(e) => {
                godot_error!("HorizonLink: {e}");
                Error::ERR_INVALID_PARAMETER
            }
        }
    }

    /// Closes the session (blocks up to about a second, for the threads to stop).
    #[func]
    fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.close();
            self.status = Self::STATUS_CLOSED;
            self.close.get_or_insert(CloseInfo {
                kind: "local",
                code: 0,
                reason: "closed by the client".into(),
            });
        }
    }

    #[func]
    fn get_status(&self) -> i64 {
        self.status
    }

    /// Why the session ended: `timeout` (no answer: server down or UDP blocked),
    /// `address`, `tls`, `refused`, `protocol` (client too old or too new), `server`
    /// (see `get_close_code`), `connection_lost`, `local`; empty while open.
    #[func]
    fn get_close_kind(&self) -> GString {
        self.close
            .as_ref()
            .map_or_else(GString::new, |c| GString::from(c.kind))
    }

    /// The server's close code (4000-4999, `horizon_protocol::CloseCode`), 0 otherwise.
    #[func]
    fn get_close_code(&self) -> i64 {
        self.close.as_ref().map_or(0, |c| c.code as i64)
    }

    #[func]
    fn get_close_reason(&self) -> GString {
        self.close
            .as_ref()
            .map_or_else(GString::new, |c| GString::from(c.reason.as_str()))
    }

    /// QUIC's smoothed round-trip time to the server, in ms (0 before the first sample).
    #[func]
    fn get_rtt_ms(&self) -> f64 {
        self.session.as_ref().map_or(0.0, |s| {
            s.stats.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0
        })
    }

    /// Traffic counters and dropped frames, for the network monitors.
    #[func]
    fn get_stats(&self) -> VarDictionary {
        let mut stats = VarDictionary::new();
        let Some(session) = &self.session else {
            return stats;
        };
        let s = &session.stats;
        let drops = *s.drops.read().unwrap_or_else(|e| e.into_inner());
        for (key, value) in [
            ("bytes_in", s.bytes_in.load(Ordering::Relaxed)),
            ("bytes_out", s.bytes_out.load(Ordering::Relaxed)),
            ("frames_in", s.frames_in.load(Ordering::Relaxed)),
            ("datagrams_in", s.datagrams_in.load(Ordering::Relaxed)),
            ("frames_out", s.frames_out.load(Ordering::Relaxed)),
            ("dropped_unknown_handle", drops.unknown_handle),
            ("replayed_after_zone_enter", drops.replayed),
            ("dropped_stale_epoch", drops.stale_epoch),
            ("dropped_stale_seq", drops.stale_seq),
            ("dropped_stale_seq_repeat", drops.stale_seq_repeat),
            ("dropped_stale_seq_late", drops.stale_seq_late),
            ("dropped_stale_seq_other_event", drops.stale_seq_other_event),
            ("dropped_stale_seq_move_unseen", drops.stale_seq_move_unseen),
            ("transforms_in", drops.transforms),
            ("applied_after_other_event", drops.applied_after_other_event),
            ("dropped_unknown_event", drops.unknown_event),
            ("dropped_bad_payload", drops.bad_payload),
        ] {
            stats.set(key, value as i64);
        }
        stats
    }

    /// The wire protocol version this client speaks.
    #[func]
    fn protocol_version(&self) -> i64 {
        horizon_protocol::PROTOCOL_VERSION as i64
    }
}

//! One WebTransport session with Horizon, run on its own tokio runtime.
//!
//! The Godot main thread never waits on the network: it hands frames to the session
//! ([`Session::send`]) and drains what arrived ([`Session::drain`]). Everything the
//! session learns — messages, the handshake, the end of the session — reaches the main
//! thread through one ordered queue, so the client never sees the session closed before
//! the messages that preceded the close (the server's `error` before its kick).
//!
//! Wire (Horizon Client Transport v2): the client opens one bidirectional stream right
//! after the session is accepted and sends `Hello` on it; reliable frames travel on that
//! stream, each preceded by its LEB128 length; the server's unreliable frames arrive as
//! datagrams. Every frame carries a header byte when the listener compresses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use horizon_protocol::stream::{FrameSplitter, write_frame};
use horizon_protocol::{
    ClientFrame, Codec, Compression, PROTOCOL_VERSION, ServerFrame, decode_server, encode_client,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use wtransport::error::{ConnectingError, ConnectionError};
use wtransport::{ClientConfig, Connection, Endpoint, VarInt};

use crate::outbound;
use crate::reassemble::{Drops, Inbound, Reassembler};

/// Server frames above this are refused (the server is trusted, the cap only guards
/// against a corrupt length prefix).
const MAX_SERVER_FRAME: usize = 16 * 1024 * 1024;
/// How often held updates are checked and the RTT sampled.
const TICK: Duration = Duration::from_millis(100);
/// Without traffic for this long the session is lost; keep-alives are sent well within.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVE: Duration = Duration::from_secs(5);

/// How to reach Horizon.
#[derive(Debug, Clone)]
pub struct Options {
    /// `https://host:port`.
    pub url: String,
    /// The compression of the server's listener: frames must match it.
    pub compression: Codec,
    /// Check the server certificate (false: any certificate, for a local self-signed one).
    pub tls_verify: bool,
    /// Client name and version, for the server's logs.
    pub client: String,
    /// Give up connecting after this long.
    pub connect_timeout: Duration,
}

/// Why and how a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseInfo {
    /// `timeout` (no answer: server down or UDP blocked), `address`, `tls`, `refused`,
    /// `protocol` (version mismatch), `server` (closed by the server, see `code`),
    /// `connection_lost` or `local`.
    pub kind: &'static str,
    /// The server's close code (`horizon_protocol::CloseCode`), 0 otherwise.
    pub code: u32,
    pub reason: String,
}

/// What the main thread drains, in order.
#[derive(Debug)]
pub enum Received {
    Message(Value),
    /// The handshake is done: events can be sent.
    Opened,
    Closed(CloseInfo),
}

/// Counters, readable from any thread.
#[derive(Debug, Default)]
pub struct Stats {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub frames_in: AtomicU64,
    pub datagrams_in: AtomicU64,
    pub frames_out: AtomicU64,
    pub rtt_us: AtomicU64,
    pub drops: RwLock<Drops>,
}

enum Command {
    /// A frame already wrapped for the wire.
    Frame(Vec<u8>),
    Close,
}

/// A running session. Dropping it closes the session.
pub struct Session {
    runtime: Option<tokio::runtime::Runtime>,
    commands: mpsc::UnboundedSender<Command>,
    received: std::sync::mpsc::Receiver<Received>,
    done: Option<oneshot::Receiver<()>>,
    compression: Compression,
    event_ids: Arc<RwLock<HashMap<String, u16>>>,
    pub stats: Arc<Stats>,
}

impl Session {
    /// Starts connecting; progress arrives through [`Session::drain`].
    pub fn start(options: Options) -> Result<Session, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ds-net")
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the network runtime: {e}"))?;
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (received_tx, received) = std::sync::mpsc::channel();
        let (done_tx, done) = oneshot::channel();
        let stats = Arc::new(Stats::default());
        let event_ids = Arc::new(RwLock::new(HashMap::new()));
        let compression = Compression::with_codec(options.compression);
        let task = Task {
            options,
            compression: compression.clone(),
            received: received_tx,
            stats: stats.clone(),
            event_ids: event_ids.clone(),
        };
        runtime.spawn(async move {
            task.run(commands_rx).await;
            let _ = done_tx.send(());
        });
        Ok(Session {
            runtime: Some(runtime),
            commands,
            received,
            done: Some(done),
            compression,
            event_ids,
            stats,
        })
    }

    /// Queues client event `name` (`namespace:event`) on the reliable stream.
    pub fn send(&self, name: &str, data: Value) -> Result<(), String> {
        let payload = outbound::payload(name, data)?;
        let frame = {
            let ids = self.event_ids.read().unwrap_or_else(|e| e.into_inner());
            outbound::event_frame(&ids, name, payload)
        };
        self.send_frame(&frame)
    }

    fn send_frame(&self, frame: &[u8]) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(frame.len() + 4);
        write_frame(&mut bytes, &self.compression.wrap(frame));
        self.commands
            .send(Command::Frame(bytes))
            .map_err(|_| "the session is closed".to_string())
    }

    /// What arrived since the last call, in order; at most `max` items (0: all).
    pub fn drain(&self, max: usize) -> Vec<Received> {
        let mut out = Vec::new();
        while max == 0 || out.len() < max {
            match self.received.try_recv() {
                Ok(item) => out.push(item),
                Err(_) => break,
            }
        }
        out
    }

    /// Closes the session and stops its threads (waits up to about a second).
    pub fn close(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let _ = self.commands.send(Command::Close);
        if let Some(done) = self.done.take() {
            // The close notice goes out before the threads stop.
            let _ = runtime
                .block_on(async { tokio::time::timeout(Duration::from_millis(500), done).await });
        }
        // Worker threads must be gone before Godot unloads the library (a thread left
        // running in an unloaded DLL crashes the game on quit, as LiveKit's did).
        runtime.shutdown_timeout(Duration::from_millis(500));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// The session's side of the runtime.
struct Task {
    options: Options,
    compression: Compression,
    received: std::sync::mpsc::Sender<Received>,
    stats: Arc<Stats>,
    event_ids: Arc<RwLock<HashMap<String, u16>>>,
}

impl Task {
    async fn run(self, commands: mpsc::UnboundedReceiver<Command>) {
        let info = match tokio::time::timeout(self.options.connect_timeout, self.connect()).await {
            Err(_) => close_info("timeout", 0, "no answer from the server"),
            Ok(Err(info)) => info,
            Ok(Ok(connection)) => self.serve(connection, commands).await,
        };
        let _ = self.received.send(Received::Closed(info));
    }

    async fn connect(&self) -> Result<Connection, CloseInfo> {
        let builder = ClientConfig::builder().with_bind_default();
        let builder = if self.options.tls_verify {
            builder.with_native_certs()
        } else {
            builder.with_no_cert_validation()
        };
        let config = builder
            .max_idle_timeout(Some(IDLE_TIMEOUT))
            .map_err(|e| close_info("address", 0, &e.to_string()))?
            .keep_alive_interval(Some(KEEPALIVE))
            .build();
        let endpoint =
            Endpoint::client(config).map_err(|e| close_info("address", 0, &e.to_string()))?;
        endpoint
            .connect(&self.options.url)
            .await
            .map_err(connecting_error)
    }

    async fn serve(
        &self,
        connection: Connection,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) -> CloseInfo {
        let (mut send, recv) = match connection.open_bi().await {
            Ok(opening) => match opening.await {
                Ok(streams) => streams,
                Err(e) => return close_info("connection_lost", 0, &e.to_string()),
            },
            Err(e) => return connection_error(e, false),
        };
        let hello = ClientFrame::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: self.options.client.clone(),
            dictionary_id: None,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &self.compression.wrap(&encode_client(&hello)));
        if let Err(e) = send.write_all(&bytes).await {
            return close_info("connection_lost", 0, &e.to_string());
        }
        self.stats
            .bytes_out
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);

        // The stream is read by its own task: a length-prefixed read cut in the middle by
        // a datagram would lose bytes.
        let (frames_tx, mut frames) = mpsc::channel::<Result<Vec<u8>, String>>(256);
        let stats = self.stats.clone();
        let reader = tokio::spawn(async move {
            let mut recv = recv;
            let mut splitter = FrameSplitter::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = match recv.read(&mut buffer).await {
                    Ok(Some(read)) => read,
                    Ok(None) => return,
                    Err(e) => {
                        let _ = frames_tx.send(Err(e.to_string())).await;
                        return;
                    }
                };
                stats.bytes_in.fetch_add(read as u64, Ordering::Relaxed);
                splitter.push(&buffer[..read]);
                loop {
                    match splitter.next_frame(MAX_SERVER_FRAME + 1) {
                        Ok(Some(frame)) => {
                            if frames_tx.send(Ok(frame)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = frames_tx.send(Err(e.to_string())).await;
                            return;
                        }
                    }
                }
            }
        });

        let mut reassembler = Reassembler::new();
        let mut undecodable = 0u64;
        let mut inbound = Vec::new();
        let mut disconnect: Option<CloseInfo> = None;
        let mut opened = false;
        let mut tick = tokio::time::interval(TICK);
        let info = loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Ok(frame)) => self.frame(&frame, &mut reassembler, &mut inbound, &mut undecodable),
                    Some(Err(e)) => break closed_after_stream(&connection, opened, &e).await,
                    None => break closed_after_stream(&connection, opened, "stream finished").await,
                },
                datagram = connection.receive_datagram() => match datagram {
                    Ok(datagram) => {
                        let payload = datagram.payload();
                        self.stats.bytes_in.fetch_add(payload.len() as u64, Ordering::Relaxed);
                        self.stats.datagrams_in.fetch_add(1, Ordering::Relaxed);
                        self.frame(&payload, &mut reassembler, &mut inbound, &mut undecodable);
                    }
                    Err(e) => break connection_error(e, opened),
                },
                command = commands.recv() => match command {
                    Some(Command::Frame(bytes)) => {
                        if let Err(e) = send.write_all(&bytes).await {
                            break close_info("connection_lost", 0, &e.to_string());
                        }
                        self.stats.bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        self.stats.frames_out.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(Command::Close) | None => {
                        connection.close(VarInt::from_u32(0), b"client closing");
                        break close_info("local", 0, "closed by the client");
                    }
                },
                _ = tick.tick() => {
                    reassembler.expire(Instant::now(), &mut inbound);
                    self.stats.rtt_us.store(connection.rtt().as_micros() as u64, Ordering::Relaxed);
                }
            }
            for item in inbound.drain(..) {
                match item {
                    Inbound::Message(value) => {
                        let _ = self.received.send(Received::Message(value));
                    }
                    Inbound::HelloAck {
                        protocol_version, ..
                    } if protocol_version != PROTOCOL_VERSION => {
                        connection.close(VarInt::from_u32(0), b"protocol version");
                        disconnect = Some(close_info(
                            "protocol",
                            0,
                            &format!("server speaks protocol {protocol_version}"),
                        ));
                    }
                    Inbound::HelloAck { .. } => {
                        opened = true;
                        let _ = self.received.send(Received::Opened);
                    }
                    Inbound::Disconnect { code, reason } => {
                        disconnect = Some(close_info("server", code as u32, &reason));
                    }
                }
            }
            if undecodable > 0 && !opened && disconnect.is_none() {
                // Not even the handshake answer reads: this client's frame format or compression
                // is not the listener's (an outdated build or a wrong client.ini).
                disconnect = Some(close_info(
                    "protocol",
                    0,
                    "frames do not match the server's listener",
                ));
            }
            let mut drops = reassembler.drops();
            drops.bad_payload += undecodable;
            *self.stats.drops.write().unwrap_or_else(|e| e.into_inner()) = drops;
            if disconnect.is_some() && !opened {
                break disconnect
                    .clone()
                    .unwrap_or_else(|| close_info("server", 0, ""));
            }
        };
        reader.abort();
        // The server's own words beat what the transport saw of its close.
        disconnect.filter(|_| info.kind != "local").unwrap_or(info)
    }

    /// One frame off the wire (stream or datagram). `undecodable` counts the frames that
    /// are not valid for this listener (compression or format).
    fn frame(
        &self,
        bytes: &[u8],
        reassembler: &mut Reassembler,
        inbound: &mut Vec<Inbound>,
        undecodable: &mut u64,
    ) {
        self.stats.frames_in.fetch_add(1, Ordering::Relaxed);
        let decoded = self
            .compression
            .unwrap(bytes, MAX_SERVER_FRAME)
            .map_err(|e| e.to_string())
            .and_then(|body| decode_server(&body, MAX_SERVER_FRAME).map_err(|e| e.to_string()));
        match decoded {
            Ok(frame) => {
                if let ServerFrame::HelloAck { event_table, .. } = &frame {
                    let mut ids = self.event_ids.write().unwrap_or_else(|e| e.into_inner());
                    *ids = event_table
                        .iter()
                        .map(|(id, name)| (name.clone(), *id))
                        .collect();
                }
                reassembler.push(frame, Instant::now(), inbound);
            }
            Err(e) => {
                *undecodable += 1;
                if *undecodable <= 10 {
                    eprintln!("[ds_net] undecodable frame ({} bytes): {e}", bytes.len());
                }
            }
        }
    }
}

/// How the session ended once its stream did: the server closes the stream, then the session with
/// its close code, which says more than the stream's end.
async fn closed_after_stream(connection: &Connection, opened: bool, reason: &str) -> CloseInfo {
    match tokio::time::timeout(Duration::from_millis(500), connection.closed()).await {
        Ok(error) => connection_error(error, opened),
        Err(_) => close_info("connection_lost", 0, reason),
    }
}

fn close_info(kind: &'static str, code: u32, reason: &str) -> CloseInfo {
    CloseInfo {
        kind,
        code,
        reason: reason.to_string(),
    }
}

fn connecting_error(error: ConnectingError) -> CloseInfo {
    let reason = error.to_string();
    match error {
        ConnectingError::InvalidUrl(_)
        | ConnectingError::DnsLookup(_)
        | ConnectingError::DnsNotFound
        | ConnectingError::InvalidServerName(_) => close_info("address", 0, &reason),
        ConnectingError::SessionRejected => close_info("refused", 0, &reason),
        ConnectingError::ConnectionError(e) => connection_error(e, false),
        _ => close_info("connection_lost", 0, &reason),
    }
}

fn connection_error(error: ConnectionError, opened: bool) -> CloseInfo {
    let reason = error.to_string();
    match error {
        ConnectionError::ApplicationClosed(close) => {
            let code = u32::try_from(close.code().into_inner()).unwrap_or(0);
            close_info("server", code, &String::from_utf8_lossy(close.reason()))
        }
        ConnectionError::TimedOut if !opened => close_info("timeout", 0, &reason),
        ConnectionError::LocallyClosed => close_info("local", 0, &reason),
        _ if reason.contains("certificate") || reason.contains("crypto") => {
            close_info("tls", 0, &reason)
        }
        _ => close_info("connection_lost", 0, &reason),
    }
}

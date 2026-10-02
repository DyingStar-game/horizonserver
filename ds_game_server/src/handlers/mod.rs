pub mod spawn_player;
pub mod spawn_prop;
pub mod player_movement;
pub mod player_action;
pub mod initial_objects;
pub mod update_prop;

use horizon_event_system::EventError;
use serde_json::Value;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error};
use websocket::message::OwnedMessage;
use websocket::sender::Writer;

/// The send queue of one Godot connection. Writing to the socket blocks once its TCP
/// buffer is full, which is what a frozen Godot server does to it: written inline, under
/// a std mutex, every task sending to that server blocked a worker of the plugin runtime
/// until none was left and the whole mesh manager stopped (minikube, 2026-10-02). The
/// write now happens on a thread of its own per connection; senders only queue.
pub struct Outbox {
    tx: std::sync::mpsc::Sender<OwnedMessage>,
    pending: Arc<AtomicUsize>,
}

impl Outbox {
    /// Starts the writer thread of a new connection. `pending` counts the messages
    /// queued and not written yet.
    pub fn spawn(mut writer: Writer<TcpStream>, name: &str, pending: Arc<AtomicUsize>) -> Outbox {
        pending.store(0, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel::<OwnedMessage>();
        let counter = Arc::clone(&pending);
        let thread_name = format!("ws-out-{}", name);
        let spawned = std::thread::Builder::new().name(thread_name.clone()).spawn(move || {
            // Ends when the Outbox is dropped (connection replaced or declared dead)
            // and the queue is drained, or at the first write error.
            for message in rx {
                counter.fetch_sub(1, Ordering::Relaxed);
                if let Err(e) = writer.send_message(&message) {
                    error!("[{}] ERROR sending message to game server: {}", thread_name, e);
                    break;
                }
            }
        });
        if let Err(e) = spawned {
            error!("could not start the websocket writer thread of {}: {}", name, e);
        }
        Outbox { tx, pending }
    }
}

/// The websocket writer to one Godot server, shared by every handler of that server.
pub type WsWriter = Arc<Mutex<Option<Outbox>>>;

/// Queues one JSON message for the Godot server; never blocks. Fails when the
/// connection is gone (no writer, or its thread stopped on a write error).
pub fn send_ws(websocket: &WsWriter, tag: &str, message: &Value) -> Result<(), EventError> {
    let ws_guard = websocket.lock().map_err(|e| {
        error!("[{}] websocket lock error: {}", tag, e);
        EventError::HandlerExecution(format!("websocket lock error: {}", e))
    })?;
    let Some(outbox) = ws_guard.as_ref() else {
        debug!("[{}] No websocket writer available", tag);
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    };
    outbox.pending.fetch_add(1, Ordering::Relaxed);
    if outbox.tx.send(OwnedMessage::Text(message.to_string())).is_err() {
        outbox.pending.fetch_sub(1, Ordering::Relaxed);
        error!("[{}] websocket writer of the game server stopped", tag);
        return Err(EventError::HandlerExecution("Message blocked: writer stopped".to_string()));
    }
    Ok(())
}

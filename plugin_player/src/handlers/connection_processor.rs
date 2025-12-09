//! # Connection Processor with MPSC Queue
//!
//! This module provides a sequential connection processor that uses an MPSC channel
//! to queue player connection/disconnection events and process them one at a time.
//! This prevents deadlocks that can occur when many players connect simultaneously.
//!
//! ## Problem Solved
//!
//! When many players connect at the same time, concurrent access to shared resources
//! (DashMap, GORC instances, spatial tracking) can cause contention and deadlocks.
//! By processing connections sequentially through an MPSC channel, we eliminate
//! race conditions while maintaining good throughput.
//!
//! ## Architecture
//!
//! ```text
//! Player Connect Event → MPSC Channel → Background Task → Sequential Processing
//!                                              ↓
//!                                    handle_player_connected()
//! ```

use std::sync::Arc;
use dashmap::DashMap;
use horizon_event_system::{
    EventSystem, PlayerId, GorcObjectId, PlayerDisconnectedEvent,
};
use tokio::sync::mpsc;
use tracing::{debug, info, error};

/// Represents a connection-related event to be processed
#[derive(Debug)]
pub enum ConnectionEvent {
    /// A new player has connected
    PlayerConnected {
        event: serde_json::Value,
        players: Arc<DashMap<PlayerId, GorcObjectId>>,
        events: Arc<EventSystem>,
    },
    /// A player has disconnected
    PlayerDisconnected {
        event: PlayerDisconnectedEvent,
        players: Arc<DashMap<PlayerId, GorcObjectId>>,
        events: Arc<EventSystem>,
    },
    /// Shutdown signal to stop the processor
    Shutdown,
}

/// Thread-safe sender handle that can be cloned and shared across closures.
/// This is the primary interface for queueing connection events.
#[derive(Clone)]
pub struct ConnectionSender {
    sender: mpsc::Sender<ConnectionEvent>,
}

impl ConnectionSender {
    /// Queues a player connection event for processing.
    ///
    /// This method is non-blocking and will return immediately after queueing.
    /// The actual connection handling happens in the background task.
    ///
    /// # Parameters
    ///
    /// - `event`: The connection event JSON data
    /// - `players`: Shared player registry
    /// - `events`: Event system reference
    ///
    /// # Returns
    ///
    /// `Result<(), String>` - Success or error if channel is full/closed
    pub fn queue_connection(
        &self,
        event: serde_json::Value,
        players: Arc<DashMap<PlayerId, GorcObjectId>>,
        events: Arc<EventSystem>,
    ) -> Result<(), String> {
        let connection_event = ConnectionEvent::PlayerConnected {
            event,
            players,
            events,
        };
        
        self.sender
            .try_send(connection_event)
            .map_err(|e| format!("Failed to queue connection: {}", e))
    }

    /// Queues a player disconnection event for processing.
    ///
    /// # Parameters
    ///
    /// - `event`: The disconnection event
    /// - `players`: Shared player registry
    /// - `events`: Event system reference
    ///
    /// # Returns
    ///
    /// `Result<(), String>` - Success or error if channel is full/closed
    pub fn queue_disconnection(
        &self,
        event: PlayerDisconnectedEvent,
        players: Arc<DashMap<PlayerId, GorcObjectId>>,
        events: Arc<EventSystem>,
    ) -> Result<(), String> {
        let connection_event = ConnectionEvent::PlayerDisconnected {
            event,
            players,
            events,
        };
        
        self.sender
            .try_send(connection_event)
            .map_err(|e| format!("Failed to queue disconnection: {}", e))
    }

    /// Signals the processor to shut down gracefully.
    pub fn shutdown(&self) {
        let _ = self.sender.try_send(ConnectionEvent::Shutdown);
    }
}

/// Manages player connections sequentially using an MPSC channel.
///
/// This processor ensures that player connections and disconnections are handled
/// one at a time, preventing deadlocks from concurrent access to shared resources.
pub struct ConnectionProcessor {
    /// Sender that can be cloned and shared
    sender: ConnectionSender,
}

impl ConnectionProcessor {
    /// Creates a new ConnectionProcessor and starts the background processing task.
    ///
    /// # Parameters
    ///
    /// - `runtime`: The tokio runtime to spawn the background task on
    /// - `buffer_size`: Size of the MPSC channel buffer (default: 1000)
    ///
    /// # Returns
    ///
    /// A new `ConnectionProcessor` instance with the background task running.
    pub fn new(runtime: Arc<tokio::runtime::Runtime>, buffer_size: usize) -> Self {
        let (sender, receiver) = mpsc::channel(buffer_size);
        
        // Spawn the background processing task
        runtime.spawn(Self::process_connections(receiver));
        
        info!("🎮 ConnectionProcessor: Started with buffer size {}", buffer_size);
        
        Self { 
            sender: ConnectionSender { sender },
        }
    }

    /// Returns a cloneable sender handle for queueing events.
    ///
    /// This sender can be cloned and passed to closures that need to
    /// queue connection events.
    pub fn sender(&self) -> ConnectionSender {
        self.sender.clone()
    }

    /// Background task that processes connection events sequentially.
    ///
    /// This runs in a loop, processing one event at a time from the channel.
    /// This ensures no concurrent access to shared resources during connection handling.
    async fn process_connections(mut receiver: mpsc::Receiver<ConnectionEvent>) {
        info!("🎮 ConnectionProcessor: Background task started");
        
        let mut processed_count: u64 = 0;
        let mut error_count: u64 = 0;
        
        while let Some(event) = receiver.recv().await {
            match event {
                ConnectionEvent::PlayerConnected { event, players, events } => {
                    debug!("🎮 ConnectionProcessor: Processing connection (queue position: {})", processed_count + 1);
                    
                    match super::connection::handle_player_connected(
                        event,
                        players,
                        events,
                    ).await {
                        Ok(()) => {
                            processed_count += 1;
                            debug!("🎮 ConnectionProcessor: ✅ Connection processed successfully (total: {})", processed_count);
                        }
                        Err(e) => {
                            error_count += 1;
                            error!("🎮 ConnectionProcessor: ❌ Connection failed: {} (errors: {})", e, error_count);
                        }
                    }
                }
                
                ConnectionEvent::PlayerDisconnected { event, players, events } => {
                    debug!("🎮 ConnectionProcessor: Processing disconnection for player {}", event.player_id);
                    
                    match super::connection::handle_player_disconnected(
                        event,
                        players,
                        events,
                    ).await {
                        Ok(()) => {
                            processed_count += 1;
                            debug!("🎮 ConnectionProcessor: ✅ Disconnection processed successfully");
                        }
                        Err(e) => {
                            error_count += 1;
                            error!("🎮 ConnectionProcessor: ❌ Disconnection failed: {}", e);
                        }
                    }
                }
                
                ConnectionEvent::Shutdown => {
                    info!("🎮 ConnectionProcessor: Shutdown requested. Processed {} events, {} errors", 
                        processed_count, error_count);
                    break;
                }
            }
        }
        
        info!("🎮 ConnectionProcessor: Background task stopped. Total processed: {}, errors: {}", 
            processed_count, error_count);
    }
}

impl Drop for ConnectionProcessor {
    fn drop(&mut self) {
        // Try to send shutdown signal when processor is dropped
        self.sender.shutdown();
    }
}

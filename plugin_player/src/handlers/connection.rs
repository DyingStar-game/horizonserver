//! # Player Connection Handler
//! 
//! Manages the complete lifecycle of player connections within the GORC system.
//! This module handles the critical events of players joining and leaving the game world,
//! ensuring proper resource allocation, cleanup, and integration with the spatial
//! replication system.
//! 
//! ## Key Responsibilities
//! 
//! - **Player Registration**: Creates and registers new GORC player objects when clients connect
//! - **Spatial Integration**: Adds players to the zone-based replication system
//! - **Resource Management**: Tracks player-to-object mappings for efficient cleanup
//! - **Graceful Cleanup**: Removes players and their associated objects on disconnect
//! 
//! ## Connection Flow
//! 
//! 1. **PlayerConnectedEvent** received from core event system
//! 2. Create new `GorcPlayer` object with default spawn position
//! 3. Register object with GORC instances manager (returns unique ID)
//! 4. Update player position to trigger zone message distribution
//! 5. Add player to spatial tracking system
//! 6. Store mapping for future cleanup
//! 
//! ## Disconnection Flow
//! 
//! 1. **PlayerDisconnectedEvent** received from core event system
//! 2. Lookup stored GORC object ID for the player
//! 3. Remove player from all tracking systems
//! 4. Clean up resource mappings
//! 
//! ## Error Handling
//! 
//! All connection operations are designed to be fault-tolerant:
//! - Missing GORC instances manager is logged but doesn't crash the plugin
//! - Failed registrations are properly logged with context
//! - Cleanup operations are idempotent and safe to retry

use std::sync::Arc;
use dashmap::DashMap;
use horizon_event_system::{
    EventSystem, PlayerId, GorcObjectId, Vec3,
    PlayerConnectedEvent, PlayerDisconnectedEvent,
};
use tracing::{debug, info, error};
use crate::player::GorcPlayer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPlayerDataObjectData {
    pub name: String,
    pub position: Vec3,
    pub rotation: Vec3,
    pub connection_id: PlayerId,
    pub parent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPlayerData {
    pub object_type: String,
    pub object_uuid: PlayerId,
    pub object_data: NewPlayerDataObjectData,
}

/// Handles player connection events and integrates new players into the GORC system.
/// 
/// This function is called whenever a player successfully connects to the server.
/// It creates a new player object, registers it with the spatial replication system,
/// and ensures the player is properly tracked for future events.
/// 
/// # Parameters
/// 
/// - `event`: The connection event containing player ID and connection details
/// - `players`: Shared registry mapping player IDs to GORC object IDs
/// - `events`: Event system for spatial updates and GORC registration
/// 
/// # Returns
/// 
/// `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Success or error details
/// 
/// # Example Flow
/// 
/// ```text
/// PlayerConnectedEvent { player_id: 42 }
///     ↓
/// Create GorcPlayer object at (0,0,0)
///     ↓
/// Register with GORC instances → GorcObjectId
///     ↓
/// Update spatial position (triggers zone messages)
///     ↓
/// Add to spatial tracking system
///     ↓
/// Store mapping: 42 → GorcObjectId
/// ```
pub async fn handle_player_connected(
    event: serde_json::Value,
    players: Arc<DashMap<PlayerId, GorcObjectId>>,
    events: Arc<EventSystem>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🎮 CONNECTION STEP 1: handle_player_connected called for player {}", event["object_data"]["connection_id"]);
    println!("🎮 GORC: Processing player connection for player {}", event["object_data"]["connection_id"]);
    println!("🎮 GORC: Player data received: {:?}", event);
    
    // Verify GORC instances manager is available
    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available for player {}", event["object_data"]["connection_id"]);
        return Ok(()); // Not a fatal error, just log and continue
    };
    
    println!("🎮 GORC: ✅ GORC instances manager available, registering player {}", event["object_data"]["connection_id"]);
    
    // Clone object_data before creating player to avoid partial move
    let object_data = event["object_data"].clone();
    
    println!("🎮 GORC: Step A - parsing position");
    // Get local position from event
    let local_position: Vec3 = serde_json::from_value(event["object_data"]["position"].clone())?;
    println!("🎮 GORC: Step B - parsing player_id");
    let player_id = PlayerId::from_str(
        event["object_uuid"].as_str()
            .ok_or("Missing or invalid object_uuid")?
    )?;
    println!("🎮 GORC: Step C - checking parent_id");
    
    // Calculate global position FIRST based on parent object
    let parent_id = event["object_data"]["parent_id"].as_str().map(|s| s.to_string());
    let mut global_position = local_position.clone();

    if let Some(parent_id_str) = &parent_id {
        println!("🎮 GORC: Step D - parsing parent GORC ID: {}", parent_id_str);
        if let Ok(parent_gorc_id) = GorcObjectId::from_str(parent_id_str) {
            println!("🎮 GORC: Step E - getting parent position (lock-free)");
            if let Some(parent_global_position) = gorc_instances.get_object_position(parent_gorc_id) {
                println!("🎮 GORC: Step F - got parent position: {:?}", parent_global_position);
                global_position = horizon_event_system::Vec3 {
                    x: parent_global_position.x + local_position.x,
                    y: parent_global_position.y + local_position.y,
                    z: parent_global_position.z + local_position.z,
                };
            } else {
                println!("🎮 GORC: Step F - parent position not found");
            }
        }
    }
    println!("🎮 GORC: Step G - creating GorcPlayer");
    
    // CRITICAL: Create GorcPlayer with GLOBAL position so zones are centered correctly
    let player = GorcPlayer::new(
        player_id,
        event["object_data"]["name"].as_str()
            .ok_or("Missing or invalid name")?
            .to_string(),
        global_position,  // Use global position for proper zone calculations
        event["object_data"]["parent_id"].as_str()
            .ok_or("Missing or invalid parent_id")?
            .to_string(),
    );
    
    // Execute GORC registration directly (we're already in async context from lib.rs spawn)
    println!("🎮 GORC: Starting player registration for player {}", event["object_uuid"]);
    
    // Register the player object with GORC spatial system
    let maybe_obj_id = match event["object_uuid"].as_str() {
        Some(uuid_str) => match GorcObjectId::from_str(uuid_str) {
            Ok(id) => Some(id),
            Err(e) => {
                error!("🎮 GORC: Failed to parse object_uuid '{}': {}", uuid_str, e);
                None
            }
        },
        None => {
            error!("🎮 GORC: object_uuid is not a string");
            None
        }
    };

    // CRITICAL FIX: Add player to spatial tracking BEFORE registering their object
    // This ensures the player gets subscribed to their own object during registration
    // Without this, the player would have 0 subscribers and never receive movement confirmations
    gorc_instances.add_player(player_id, global_position).await;
    println!("🎮 GORC: ✅ Player {} added to spatial tracking BEFORE object registration", player_id);

    // Now register the object - player will be auto-subscribed since they're in player_positions
    let gorc_id = gorc_instances.register_object_with_uuid(player, global_position, maybe_obj_id).await;

    // Store the GORC ID for future operations (movement, cleanup, etc.)
    players.insert(player_id, gorc_id);

    println!("🎮 GORC: ✅ Player {} registered with GORC instance ID {:?} at position {:?}",
        event["object_data"]["connection_id"], gorc_id, event["object_data"]["position"]);

    // Send GORC object info to client on channel 0 (player's own object)
    if let Err(e) = events.emit_gorc_instance(
        gorc_id,
        0, // Channel 0 for critical info
        "gorc_zone_enter",
        &object_data,
        horizon_event_system::Dest::Client
    ).await {
        error!("🎮 GORC: ❌ Failed to send GORC info to client: {}", e);
    } else {
        println!("🎮 GORC: ✅ Sent GORC object info to client: {}", object_data);
    }

    // CRITICAL: Subscribe the new player to all existing objects they're within range of
    // This detects zones for planets, cities, etc. that existed before the player connected
    if let Err(e) = events.subscribe_player_to_existing_objects(player_id, global_position).await {
        error!("🎮 GORC: ❌ Failed to subscribe player to existing objects: {}", e);
    } else {
        println!("🎮 GORC: ✅ Player subscribed to existing objects successfully");
    }

    // Note: add_player was already called BEFORE register_object_with_uuid
    // to ensure the player gets subscribed to their own object

    println!("🎮 GORC: ✅ Player {} fully integrated into GORC system", player_id);
    
    // now send player data to the game server
    if let Err(e) = events
        .emit_plugin("plugingameserver", "new_player", &serde_json::json!(event))
        .await
    {
        tracing::error!("Failed to emit plugin event to player plugin: {}", e);
    }

    Ok(())
}

/// Handles player disconnection events and performs complete cleanup.
/// 
/// This function is called when a player disconnects from the server.
/// It ensures all resources associated with the player are properly cleaned up,
/// including removal from spatial tracking and GORC object registry.
/// 
/// # Parameters
/// 
/// - `event`: The disconnection event containing player ID
/// - `players`: Shared registry mapping player IDs to GORC object IDs
/// 
/// # Returns
/// 
/// `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Success or error details
/// 
/// # Cleanup Process
/// 
/// 1. Look up the player's GORC object ID
/// 2. Remove from player registry
/// 3. Log successful cleanup with relevant IDs
/// 
/// Note: The GORC instances manager automatically handles spatial cleanup
/// when objects are no longer referenced.
pub async fn handle_player_disconnected(
    event: PlayerDisconnectedEvent,
    players: Arc<DashMap<PlayerId, GorcObjectId>>,
    events: Arc<EventSystem>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("🎮 GORC: Processing player disconnection for player {}", event.player_id);
    
    // Remove player from registry and get their GORC object ID
    if let Some((_, gorc_id)) = players.remove(&event.player_id) {
        debug!("🎮 GORC: ✅ Player {} disconnected and unregistered (GORC ID {:?})", 
            event.player_id, gorc_id);
    } else {
        // This could happen if the player was never successfully registered
        error!("🎮 GORC: Player {} disconnected but was not in registry", event.player_id);
    }

    // Send GORC object info to client on channel 0
    if let Err(e) = events.emit_gorc_instance(
        GorcObjectId::from_str(event.player_id.to_string().as_str())?,
        0, // Channel 0 for critical info
        "gorc_zone_exit",
        &serde_json::json!({}),
        horizon_event_system::Dest::Client
    ).await {
        error!("🎮 GORC: ❌ Failed to send GORC info to client: {}", e);
    } else {
        println!("🎮 GORC: ✅ Sent GORC zone exit info to client");
    }

    // Verify GORC instances manager is available
    let Some(gorc_instances) = events.get_gorc_instances() else {
        error!("🎮 GORC: ❌ No GORC instances manager available for diconnect player");
        return Ok(()); // Not a fatal error, just log and continue
    };

    gorc_instances.remove_player(event.player_id).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    /// Test that connection handler creates proper player mapping
    #[tokio::test]
    async fn test_player_connection_creates_mapping() {
        // This would require mock GORC instances manager
        // Implementation depends on available testing infrastructure
    }
    
    /// Test that disconnection handler properly cleans up
    #[tokio::test] 
    async fn test_player_disconnection_cleanup() {
        // Test cleanup logic with mock registry
    }
}
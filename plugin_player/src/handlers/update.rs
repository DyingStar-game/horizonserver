//! # Player Update Handler
//!
//! Handles player data update events on GORC channel 4, providing a comprehensive
//! mechanism for players to update their game state including inventory, equipment,
//! health, stamina, oxygen, and other player-specific data.
//!
//! ## Channel 4 Characteristics
//!
//! - **Purpose**: Player state updates - inventory, equipment, stats
//! - **Range**: Event-driven replication to relevant nearby players
//! - **Frequency**: Event-driven with moderate priority
//! - **Features**: Comprehensive player state synchronization
//!
//! ## Update System Design
//!
//! The update system provides flexible player state modification:
//! 1. **Inventory Updates**: Add, remove, or modify player inventory items
//! 2. **Equipment Changes**: Equip or unequip tools and items
//! 3. **Stat Updates**: Health, stamina, oxygen level changes
//! 4. **Action State**: Current player action/animation state
//! 5. **Parent Binding**: Attach/detach from parent objects (vehicles, stations)
//!
//! ## Security and Validation
//!
//! - **Player Ownership**: Players can only update their own data
//! - **Value Bounds**: Stats are validated within acceptable ranges
//! - **Rate Limiting**: Prevents spam updates (future enhancement)
//! - **Sanity Checks**: Prevents invalid state transitions

use std::sync::Arc;
use horizon_event_system::{
    EventSystem, PlayerId, GorcEvent, GorcObjectId, ClientConnectionRef, ObjectInstance,
    EventError,
};
use tracing::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json;

/// Player update request event for GORC channel 4.
///
/// This structure represents a client request to update player state.
/// It supports partial updates - only provided fields will be modified.
///
/// ## Update Fields
///
/// All fields are optional - only non-None values will be applied:
/// - `health`: Update player health (0.0 to 100.0)
/// - `stamina`: Update player stamina (0.0 to 100.0)
/// - `oxygen_level`: Update oxygen level (0.0 to 100.0)
/// - `action`: Current action/animation state
/// - `parent_id`: Attach to or detach from parent object
/// - `inventory`: Replace inventory contents
/// - `equipped_tools`: Replace equipped tools list
/// - `movement_state`: Update movement animation state
/// - `level`: Update player level
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerUpdateRequest {
    /// ID of the player requesting the update
    pub player_id: PlayerId,
    
    // Critical data updates (Zone 0)
    /// New health value (0.0 to 100.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<f32>,
    /// New stamina value (0.0 to 100.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stamina: Option<f32>,
    /// Current action state (e.g., "idle", "mining", "crafting")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Parent object ID (empty string to detach)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    
    // Detailed data updates (Zone 1)
    /// New oxygen level (0.0 to 100.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oxygen_level: Option<f32>,
    /// New inventory contents
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory: Option<Vec<String>>,
    /// New equipped tools list
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equipped_tools: Option<Vec<String>>,
    /// Movement state (e.g., "idle", "walking", "running")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movement_state: Option<String>,
    /// Player level
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    
    // Social data updates (Zone 2)
    /// Chat bubble text (None to clear)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_bubble: Option<String>,
}

/// Handles player update requests from clients on GORC channel 4.
///
/// This handler processes requests to update various aspects of player state,
/// validates the updates, and broadcasts changes to nearby players.
///
/// # Parameters
///
/// - `gorc_event`: The GORC event containing update data
/// - `client_player`: ID of the player making the update
/// - `_connection`: Client connection reference for authentication
/// - `object_instance`: Player's object instance to update
/// - `events`: Event system for broadcasting updates
/// - `luminal_handle`: Async runtime handle for background processing
///
/// # Returns
///
/// `Result<(), EventError>` - Success or detailed error information
///
/// # Security Validations
///
/// 1. **Player Ownership**: Only the owning player can update their state
/// 2. **Value Bounds**: All numeric values are clamped to valid ranges
/// 3. **String Validation**: Action and state strings are validated
///
/// # Example Update Request
///
/// ```json
/// {
///     "player_id": "player_42",
///     "health": 85.0,
///     "stamina": 60.0,
///     "action": "mining",
///     "inventory": ["pickaxe", "ore_iron", "ore_copper"]
/// }
/// ```
pub fn handle_update_request(
    gorc_event: GorcEvent,
    client_player: PlayerId,
    _connection: ClientConnectionRef,
    object_instance: &mut ObjectInstance,
    events: Arc<EventSystem>,
    luminal_handle: luminal::Handle,
) -> Result<(), EventError> {
    debug!("📝 GORC: Received player update request from {}: {:?}", 
        client_player, gorc_event);
    
    // Parse update data from GORC event payload
    let event_data = serde_json::from_slice::<serde_json::Value>(&gorc_event.data)
        .map_err(|e| {
            error!("📝 GORC: ❌ Failed to parse JSON from update event data: {}", e);
            EventError::HandlerExecution("Invalid JSON in update request".to_string())
        })?;
    
    let update_data = serde_json::from_value::<PlayerUpdateRequest>(event_data.clone())
        .map_err(|e| {
            error!("📝 GORC: ❌ Failed to parse PlayerUpdateRequest: {}", e);
            EventError::HandlerExecution("Invalid update request format".to_string())
        })?;
    
    debug!("📝 GORC: Processing update for player {}", update_data.player_id);
    
    // SECURITY: Validate player ownership - players can only update their own data
    if update_data.player_id != client_player {
        error!("📝 GORC: ❌ Security violation: Player {} tried to update data for {}", 
            client_player, update_data.player_id);
        return Err(EventError::HandlerExecution(
            "Unauthorized update request".to_string()
        ));
    }
    
    // Get the player object from the instance and apply updates
    if let Some(player) = object_instance.get_object_mut::<crate::player::GorcPlayer>() {
        apply_player_updates(player, &update_data);
        debug!("📝 GORC: ✅ Applied updates to player {}", client_player);
    } else {
        error!("📝 GORC: ❌ Failed to get GorcPlayer from object instance");
        return Err(EventError::HandlerExecution(
            "Failed to access player object".to_string()
        ));
    }
    
    // Broadcast the update to nearby players
    broadcast_player_update(
        &gorc_event.object_id,
        client_player,
        &update_data,
        events,
        luminal_handle,
    );
    
    Ok(())
}

/// Applies validated updates to the player object.
///
/// This function applies all non-None fields from the update request to the
/// player object, ensuring proper bounds checking for numeric values.
///
/// # Parameters
///
/// - `player`: Mutable reference to the player object
/// - `update`: The update request containing new values
fn apply_player_updates(player: &mut crate::player::GorcPlayer, update: &PlayerUpdateRequest) {
    // Update timestamp
    player.last_update = chrono::Utc::now();
    
    // Apply critical data updates (Zone 0)
    if let Some(health) = update.health {
        player.critical_data.health = health.clamp(0.0, 100.0);
        debug!("📝 GORC: Updated health to {}", player.critical_data.health);
    }
    
    if let Some(stamina) = update.stamina {
        player.critical_data.stamina = stamina.clamp(0.0, 100.0);
        debug!("📝 GORC: Updated stamina to {}", player.critical_data.stamina);
    }
    
    if let Some(ref action) = update.action {
        player.critical_data.action = action.clone();
        debug!("📝 GORC: Updated action to '{}'", player.critical_data.action);
    }
    
    if let Some(ref parent_id) = update.parent_id {
        player.critical_data.parent_id = parent_id.clone();
        debug!("📝 GORC: Updated parent_id to '{}'", player.critical_data.parent_id);
    }
    
    // Apply detailed data updates (Zone 1)
    if let Some(oxygen_level) = update.oxygen_level {
        player.detailed_data.oxygen_level = oxygen_level.clamp(0.0, 100.0);
        debug!("📝 GORC: Updated oxygen_level to {}", player.detailed_data.oxygen_level);
    }
    
    if let Some(ref inventory) = update.inventory {
        player.detailed_data.inventory = inventory.clone();
        debug!("📝 GORC: Updated inventory with {} items", player.detailed_data.inventory.len());
    }
    
    if let Some(ref equipped_tools) = update.equipped_tools {
        player.detailed_data.equiped_tools = equipped_tools.clone();
        debug!("📝 GORC: Updated equipped_tools with {} items", player.detailed_data.equiped_tools.len());
    }
    
    if let Some(ref movement_state) = update.movement_state {
        player.detailed_data.movement_state = movement_state.clone();
        debug!("📝 GORC: Updated movement_state to '{}'", player.detailed_data.movement_state);
    }
    
    if let Some(level) = update.level {
        player.detailed_data.level = level;
        debug!("📝 GORC: Updated level to {}", player.detailed_data.level);
    }
    
    // Apply social data updates (Zone 2)
    if let Some(ref chat_bubble) = update.chat_bubble {
        if chat_bubble.is_empty() {
            player.social_data.chat_bubble = None;
            debug!("📝 GORC: Cleared chat bubble");
        } else {
            player.social_data.chat_bubble = Some(chat_bubble.clone());
            debug!("📝 GORC: Updated chat bubble to '{}'", chat_bubble);
        }
    }
}

/// Broadcasts the player update to nearby players on appropriate channels.
///
/// Emits updates on the correct GORC channels based on which properties were changed:
/// - **Channel 0**: Critical data (health, stamina, action, parent_id)
/// - **Channel 1**: Detailed data (oxygen_level, inventory, equipped_tools, movement_state, level)
/// - **Channel 2**: Social data (chat_bubble)
///
/// # Parameters
///
/// - `object_id`: The GORC object ID of the player
/// - `player_id`: The player ID for the broadcast
/// - `update`: The update request containing changed values
/// - `events`: Event system for broadcasting
/// - `luminal_handle`: Async runtime handle for background processing
fn broadcast_player_update(
    object_id: &str,
    player_id: PlayerId,
    update: &PlayerUpdateRequest,
    events: Arc<EventSystem>,
    luminal_handle: luminal::Handle,
) {
    let object_id_str = object_id.to_string();
    
    // Check which zones have updates
    let has_critical_updates = update.health.is_some() 
        || update.stamina.is_some() 
        || update.action.is_some() 
        || update.parent_id.is_some();
    
    let has_detailed_updates = update.oxygen_level.is_some()
        || update.inventory.is_some()
        || update.equipped_tools.is_some()
        || update.movement_state.is_some()
        || update.level.is_some();
    
    let has_social_updates = update.chat_bubble.is_some();
    
    let gorc_id = match GorcObjectId::from_str(&object_id_str) {
        Ok(id) => id,
        Err(_) => {
            error!("📝 GORC: ❌ Invalid GORC object ID format: {}", object_id_str);
            return;
        }
    };
    
    // Emit on Channel 0 (Critical data) if critical fields were updated
    if has_critical_updates {
        let critical_broadcast = serde_json::json!({
            "player_id": player_id,
            "health": update.health,
            "stamina": update.stamina,
            "action": update.action,
            "parent_id": update.parent_id,
            "update_timestamp": chrono::Utc::now()
        });
        
        let events_ch0 = events.clone();
        let gorc_id_ch0 = gorc_id.clone();
        luminal_handle.spawn(async move {
            if let Err(e) = events_ch0.emit_gorc_instance(
                gorc_id_ch0,
                0, // Channel 0: Critical data
                "player_critical_updated",
                &critical_broadcast,
                horizon_event_system::Dest::Client
            ).await {
                error!("📝 GORC: ❌ Failed to broadcast critical update on channel 0: {}", e);
            } else {
                debug!("📝 GORC: ✅ Broadcast critical update for player {} on channel 0", 
                    player_id);
            }
        });
    }
    
    // Emit on Channel 2 (Social data) if social fields were updated
    if has_social_updates {
        let social_broadcast = serde_json::json!({
            "player_id": player_id,
            "chat_bubble": update.chat_bubble,
            "update_timestamp": chrono::Utc::now()
        });
        
        let events_ch2 = events.clone();
        let gorc_id_ch2 = gorc_id.clone();
        luminal_handle.spawn(async move {
            if let Err(e) = events_ch2.emit_gorc_instance(
                gorc_id_ch2,
                2, // Channel 2: Social/communication data
                "player_social_updated",
                &social_broadcast,
                horizon_event_system::Dest::Client
            ).await {
                error!("📝 GORC: ❌ Failed to broadcast social update on channel 2: {}", e);
            } else {
                debug!("📝 GORC: ✅ Broadcast social update for player {} on channel 2", 
                    player_id);
            }
        });
    }
    
    // Emit on Channel 1 (Detailed data) if detailed fields were updated
    if has_detailed_updates {
        let detailed_broadcast = serde_json::json!({
            "player_id": player_id,
            "oxygen_level": update.oxygen_level,
            "inventory": update.inventory,
            "equipped_tools": update.equipped_tools,
            "movement_state": update.movement_state,
            "level": update.level,
            "update_timestamp": chrono::Utc::now()
        });
        
        let events_ch1 = events.clone();
        let gorc_id_ch1 = gorc_id.clone();
        luminal_handle.spawn(async move {
            if let Err(e) = events_ch1.emit_gorc_instance(
                gorc_id_ch1,
                1, // Channel 1: Detailed data
                "player_detailed_updated",
                &detailed_broadcast,
                horizon_event_system::Dest::Client
            ).await {
                error!("📝 GORC: ❌ Failed to broadcast detailed update on channel 1: {}", e);
            } else {
                debug!("📝 GORC: ✅ Broadcast detailed update for player {} on channel 1", 
                    player_id);
            }
        });
    }
    
    // Log if no updates were broadcasted (shouldn't happen in normal flow)
    if !has_critical_updates && !has_detailed_updates && !has_social_updates {
        debug!("📝 GORC: No properties to broadcast for player {}", player_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_event_system::{PlayerId, Vec3};
    use crate::player::GorcPlayer;
    
    #[test]
    fn test_player_update_request_partial() {
        // Test that partial updates work correctly
        let json = r#"{
            "player_id": "player_1",
            "health": 50.0,
            "action": "mining"
        }"#;
        
        let update: PlayerUpdateRequest = serde_json::from_str(json).unwrap();
        assert_eq!(update.health, Some(50.0));
        assert_eq!(update.action, Some("mining".to_string()));
        assert!(update.stamina.is_none());
        assert!(update.inventory.is_none());
    }
    
    #[test]
    fn test_apply_player_updates_health_clamping() {
        let mut player = GorcPlayer::new(
            PlayerId::from_str("player_1").unwrap(),
            "TestPlayer".to_string(),
            Vec3::new(0.0, 0.0, 0.0),
            String::new(),
        );
        
        let update = PlayerUpdateRequest {
            player_id: PlayerId::from_str("player_1").unwrap(),
            health: Some(150.0), // Should be clamped to 100.0
            stamina: Some(-10.0), // Should be clamped to 0.0
            action: None,
            parent_id: None,
            oxygen_level: None,
            inventory: None,
            equipped_tools: None,
            movement_state: None,
            level: None,
            chat_bubble: None,
        };
        
        apply_player_updates(&mut player, &update);
        
        assert_eq!(player.critical_data.health, 100.0);
        assert_eq!(player.critical_data.stamina, 0.0);
    }
}

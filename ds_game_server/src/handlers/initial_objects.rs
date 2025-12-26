use horizon_event_system::{
    EventError,
};
use tracing::{info, debug, warn, error};
use std::sync::{Arc, Mutex};
use std::net::TcpStream;
use websocket::sender::Writer;
use websocket::message::OwnedMessage;
use serde_json::json;
use ds_common::events::GenericPropsRequest;
use crate::server::Zone;
use std::collections::HashMap;

pub async fn handle_initial_object(
    event: serde_json::Value,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
    managed_objects: Arc<Mutex<Vec<String>>>,
    managed_players: Arc<Mutex<Vec<String>>>,
    zone: &Zone,
) -> Result<(), EventError> {
    info!("[initial_object] handler called");
    debug!("[initial_object] handler called with event: {:?}", event);
    // debug!("[initial_object] websocket Arc ptr: {:p}", &websocket);

    let mut ws_guard = match websocket.lock() {
        Ok(g) => g,
        Err(e) => {
            error!("[initial_object] websocket lock error: {}", e);
            return Err(EventError::HandlerExecution(format!("websocket lock error: {}", e)));
        }
    };
    if ws_guard.is_none() {
        debug!("[initial_object] No websocket writer available");
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    }

    // Inside the handler:
    if let Ok(items) = serde_json::from_value::<HashMap<String, GenericPropsRequest>>(event["items"].clone()) {
        for (uuid, item) in items {
            // check if item in the server zone
            let pos = &item.object_data["_global_position"];
            let x = pos["x"].as_f64().unwrap_or(0.0);
            let y = pos["y"].as_f64().unwrap_or(0.0);
            let z = pos["z"].as_f64().unwrap_or(0.0);
            // Remove _global_position from object_data
            let mut item = item;
            if let Some(obj) = item.object_data.as_object_mut() {
                obj.remove("_global_position");
            }
            if item.object_type == "player" {
                info!("[initial_object] Checking item {} position ({}, {}, {}) against zone {:?}", uuid, x, y, z, zone);
            }
            if !is_position_in_zone(x, y, z, zone) {
                debug!("[initial_object] Item {} position ({}, {}, {}) is out of zone {:?}, skipping", uuid, x, y, z, zone);
                // we send to create objects, but we will send freeze after

                let message = json!({
                    "namespace": "server",
                    "event": "initial_object",
                    "data": item,
                });

                debug!("[initial_object] constructed message: {:?}", message);
                if let Some(w) = ws_guard.as_mut() {
                    debug!("[initial_object] Sending message to websocket");
                    if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                        error!("[initial_object] ERROR sending message to game server: {}", e);
                        return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                    } else {
                        debug!("[initial_object] Message sent successfully");
                    }
                }

                let message = json!({
                    "namespace": "server",
                    "event": "freeze_object",
                    "data": item,
                });

                debug!("[freeze_object] constructed message: {:?}", message);
                if let Some(w) = ws_guard.as_mut() {
                    debug!("[freeze_object] Sending message to websocket");
                    if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                        error!("[freeze_object] ERROR sending message to game server: {}", e);
                        return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                    } else {
                        debug!("[freeze_object] Message sent successfully");
                    }
                }
            } else {
                // server manage the object in its zone
                managed_objects.lock().unwrap().push(item.object_uuid.clone());
                debug!("🔧 [initial_object] Item {}: type={}, data={:?}", uuid, item.object_type, item.object_data);
                if item.object_type == "player" {
                    managed_players.lock().unwrap().push(item.object_uuid.clone());
                }

                let message = json!({
                    "namespace": "server",
                    "event": "initial_object",
                    "data": item,
                });

                debug!("[initial_object] constructed message: {:?}", message);
                if let Some(w) = ws_guard.as_mut() {
                    debug!("[initial_object] Sending message to websocket");
                    if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                        error!("[initial_object] ERROR sending message to game server: {}", e);
                        return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                    } else {
                        debug!("[initial_object] Message sent successfully");
                    }
                }
            }
        }
        info!("[initial_object] List of players on new server manage now: {:?}", managed_players.lock().unwrap());
    }

    // send final message
    let message = json!({
        "namespace": "server",
        "event": "initial_object_end",
        "data": {},
    });
    debug!("[initial_object] send end of initial objects");
    if let Some(w) = ws_guard.as_mut() {
        debug!("[initial_object] Sending message to websocket");
        if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
            error!("[initial_object] ERROR sending message to game server: {}", e);
            return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
        } else {
            debug!("[initial_object] Message sent successfully");
        }
    }
    Ok(())
}

pub async fn handle_freeze_object(
    event: serde_json::Value,
    websocket: Arc<Mutex<Option<Writer<TcpStream>>>>,
    managed_objects: Arc<Mutex<Vec<String>>>,
    managed_players: Arc<Mutex<Vec<String>>>,
    zone: &Zone,
) -> Result<(), EventError> {
    info!("[freeze_object] handler called");
    debug!("[freeze_object] handler called with event: {:?}", event);
    // debug!("[freeze_object] websocket Arc ptr: {:p}", &websocket);

    let mut ws_guard = match websocket.lock() {
        Ok(g) => g,
        Err(e) => {
            error!("[freeze_object] websocket lock error: {}", e);
            return Err(EventError::HandlerExecution(format!("websocket lock error: {}", e)));
        }
    };
    if ws_guard.is_none() {
        debug!("[freeze_object] No websocket writer available");
        return Err(EventError::HandlerExecution("No websocket writer available".to_string()));
    }

    // Inside the handler:
    if let Ok(items) = serde_json::from_value::<HashMap<String, GenericPropsRequest>>(event["items"].clone()) {
        for (uuid, item) in items {
            // check if item in the server zone
            let pos = &item.object_data["_global_position"];
            let x = pos["x"].as_f64().unwrap_or(0.0);
            let y = pos["y"].as_f64().unwrap_or(0.0);
            let z = pos["z"].as_f64().unwrap_or(0.0);
            if is_position_in_zone(x, y, z, zone) {
                debug!("[initial_object] Item {} position ({}, {}, {}) is out of zone {:?}, skipping", uuid, x, y, z, zone);
                continue;
            }
            // Remove _global_position from object_data
            let mut item = item;
            if let Some(obj) = item.object_data.as_object_mut() {
                obj.remove("_global_position");
            }

            managed_objects.lock().unwrap().push(item.object_uuid.clone());
            debug!("🔧 [freeze_object] Item {}: type={}, data={:?}", uuid, item.object_type, item.object_data);
            if item.object_type == "player" {
                // NOTE: For runtime transfers (player_out_of_zone), managed_players
                // is already removed synchronously in server.rs BEFORE freeze is called.
                // For initial zone splits, the player should also have been removed
                // by the split logic. Only remove here as a defensive fallback.
                let mut players = managed_players.lock().unwrap();
                if let Some(pos) = players.iter().position(|x| x == &item.object_uuid) {
                    warn!("[freeze_object] Player {} still in managed_players during freeze (unexpected), removing as fallback", item.object_uuid);
                    players.remove(pos);
                }
            }

            let message = json!({
                "namespace": "server",
                "event": "freeze_object",
                "data": item,
            });

            debug!("[freeze_object] constructed message: {:?}", message);
            if let Some(w) = ws_guard.as_mut() {
                debug!("[freeze_object] Sending message to websocket");
                if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
                    error!("[freeze_object] ERROR sending message to game server: {}", e);
                    return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
                } else {
                    debug!("[freeze_object] Message sent successfully");
                }
            }
        }
    }
    info!("[initial_object] List of players on old server manage now: {:?}", managed_players.lock().unwrap());


    // // send final message
    // let message = json!({
    //     "namespace": "server",
    //     "event": "freeze_object_end",
    //     "data": {},
    // });
    // debug!("[freeze_object] send end of freezing objects");
    // if let Some(w) = ws_guard.as_mut() {
    //     debug!("[freeze_object] Sending message to websocket");
    //     if let Err(e) = w.send_message(&OwnedMessage::Text(message.to_string())) {
    //         error!("[freeze_object] ERROR sending message to game server: {}", e);
    //         return Err(EventError::HandlerExecution(format!("Message blocked: {}", e)));
    //     } else {
    //         debug!("[freeze_object] Message sent successfully");
    //     }
    // }

    Ok(())
}

pub fn is_position_in_zone(x: f64, y: f64, z: f64, zone: &Zone) -> bool {
    x >= zone.min_x && x <= zone.max_x &&
    y >= zone.min_y && y <= zone.max_y &&
    z >= zone.min_z && z <= zone.max_z
}
use serde::{Deserialize, Serialize};
use horizon_event_system::Vec3;
use uuid::Uuid;

// Define the player
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Player {
    pub name: String,
    pub position: Vec3,
    pub rotation: Vec3,
    pub uuid: String,
}

impl Player {
    pub fn new(name: String, position: Vec3, rotation: Vec3) -> Self {
        Self {
            name,
            position,
            rotation,
            uuid: Uuid::new_v4().to_string()
        }
    }
}
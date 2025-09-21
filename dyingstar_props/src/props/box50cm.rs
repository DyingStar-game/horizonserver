use serde::{Deserialize, Serialize};
use horizon_event_system::Vec3;
use uuid::Uuid;

// Define the box50cm
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Box50cm {
    pub position: Vec3,
    pub rotation: Vec3,
    pub uuid: String,
}

impl Box50cm {
    pub fn new(position: Vec3, rotation: Vec3, mut uuid: String) -> Self {
        if uuid.is_empty() {
            uuid = Uuid::new_v4().to_string();
        }        
        Self {
            position,
            rotation,
            uuid
        }
    }
}
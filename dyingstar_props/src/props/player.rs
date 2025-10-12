use serde::{Deserialize, Serialize};
use horizon_event_system::{GorcObject, ReplicationLayer, ReplicationPriority, CompressionType, Vec3, PlayerId};
use uuid::Uuid;

// Define the player
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Player {
    pub name: String,
    pub position: Vec3,
    pub rotation: Vec3,
    pub uuid: PlayerId,
    pub equipment: Vec<String>,
    pub health: f32,
    pub hs_player_id: PlayerId,
}

impl Player {
    pub fn new(name: String, position: Vec3, rotation: Vec3, uuid: PlayerId) -> Self {
        Self {
            name,
            position,
            rotation,
            uuid,
            equipment: Vec::new(),
            health: 100.0,
            hs_player_id,
        }
    }
}

impl GorcObject for Player {
    fn type_name(&self) -> &'static str {
        "player"
    }

    fn position(&self) -> Vec3 {
        self.position
    }

    fn get_priority(&self, observer_pos: Vec3) -> ReplicationPriority {
        let distance = self.position.distance(observer_pos);
        if distance < 50.0 {
            ReplicationPriority::Critical
        } else if distance < 200.0 {
            ReplicationPriority::High
        } else if distance < 500.0 {
            ReplicationPriority::Normal
        } else {
            ReplicationPriority::Low
        }
    }

    fn serialize_for_layer(&self, layer: &ReplicationLayer) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut data = serde_json::Map::new();

        for property in &layer.properties {
            match property.as_str() {
                "name" => { data.insert("name".to_string(), serde_json::Value::String(self.name.clone())); },
                "position" => { data.insert("position".to_string(), serde_json::to_value(&self.position)?); },
                "rotation" => { data.insert("rotation".to_string(), serde_json::to_value(&self.rotation)?); },
                "uuid" => { data.insert("uuid".to_string(), serde_json::Value::String(self.uuid.clone().to_string())); },
                "equipment" => { data.insert("equipment".to_string(), serde_json::to_value(&self.equipment)?); },
                "health" => { data.insert("health".to_string(), serde_json::Value::Number(serde_json::Number::from_f64(self.health as f64).unwrap())); },
                _ => {},
            }
        }

        Ok(serde_json::to_vec(&data)?)
    }

    fn get_layers(&self) -> Vec<ReplicationLayer> {
        vec![
            ReplicationLayer::new(
                0, 50.0, 60.0,
                vec!["position".to_string(), "rotation".to_string(), "health".to_string(), "uuid".to_string()],
                CompressionType::Delta,
            ),
            ReplicationLayer::new(
                1, 200.0, 20.0,
                vec!["equipment".to_string()],
                CompressionType::Lz4,
            ),
            ReplicationLayer::new(
                3, 1000.0, 5.0,
                vec!["name".to_string()],
                CompressionType::Lz4,
            ),
        ]
    }

    fn update_position(&mut self, new_position: Vec3) {
        self.position = new_position;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn clone_object(&self) -> Box<dyn GorcObject> {
        Box::new(self.clone())
    }
}

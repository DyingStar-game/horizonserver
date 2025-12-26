use serde::{Deserialize, Serialize};
use horizon_event_system::{GorcObject, GorcObjectId, ReplicationLayer, ReplicationPriority, CompressionType, Vec3};
use uuid::Uuid;

// Define the box50cm
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Box50cm {
    pub gorc_id: Option<GorcObjectId>,
    /// 3D position in world space
    pub position: Vec3,
    pub rotation: Vec3,
    pub uuid: String,
    pub qrcode: Option<String>,
    pub symbol: Option<String>,
    pub led_state: Option<String>,
    pub opened: bool,
    pub parcel_number: Option<String>,
    pub weight: f32,
}

impl Box50cm {
    pub fn new(position: Vec3, rotation: Vec3, mut uuid: String) -> Self {
        if uuid.is_empty() {
            uuid = Uuid::new_v4().to_string();
        }        
        Self {
            gorc_id: None,
            position,
            rotation,
            uuid,
            qrcode: None,
            symbol: None,
            led_state: None,
            opened: false,
            parcel_number: None,
            weight: 0.25,
        }
    }
}

impl GorcObject for Box50cm {
    fn type_name(&self) -> &'static str {
        "box50cm"
    }

    fn position(&self) -> Vec3 {
        self.position
    }

    fn get_priority(&self, observer_pos: Vec3) -> ReplicationPriority {
        let distance = self.position.distance(observer_pos);
        if distance < 50.0 {
            ReplicationPriority::Critical
        } else if distance < 100.0 {
            ReplicationPriority::High
        } else if distance < 300.0 {
            ReplicationPriority::Normal
        } else {
            ReplicationPriority::Low
        }
    }

    fn serialize_for_layer(&self, layer: &ReplicationLayer) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut data = serde_json::Map::new();

        for property in &layer.properties {
            match property.as_str() {
                "position" => {
                    data.insert("position".to_string(), serde_json::to_value(&self.position)?);
                }
                "rotation" => {
                    data.insert("rotation".to_string(), serde_json::to_value(&self.rotation)?);
                }
                "uuid" => {
                    data.insert("uuid".to_string(), serde_json::to_value(&self.uuid)?);
                }
                "qrcode" => {
                    if let Some(qr) = &self.qrcode {
                        data.insert("qrcode".to_string(), serde_json::to_value(qr)?);
                    }
                }
                "symbol" => {
                    if let Some(sym) = &self.symbol {
                        data.insert("symbol".to_string(), serde_json::to_value(sym)?);
                    }
                }
                "led_state" => {
                    if let Some(led) = &self.led_state {
                        data.insert("led_state".to_string(), serde_json::to_value(led)?);
                    }
                }
                "opened" => {
                    data.insert("opened".to_string(), serde_json::to_value(&self.opened)?);
                }
                "parcel_number" => {
                    if let Some(parcel) = &self.parcel_number {
                        data.insert("parcel_number".to_string(), serde_json::to_value(parcel)?);
                    }
                }
                "weight" => {
                    data.insert("weight".to_string(), serde_json::to_value(&self.weight)?);
                }
                _ => {} // Ignore unknown properties
            }
        }
        Ok(serde_json::to_vec(&data)?)
    }

    fn get_layers(&self) -> Vec<ReplicationLayer> {
        vec![
            ReplicationLayer::new(
                0, 50.0, 30.0, 
                vec![
                    "position".to_string(),
                    "rotation".to_string(),
                    "uuid".to_string(),
                    "opened".to_string(),
                ],
                CompressionType::Delta,
            ),
            ReplicationLayer::new(
                1, 200.0, 15.0, 
                vec![
                    "led_state".to_string(),
                ],
                CompressionType::Lz4,
            ),
            ReplicationLayer::new(
                3, 600.0, 2.0, 
                vec![
                    "qrcode".to_string(),
                    "symbol".to_string(),
                    "parcel_number".to_string(),
                    "weight".to_string(),
                ],
                CompressionType::High,
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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::iter::Map;
use serde_json::{json, Value};
use tracing::{ debug, error };
use std::string::String;
use std::error::Error;
use std::sync::Arc;
use horizon_event_system::Vec3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectDefinition {
	pub name: String,
	//FEATURE use a biderectionnal index ?
	pub index: HashMap<String, u8>,
	pub channels: Vec<Channel>
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Channel {
	pub zone: u8,
	pub distance: f64, //overkill f32 should be enough or even i32
	pub frequency: f64 //f64 is overkill
}
impl ObjectDefinition {
	
	pub fn new(name: String, data: serde_json::Value) -> Self {
		let mut index = HashMap::<String, u8>::new();
		let mut channels = Vec::<Channel>::new();
		if data.is_object() {
			for channel in data["channels"].as_array().unwrap().iter() {
				let zone: u8 = channel["zone"].as_u64().unwrap().try_into().unwrap();
				channels.push(Channel{
					zone: zone,
					distance: channel["distance"].as_f64().unwrap(),
					frequency: channel["frequency"].as_f64().unwrap()
				});
				for property in channel["properties"].as_array().unwrap().iter() {
					if let Value::String(property_name) = property {
						index.insert(property_name.to_string(), zone);
					}
				}
			}
		}
		Self {
			name,
			index,
			channels,
		}
	}
	
	
	pub fn order_data(&self, data: serde_json::Value) -> Result<HashMap<u8, serde_json::Value>, Box<dyn Error>> {
		let mut out = HashMap::<u8, serde_json::Value>::new();
		match data {
			Value::Null => return Ok(out),
			Value::Object(map) => {
				for(key, value) in map.iter() {
					match self.index.get(key) {
						None => debug!("Property {} not indexed in object {}, skipped", key, self.name),
						Some(zone) =>  {
							if !out.contains_key(zone) {
								out.insert(*zone, json!({}));
							}
							out.get_mut(zone).expect("it should exist").as_object_mut().unwrap().insert(key.to_string(), value.clone());
						}
					}
				}
				return Ok(out)
			},
			_ => return Err("object creation error: Invalid data input".into())
		}
	}
	
	/// Get position from object definition data
	/// Priority order:
	/// 1. Check for "positions" array and return first item
	/// 2. Check for "position" value and return it
	/// 3. Return Vec3::zero() as fallback
	pub fn get_position(&self, data: &serde_json::Value) -> Vec3 {
		// Case 1: Check for "positions" array
		if let Some(positions) = data.get("positions") {
			if let Some(array) = positions.as_array() {
				if let Some(first_pos) = array.first() {
					if let Ok(position) = serde_json::from_value::<Vec3>(first_pos.clone()) {
						return position;
					}
				}
			}
		}
		
		// Case 2: Check for "position" value
		if let Some(position) = data.get("position") {
			if let Ok(pos) = serde_json::from_value::<Vec3>(position.clone()) {
				return pos;
			}
		}
		
		// Case 3: Return zero vector as fallback
		Vec3::zero()
	}
}
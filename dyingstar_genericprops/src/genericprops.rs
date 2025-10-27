use serde::{Deserialize, Serialize};
use horizon_event_system::{GorcObjectId, GorcObject, ReplicationLayer, ReplicationPriority, CompressionType, Vec3};
use horizon_event_system::GorcZoneData;
use horizon_event_system::EventError;
use uuid::Uuid;
use std::collections::HashMap;
use std::iter::Map;
use serde_json::{json, Value};
use tracing::{ debug, error };
use std::string::String;
use std::error::Error;
use std::sync::Arc;
use std::collections::HashSet;

use crate::objectdefinition::ObjectDefinition;

pub fn distance_squared(source: Vec3, other: Vec3) -> f64 {
    let dx = source.x - other.x;
    let dy = source.y - other.y;
    let dz = source.z - other.z;
    dx * dx + dy * dy + dz * dz
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenericProps {
	//pub gorc_id: Option<GorcObjectId>,
    pub object_def: ObjectDefinition,
    pub uuid: String,
	//data is zone ordered
	pub data: HashMap<u8, serde_json::Value>,
}

impl GenericProps {
    pub fn new(object_def: ObjectDefinition, data: serde_json::Value, mut uuid: String) -> Self {
        if uuid.is_empty() {
            uuid = Uuid::new_v4().to_string();
        }
		let data = object_def.order_data(data).expect("REASON");
        Self {
			object_def,
			uuid,
			data
        }
    }
	pub fn update(&mut self, new_data: serde_json::Value) -> HashSet<u8> {
		let mut out: HashSet<u8> = HashSet::new();
		match new_data {
			Value::Null => return out,
			Value::Object(map) => {
				for(key, value) in map.iter() {
					match self.object_def.index.get(key) {
						None => debug!("Property {} not indexed in object {}, skipped", key, self.uuid),
						Some(zone) =>  {
							if !self.data.contains_key(zone) {
								self.data.insert(*zone, json!({}));
								out.insert(*zone);
							}
							if self.data.get_mut(zone).expect("it should exist").as_object().unwrap().contains_key(key) {
								self.data.get_mut(zone).expect("it should exist").as_object_mut().unwrap()[key] = value.clone();
								out.insert(*zone);
							}
							else {
								self.data.get_mut(zone).expect("it should exist").as_object_mut().unwrap().insert(key.to_string(), value.clone());
								out.insert(*zone);
							}
						}
					}
				}
				return out;
			},
			_ => return out,
		}
	}
}

impl GorcObject for GenericProps {
	fn type_name(&self) -> &'static str {
        "genericprops"
    }

    fn position(&self) -> Vec3 {
        //self.position
		if let Some(zone) = self.object_def.index.get("position") {
			 serde_json::from_value::<Vec3>(self.data[zone].get("position").unwrap().clone()).expect("Object should have position")
		}
		else {
			Vec3::zero()
		}
    }
	
	fn get_priority(&self, observer_pos: Vec3) -> ReplicationPriority {
		// use ² distance comparaison do gain on cpu time, sqrt is slow
		let distance_squared = distance_squared(self.position(),observer_pos);
		match distance_squared {
			d if d < (100.0)*(100.0) => ReplicationPriority::Critical,
			d if d < (300.0)*(300.0) => ReplicationPriority::High,
			d if d < (1000.0)*(1000.0) => ReplicationPriority::Normal,
			_ => ReplicationPriority::Low,
		}
	}
	
	fn serialize_for_layer(&self, layer: &ReplicationLayer) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
		if !self.data.contains_key(&layer.channel) {
			return Err("Invalid channel for this object type".into());
		}
		Ok(serde_json::to_vec(&self.data[&layer.channel])?)
	}
	
	fn get_layers(&self) -> Vec<ReplicationLayer> {
		//TODO make a channels type with a into<Vec>() impl
		let mut layers = Vec::new();
		for channel in self.object_def.channels.iter() {
			layers.push(
				ReplicationLayer::new(
					channel.zone,
					channel.distance,
					channel.frequency.into(),
					vec![],// Field no longer use since 0.40
					match channel.zone {
						0 => CompressionType::Delta,
						1 | 2 => CompressionType::Lz4,
						3 | _ => CompressionType::High
					}
				)
			)
		}
		layers
    }
	
	//TODO found why this trait exist ; use by	event handlers but why it's in trait ?
	fn update_position(&mut self, new_position: Vec3) {
        //self.position = new_position
		self.update(json!({"position":new_position}));
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
	
	fn on_register(&mut self, object_id: GorcObjectId) {
        let _ = object_id; // Default implementation does nothing
    }
}
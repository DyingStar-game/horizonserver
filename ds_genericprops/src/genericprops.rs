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
	pub global_position: Vec3,
	//data is zone ordered
	pub data: HashMap<u8, serde_json::Value>,
}

impl GenericProps {
    pub fn new(object_def: ObjectDefinition, data: serde_json::Value, mut uuid: String) -> Self {
        if uuid.is_empty() {
            uuid = Uuid::new_v4().to_string();
        }
		let data = object_def.order_data(data).expect("REASON");
        let this = Self {
			object_def,
			uuid,
			global_position: Vec3::zero(),
			data
        };
		crate::children::set_parent(&this.uuid, this.parent_id().as_deref());
		this
    }

	/// First non-empty `parent_id` across channels.
	pub fn parent_id(&self) -> Option<String> {
		self.data
			.values()
			.filter_map(|zone_data| zone_data.get("parent_id"))
			.filter_map(|v| v.as_str())
			.find(|s| !s.is_empty())
			.map(|s| s.to_string())
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
				// The index is keyed by uuid, so applying the same payload to a
				// clone of this instance lands on the same entry: harmless.
				if map.contains_key("parent_id") {
					crate::children::set_parent(&self.uuid, self.parent_id().as_deref());
				}
				return out;
			},
			_ => return out,
		}
	}
	pub fn get_data_for_layer(&self, layer: &ReplicationLayer) -> Option<serde_json::Value> {
		self.data.get(&layer.channel).cloned()
	}
}

impl GorcObject for GenericProps {
	fn type_name(&self) -> &str {
        self.object_def.name.as_str()
    }

    fn position(&self) -> Vec3 {
		self.global_position
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

	/// JSON clients get the channel's object as before; bitcode clients a
	/// `ds_protocol::PropState`: position and rotation apart, the other keys as a
	/// `DynValue` (the shape `lod` uses for later updates).
	fn serialize_for_layer_as(
		&self,
		layer: &ReplicationLayer,
		format: horizon_event_system::Format,
	) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
		match format {
			horizon_event_system::Format::Json => self.serialize_for_layer(layer),
			horizon_event_system::Format::Bitcode => {
				let data = self.data.get(&layer.channel).ok_or("Invalid channel for this object type")?;
				let mut state = prop_state(data);
				if let Ok(gorc_id) = GorcObjectId::from_str(&self.uuid) {
					state.transform.epoch = crate::lod::epoch(gorc_id);
				}
				Ok(horizon_event_system::protocol::bitcode::encode(&state))
			}
		}
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
					// Ignored by Horizon: compression is set per listener ([server.compression]).
					CompressionType::None
				)
			)
		}
		layers
    }
	
	fn update_position(&mut self, new_position: Vec3) {
		self.global_position = new_position;
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

/// A channel's data as a bitcode `PropState`. Data that is not a JSON object goes
/// whole into the other keys.
pub fn prop_state(data: &serde_json::Value) -> ds_protocol::PropState {
	match data.as_object() {
		Some(map) => {
			let (transform, rest) = ds_protocol::props::split(map);
			ds_protocol::PropState { transform, data: ds_protocol::DynValue::from(&serde_json::Value::Object(rest)) }
		}
		None => ds_protocol::PropState { transform: Default::default(), data: ds_protocol::DynValue::from(data) },
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn player_def() -> ObjectDefinition {
		let path = format!("{}/props/player_def.json", env!("CARGO_MANIFEST_DIR"));
		ObjectDefinition::new("player".to_string(), serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
	}

	/// A player's look (the game's CharacterAppearance string) rides a slow channel of its own: it
	/// changes a few times a session, and on the 30 Hz movement channel its ~100 bytes would go out with
	/// every snapshot of every player.
	#[test]
	fn a_players_appearance_is_its_own_slow_channel() {
		let def = player_def();
		assert_eq!(def.index.get("appearance"), Some(&1));
		assert_eq!(def.index.get("position"), Some(&0));
		let channel = def.channel(1).expect("zone 1");
		assert_eq!(channel.frequency, 1.0);
		assert_eq!(channel.distance, 200.0, "seen as far as the player is");
	}

	#[test]
	fn a_spawned_player_carries_its_appearance_on_zone_1() {
		let player = GenericProps::new(player_def(), json!({
			"position": { "x": 1.0, "y": 2.0, "z": 3.0 },
			"name": "bob",
			"appearance": "AQIDBA",
		}), "p1".to_string());
		assert_eq!(player.data[&1]["appearance"], "AQIDBA");
		assert!(player.data[&0].get("appearance").is_none());
		assert_eq!(player.data[&0]["name"], "bob");
	}

	#[test]
	fn a_new_appearance_dirties_zone_1_alone() {
		let mut player = GenericProps::new(player_def(), json!({ "position": { "x": 0.0, "y": 0.0, "z": 0.0 } }), "p1".to_string());
		let dirty = player.update(json!({ "appearance": "AQIDBA" }));
		assert_eq!(dirty, HashSet::from([1u8]), "the movement channel is not resent for a new look");
		let dirty = player.update(json!({ "head": 0.3 }));
		assert_eq!(dirty, HashSet::from([0u8]), "and a movement-channel property does not resend the look");
	}
}

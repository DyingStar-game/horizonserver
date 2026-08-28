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
	pub frequency: f64, //f64 is overkill
	/// Outbound rate ladder, innermost tier first.
	///
	/// `distance` stays the single subscription radius: GORC still knows only
	/// "inside the sphere or not". This ladder refines *how often* a subscriber
	/// is served once inside, based on how far it actually is. Built from the
	/// optional `lod` array of the definition file and never empty - a channel
	/// without `lod` collapses to a single tier at (`distance`, `frequency`),
	/// which is the plain "this channel runs at N Hz" case.
	pub lod: Vec<LodTier>
}

/// One rung of a channel's rate ladder: subscribers within `distance` of the
/// object are served at most `frequency` times per second.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct LodTier {
	pub distance: f64,
	pub frequency: f64
}
impl ObjectDefinition {

	/// Reads the optional `lod` array of a channel into a sorted, non-empty ladder.
	///
	/// Malformed tiers are dropped rather than fatal: a typo in one rung must not
	/// take the whole object type offline, and the (`distance`, `frequency`) pair
	/// is always a valid fallback.
	fn parse_lod(raw: &Value, distance: f64, frequency: f64, name: &str, zone: u8) -> Vec<LodTier> {
		let mut tiers: Vec<LodTier> = raw
			.as_array()
			.map(|entries| {
				entries
					.iter()
					.filter_map(|tier| {
						let d = tier.get("distance").and_then(Value::as_f64);
						let f = tier.get("frequency").and_then(Value::as_f64);
						match (d, f) {
							(Some(d), Some(f)) if d > 0.0 && f > 0.0 => Some(LodTier { distance: d, frequency: f }),
							_ => {
								error!("Object {} channel {}: ignoring invalid lod tier {}", name, zone, tier);
								None
							}
						}
					})
					.collect()
			})
			.unwrap_or_default();

		if tiers.is_empty() {
			tiers.push(LodTier { distance, frequency });
		}
		// The ladder is walked innermost-first, so ordering is a correctness
		// requirement, not a cosmetic one.
		tiers.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap_or(std::cmp::Ordering::Equal));
		tiers
	}

	pub fn new(name: String, data: serde_json::Value) -> Self {
		let mut index = HashMap::<String, u8>::new();
		let mut channels = Vec::<Channel>::new();
		if data.is_object() {
			for channel in data["channels"].as_array().unwrap().iter() {
				let zone: u8 = channel["zone"].as_u64().unwrap().try_into().unwrap();
				let distance = channel["distance"].as_f64().unwrap();
				let frequency = channel["frequency"].as_f64().unwrap();
				channels.push(Channel{
					zone: zone,
					distance: distance,
					frequency: frequency,
					lod: Self::parse_lod(&channel["lod"], distance, frequency, &name, zone)
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
	
	
	/// Look up a channel by its zone number.
	pub fn channel(&self, zone: u8) -> Option<&Channel> {
		self.channels.iter().find(|channel| channel.zone == zone)
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
#[cfg(test)]
mod tests {
	use super::*;

	fn definition(channels: serde_json::Value) -> ObjectDefinition {
		ObjectDefinition::new("miningrock".to_string(), json!({ "channels": channels }))
	}

	#[test]
	fn channel_without_lod_collapses_to_a_single_tier() {
		let def = definition(json!([
			{ "zone": 0, "distance": 200.0, "frequency": 30.0, "properties": ["position"] }
		]));
		let tiers = &def.channel(0).unwrap().lod;

		assert_eq!(tiers.len(), 1);
		assert_eq!(tiers[0].distance, 200.0);
		assert_eq!(tiers[0].frequency, 30.0);
	}

	#[test]
	fn lod_tiers_are_ordered_innermost_first() {
		// Declared outermost first on purpose: the delivery loop walks the ladder
		// in order, so parsing has to sort rather than trust the file.
		let def = definition(json!([
			{
				"zone": 0, "distance": 200.0, "frequency": 30.0,
				"lod": [
					{ "distance": 200.0, "frequency": 10.0 },
					{ "distance": 100.0, "frequency": 30.0 }
				],
				"properties": ["position"]
			}
		]));
		let tiers = &def.channel(0).unwrap().lod;

		assert_eq!(tiers.len(), 2);
		assert_eq!(tiers[0].distance, 100.0);
		assert_eq!(tiers[0].frequency, 30.0);
		assert_eq!(tiers[1].distance, 200.0);
		assert_eq!(tiers[1].frequency, 10.0);
	}

	#[test]
	fn malformed_tiers_are_dropped_not_fatal() {
		let def = definition(json!([
			{
				"zone": 0, "distance": 200.0, "frequency": 30.0,
				"lod": [
					{ "distance": 100.0, "frequency": 30.0 },
					{ "distance": 150.0 },
					{ "distance": 200.0, "frequency": 0.0 }
				],
				"properties": ["position"]
			}
		]));
		let tiers = &def.channel(0).unwrap().lod;

		assert_eq!(tiers.len(), 1);
		assert_eq!(tiers[0].distance, 100.0);
	}

	#[test]
	fn a_fully_invalid_ladder_falls_back_to_the_channel_rate() {
		let def = definition(json!([
			{
				"zone": 0, "distance": 200.0, "frequency": 30.0,
				"lod": [{ "nope": true }],
				"properties": ["position"]
			}
		]));
		let tiers = &def.channel(0).unwrap().lod;

		assert_eq!(tiers.len(), 1);
		assert_eq!(tiers[0].distance, 200.0);
		assert_eq!(tiers[0].frequency, 30.0);
	}

	#[test]
	fn channel_lookup_is_by_zone_number_not_position() {
		let def = definition(json!([
			{ "zone": 0, "distance": 200.0, "frequency": 30.0, "properties": ["position"] },
			{ "zone": 6, "distance": 203.0, "frequency": 3.0, "properties": ["scenename"] }
		]));

		assert_eq!(def.channel(6).unwrap().frequency, 3.0);
		assert!(def.channel(1).is_none());
	}
}

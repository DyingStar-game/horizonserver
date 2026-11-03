//! Godot Variants to JSON values and back, as `JSON.stringify` and `JSON.parse_string`
//! did on the WebSocket link, so the GDScript handlers see the same values.
//!
//! - Inbound, every number is a float, as `JSON.parse_string` gives them.
//! - Outbound, a float holding an integer becomes a JSON integer, so it reads into the
//!   integer fields of `ds_protocol` (`seq`, `channel`); a value JSON has no type for
//!   (Vector3, StringName...) becomes its text, as `JSON.stringify` writes it.

use godot::prelude::*;
use serde_json::{Map, Number, Value};

/// Largest integer a float holds exactly (2^53).
const EXACT_INT: f64 = 9_007_199_254_740_992.0;

/// A Variant as JSON.
pub fn to_json(variant: &Variant) -> Value {
    match variant.get_type() {
        VariantType::NIL => Value::Null,
        VariantType::BOOL => Value::Bool(variant.to::<bool>()),
        VariantType::INT => Value::Number(variant.to::<i64>().into()),
        VariantType::FLOAT => float(variant.to::<f64>()),
        VariantType::STRING => Value::String(variant.to::<GString>().to_string()),
        VariantType::DICTIONARY => {
            let dictionary = variant.to::<VarDictionary>();
            let mut map = Map::new();
            for (key, value) in dictionary.iter_shared() {
                let key = if key.get_type() == VariantType::STRING {
                    key.to::<GString>().to_string()
                } else {
                    key.stringify().to_string()
                };
                map.insert(key, to_json(&value));
            }
            Value::Object(map)
        }
        VariantType::ARRAY => Value::Array(
            variant
                .to::<VarArray>()
                .iter_shared()
                .map(|v| to_json(&v))
                .collect(),
        ),
        _ => Value::String(variant.stringify().to_string()),
    }
}

fn float(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < EXACT_INT {
        Value::Number((value as i64).into())
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// A JSON value as a Variant.
pub fn to_variant(value: &Value) -> Variant {
    match value {
        Value::Null => Variant::nil(),
        Value::Bool(b) => b.to_variant(),
        Value::Number(n) => n.as_f64().unwrap_or(0.0).to_variant(),
        Value::String(s) => GString::from(s.as_str()).to_variant(),
        Value::Array(items) => {
            let mut array = VarArray::new();
            for item in items {
                array.push(&to_variant(item));
            }
            array.to_variant()
        }
        Value::Object(map) => to_dictionary(map).to_variant(),
    }
}

/// A JSON object as a Dictionary with String keys.
pub fn to_dictionary(map: &Map<String, Value>) -> VarDictionary {
    let mut dictionary = VarDictionary::new();
    for (key, value) in map {
        dictionary.set(
            &GString::from(key.as_str()).to_variant(),
            &to_variant(value),
        );
    }
    dictionary
}

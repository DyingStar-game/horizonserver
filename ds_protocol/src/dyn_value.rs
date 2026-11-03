//! A JSON value that bitcode can carry.
//!
//! Most DyingStar payloads are schema-less: the keys of a prop come from its
//! `*_def.json`, an action carries whatever the client put in it. bitcode's serde
//! mode cannot encode a `serde_json::Value` (it is not self-describing), and its
//! derive does not accept recursive types. [`DynValue`] stores the value as a flat
//! list of tokens, in depth-first order, which the derive accepts.
//!
//! In JSON a `DynValue` is the plain JSON value, so a JSON client sees no
//! difference. In bitcode it must go through the derive
//! (`horizon_protocol::payload::encode_packed` / `decode_packed`), never through
//! bitcode's serde mode.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};

/// One node of a [`DynValue`], depth first. An `Array` is followed by its items, an
/// `Object` by its entries, each a `Str` key then the value.
#[derive(Debug, Clone, PartialEq, bitcode::Encode, bitcode::Decode)]
pub enum Token {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
    Str(String),
    /// An array of that many items.
    Array(u32),
    /// An object of that many entries.
    Object(u32),
}

/// A JSON value encoded for bitcode. See the module docs.
#[derive(Debug, Clone, PartialEq, bitcode::Encode, bitcode::Decode)]
pub struct DynValue {
    tokens: Vec<Token>,
}

/// Null.
impl Default for DynValue {
    fn default() -> Self {
        DynValue { tokens: vec![Token::Null] }
    }
}

/// A [`DynValue`] whose tokens do not form one value.
#[derive(Debug, Clone, PartialEq)]
pub struct MalformedDynValue;

impl std::fmt::Display for MalformedDynValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed DynValue")
    }
}

impl std::error::Error for MalformedDynValue {}

impl DynValue {
    /// The JSON value, or an error when the tokens are not exactly one value
    /// (a frame from a broken or hostile client).
    pub fn to_value(&self) -> Result<Value, MalformedDynValue> {
        let mut index = 0;
        let value = read(&self.tokens, &mut index, 0)?;
        if index != self.tokens.len() {
            return Err(MalformedDynValue);
        }
        Ok(value)
    }

    /// The JSON value, `Null` when malformed.
    pub fn into_value(self) -> Value {
        self.to_value().unwrap_or(Value::Null)
    }
}

impl From<&Value> for DynValue {
    fn from(value: &Value) -> Self {
        let mut tokens = Vec::new();
        write(value, &mut tokens);
        DynValue { tokens }
    }
}

impl From<Value> for DynValue {
    fn from(value: Value) -> Self {
        DynValue::from(&value)
    }
}

fn write(value: &Value, tokens: &mut Vec<Token>) {
    match value {
        Value::Null => tokens.push(Token::Null),
        Value::Bool(b) => tokens.push(Token::Bool(*b)),
        Value::Number(n) => tokens.push(if let Some(u) = n.as_u64() {
            Token::U64(u)
        } else if let Some(i) = n.as_i64() {
            Token::I64(i)
        } else {
            Token::F64(n.as_f64().unwrap_or(0.0))
        }),
        Value::String(s) => tokens.push(Token::Str(s.clone())),
        Value::Array(items) => {
            tokens.push(Token::Array(items.len() as u32));
            for item in items {
                write(item, tokens);
            }
        }
        Value::Object(map) => {
            tokens.push(Token::Object(map.len() as u32));
            for (key, item) in map {
                tokens.push(Token::Str(key.clone()));
                write(item, tokens);
            }
        }
    }
}

/// Nesting deeper than this is refused, so a hostile frame cannot overflow the stack.
const MAX_DEPTH: usize = 64;

fn read(tokens: &[Token], index: &mut usize, depth: usize) -> Result<Value, MalformedDynValue> {
    if depth > MAX_DEPTH {
        return Err(MalformedDynValue);
    }
    let token = tokens.get(*index).ok_or(MalformedDynValue)?;
    *index += 1;
    Ok(match token {
        Token::Null => Value::Null,
        Token::Bool(b) => Value::Bool(*b),
        Token::U64(u) => Value::Number((*u).into()),
        Token::I64(i) => Value::Number((*i).into()),
        // JSON has no NaN or infinity: such a float becomes null, as serde_json does.
        Token::F64(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Token::Str(s) => Value::String(s.clone()),
        Token::Array(len) => {
            let len = *len as usize;
            // Every item takes at least one token: bounds the allocation.
            if len > tokens.len() - *index {
                return Err(MalformedDynValue);
            }
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(read(tokens, index, depth + 1)?);
            }
            Value::Array(items)
        }
        Token::Object(len) => {
            let len = *len as usize;
            if len.saturating_mul(2) > tokens.len() - *index {
                return Err(MalformedDynValue);
            }
            let mut map = Map::new();
            for _ in 0..len {
                let Some(Token::Str(key)) = tokens.get(*index) else {
                    return Err(MalformedDynValue);
                };
                *index += 1;
                let item = read(tokens, index, depth + 1)?;
                map.insert(key.clone(), item);
            }
            Value::Object(map)
        }
    })
}

impl Serialize for DynValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().map_err(serde::ser::Error::custom)?.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DynValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(DynValue::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "action": "seat",
            "seat": {"vehicle": "6f1c0b9a-9d1e-4a57-9b7e-3f0d6c2a1b11", "index": 2},
            "tools": [1, -2, 3.5, true, null, "drill", [], {}],
            "big": u64::MAX,
            "neg": i64::MIN,
            "nested": {"a": {"b": {"c": [[1], [2, {"d": "e"}]]}}}
        })
    }

    #[test]
    fn json_value_round_trips_through_tokens() {
        let value = sample();
        assert_eq!(DynValue::from(&value).to_value().unwrap(), value);
    }

    #[test]
    fn json_value_round_trips_through_bitcode() {
        let value = sample();
        let bytes = bitcode::encode(&DynValue::from(&value));
        let decoded: DynValue = bitcode::decode(&bytes).unwrap();
        assert_eq!(decoded.to_value().unwrap(), value);
    }

    #[test]
    fn json_form_is_the_plain_value() {
        let value = sample();
        let dyn_value = DynValue::from(&value);
        assert_eq!(serde_json::to_vec(&dyn_value).unwrap(), serde_json::to_vec(&value).unwrap());
        let back: DynValue = serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(back, dyn_value);
    }

    #[test]
    fn malformed_tokens_are_refused_not_panicking() {
        let cases = vec![
            vec![],
            vec![Token::Array(3), Token::Null],
            vec![Token::Object(1), Token::Null, Token::Null],
            vec![Token::Object(u32::MAX)],
            vec![Token::Null, Token::Null],
            std::iter::repeat_n(Token::Array(1), MAX_DEPTH + 2).chain([Token::Null]).collect(),
        ];
        for tokens in cases {
            assert_eq!(DynValue { tokens }.to_value(), Err(MalformedDynValue));
        }
    }

    #[test]
    fn default_is_null() {
        assert_eq!(DynValue::default().to_value().unwrap(), Value::Null);
    }

    #[test]
    fn non_finite_floats_become_null() {
        let dyn_value = DynValue { tokens: vec![Token::F64(f64::NAN)] };
        assert_eq!(dyn_value.to_value().unwrap(), Value::Null);
    }
}

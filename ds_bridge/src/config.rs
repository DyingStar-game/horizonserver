use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;

/// Envelope used in both directions over the WebSocket connection.
///
/// Outgoing (Horizon → external service):
///   The bridge wraps a subscribed Horizon event into this envelope and sends
///   it as a JSON text frame.
///
/// Incoming (external service → Horizon):
///   The external service sends an envelope; the bridge deserialises it and
///   re-emits the event into the Horizon event system using the same fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeEventEnvelope {
    /// "core" or "plugin"
    pub event_type: String,
    /// Plugin namespace when event_type == "plugin", otherwise null / None
    pub namespace: Option<String>,
    /// Event name (e.g. "player_connected" or "new_player")
    pub name: String,
    /// Arbitrary JSON payload — the original event data
    pub payload: serde_json::Value,
}

/// Configuration for a single external WebSocket service.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Human-readable name used in log messages
    pub name: String,
    /// WebSocket URL to connect to (e.g. "ws://scoring-service:8080")
    pub url: String,
    /// List of Horizon event keys to subscribe and forward.
    /// Format: "core:<event_name>" or "plugin:<plugin_name>:<event_name>"
    pub subscribe: Vec<String>,
}

/// Top-level bridge configuration loaded from plugins.toml.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub log_level: String,
    pub services: Vec<ServiceConfig>,
}

impl BridgeConfig {
    /// Load from `plugins.toml` via `ds_common::Config`.
    pub fn load() -> Self {
        let config = ds_common::config::Config::new("ds_bridge");

        let log_level = config
            .get_value("log_level")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string();

        let services = match config.get_value("services") {
            None => vec![],
            Some(TomlValue::Array(arr)) => arr
                .iter()
                .filter_map(|entry| {
                    let table = entry.as_table()?;
                    let name = table.get("name")?.as_str()?.to_string();
                    let url = table.get("url")?.as_str()?.to_string();
                    let subscribe = table
                        .get("subscribe")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| s.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(ServiceConfig { name, url, subscribe })
                })
                .collect(),
            _ => vec![],
        };

        BridgeConfig { log_level, services }
    }
}

# gRPC Bridge Library for Horizon Event System

This library provides a **bridge plugin** to connect the [Horizon Event System](https://github.com/your-org/horizon_event_system) with a **gRPC server**.  
It allows you to forward Horizon events (client, core, plugin) to a remote gRPC service, and receive responses or additional events back.

---

## ✨ Features
- Connect any Horizon plugin to a gRPC server
- Forward **client**, **core**, and **plugin** events
- Receive responses and trigger new Horizon events from the gRPC server
- Built-in **Tokio runtime** with configurable worker threads
- Optional **health checks** to keep the connection alive
- Flexible configuration via builder pattern

---

## ⚡ Installation

In your `Cargo.toml`:

```toml
[dependencies]
grpc-bridge-lib = { path = "../grpc-bridge-lib" } # or from crates.io if published
horizon-event-system = { path = "../horizon-event-system" }
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
```

🚀 Usage Example

Here’s how to use the bridge in a plugin:


```rust
// bridge_grpc_plugin/src/lib.rs
use grpc_bridge_lib::{create_grpc_plugin, GrpcBridgeConfig};
use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, PluginError, ServerContext, SimplePlugin
};

pub struct BridgeGrpcPlugin {
    inner: Box<dyn SimplePlugin>,
}

impl BridgeGrpcPlugin {
    pub fn new() -> Self {
        let config = GrpcBridgeConfig::new("bridge_grpc_plugin", "http://172.22.0.1:50051")
            .with_version("2.4.0")
            .with_health_check(true, 30)
            .with_client_events(vec![
                ("game", "custom_action"),
                ("player", "init"),
                ("player", "position"),
                ("box50cm", "spawn"),
            ])
            .with_core_events(vec![
                "player_connected",
                "player_disconnected",
            ])
            .with_plugin_events(vec![
                ("gameserverplugin", "init_server"),
                ("gameserverplugin", "send_props"),
            ])
            .with_buffer_size(100)
            .with_worker_threads(2);

        Self {
            inner: create_grpc_plugin(config),
        }
    }
}


#[async_trait]
impl SimplePlugin for BridgeGrpcPlugin {
    fn name(&self) -> &str { self.inner.name() }
    fn version(&self) -> &str { self.inner.version() }

    async fn register_handlers(
        &mut self,
        events: std::sync::Arc<horizon_event_system::EventSystem>,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), PluginError> {
        self.inner.register_handlers(events, context).await
    }

    async fn on_init(
        &mut self,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), PluginError> {
        self.inner.on_init(context).await
    }

    async fn on_shutdown(
        &mut self,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), PluginError> {
        self.inner.on_shutdown(context).await
    }
}

create_simple_plugin!(BridgeGrpcPlugin);
```

⚙️ Configuration

The GrpcBridgeConfig struct lets you fully configure the plugin:

``` rust
let config = GrpcBridgeConfig::new("bridge_grpc_plugin", "http://localhost:50051")
    .with_version("1.0.0")
    .with_health_check(true, 30)        // enable health checks
    .with_client_events(vec![("player", "init")])
    .with_core_events(vec!["player_connected"])
    .with_plugin_events(vec![("gameserverplugin", "send_props")])
    .with_buffer_size(200)              // channel buffer size
    .with_worker_threads(4);            // tokio runtime threads
```

📡 Event Model

The library exchanges events with the gRPC server using a JSON structure similar to:

```json
{
  "category": "client",             // "client", "core", or "plugin"
  "namespace": "player",            // required for client events
  "plugin": "gameserverplugin",     // required for plugin events
  "event": "init",
  "data": { "id": "1234" },
  "timestamp": 1699969200
}
```

Responses from the gRPC server may include new events to trigger:
```json
{
  "trigger_events": [
    {
      "type": "spawn",
      "category": "client",
      "namespace": "npc",
      "data": { "npc_type": "goblin" }
    }
  ]
}
```
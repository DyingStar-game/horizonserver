// bridge_grpc_plugin/src/lib.rs
use grpc_bridge_lib::{create_grpc_plugin, GrpcBridgeConfig};
use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin,PluginError, ServerContext, SimplePlugin
};
use std::sync::Arc;


pub struct BridgeGrpcPlugin {
    inner: Box<dyn SimplePlugin>,
}

impl BridgeGrpcPlugin {
    pub fn new() -> Self {
        let config = GrpcBridgeConfig::new("bridge_grpc_plugin", "http://172.22.0.1:50051")
            .with_version("2.4.0")
            .with_health_check(true, 30)
            .with_client_events(vec![
                ("chat", "message"), ("chat", "whisper"),
                ("movement", "update"), ("movement", "jump"), ("movement", "teleport"),
                ("combat", "attack"), ("combat", "cast_spell"), ("combat", "take_damage"),
                ("inventory", "use_item"), ("inventory", "drop_item"), ("inventory", "pickup_item"),
                ("trade", "initiate"), ("trade", "accept"), ("trade", "cancel"),
                ("ui", "open_menu"), ("ui", "close_menu"),
                ("game", "custom_action"), ("player", "init"), ("player", "position"),
                ("box50cm", "spawn"),
            ])
            .with_core_events(vec![
                "player_connected", "player_disconnected",
                "region_started", "region_stopped",
                "plugin_loaded", "plugin_unloaded",
                "server_starting", "server_stopping",
            ])
            .with_plugin_events(vec![
                ("propsplugin", "new_player"), ("propsplugin", "player_position_update"),
                ("gameserverplugin", "init_server"), ("gameserverplugin", "send_props"),
            ])
            .with_buffer_size(100)
            .with_worker_threads(2);

            Self {
                inner: create_grpc_plugin(config),
            }
    }
}

#[async_trait::async_trait]
impl SimplePlugin for BridgeGrpcPlugin {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn version(&self) -> &str {
        self.inner.version()
    }

    async fn register_handlers(
        &mut self,
        events: std::sync::Arc<horizon_event_system::EventSystem>,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), horizon_event_system::PluginError> {
        self.inner.register_handlers(events, context).await
    }

    async fn on_init(
        &mut self,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), horizon_event_system::PluginError> {
        self.inner.on_init(context).await
    }

    async fn on_shutdown(
        &mut self,
        context: std::sync::Arc<dyn horizon_event_system::ServerContext>,
    ) -> Result<(), horizon_event_system::PluginError> {
        self.inner.on_shutdown(context).await
    }
}

create_simple_plugin!(BridgeGrpcPlugin);
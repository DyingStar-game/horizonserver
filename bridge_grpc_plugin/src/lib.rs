use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, EventSystem, LogLevel, PluginError, ServerContext, SimplePlugin, EventError
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info};

mod grpc;
use crate::grpc::client::GrpcBridge;

// Configuration
const GRPC_ENDPOINT: &str = "http://172.22.0.1:50051";
const ENABLE_HEALTH_CHECK: bool = true;
const HEALTH_CHECK_INTERVAL_SECONDS: u64 = 30;

// Événements
const CLIENT_EVENTS: &[(&str, &str)] = &[
    ("chat", "message"), ("chat", "whisper"),
    ("movement", "update"), ("movement", "jump"), ("movement", "teleport"),
    ("combat", "attack"), ("combat", "cast_spell"), ("combat", "take_damage"),
    ("inventory", "use_item"), ("inventory", "drop_item"), ("inventory", "pickup_item"),
    ("trade", "initiate"), ("trade", "accept"), ("trade", "cancel"),
    ("ui", "open_menu"), ("ui", "close_menu"),
    ("game", "custom_action"), ("player", "init"), ("player", "position"),
    ("box50cm", "spawn"),
];

const CORE_EVENTS: &[&str] = &[
    "player_connected", "player_disconnected",
    "region_started", "region_stopped",
    "plugin_loaded", "plugin_unloaded",
    "server_starting", "server_stopping",
];

const PLUGIN_EVENTS: &[(&str, &str)] = &[
    ("propsplugin", "new_player"), ("propsplugin", "player_position_update"),
    ("gameserverplugin", "init_server"), ("gameserverplugin", "send_props"),
];

#[derive(Debug, Clone)]
struct EventMessage {
    category: String,
    namespace: Option<String>,
    plugin: Option<String>,
    event: String,
    data: Value,
    timestamp: u64,
}

/// Worker gRPC avec runtime Tokio
struct GrpcWorker {
    grpc_bridge: Arc<RwLock<Option<GrpcBridge>>>,
    is_running: Arc<RwLock<bool>>,
}

impl GrpcWorker {
    fn spawn(mut receiver: mpsc::Receiver<EventMessage>) -> Arc<Self> {
        let grpc_bridge = Arc::new(RwLock::new(None));
        let is_running = Arc::new(RwLock::new(true));

        let worker = Arc::new(Self {
            grpc_bridge: Arc::clone(&grpc_bridge),
            is_running: Arc::clone(&is_running),
        });

        let worker_clone = Arc::clone(&worker);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("Failed to build Tokio runtime");

            rt.block_on(async move {
                // Connexion gRPC
                match GrpcBridge::connect(GRPC_ENDPOINT).await {
                    Ok(bridge) => {
                        *grpc_bridge.write().await = Some(bridge.clone());
                        info!("gRPC bridge initialized successfully");

                        // Appel initialize côté serveur Go
                        match bridge.initialize().await {
                            Ok(msg) => info!("Bridge initialized on server: {}", msg),
                            Err(e) => error!("Failed to initialize bridge on server: {}", e),
                        }

                        // Health check périodique
                        if ENABLE_HEALTH_CHECK {
                            let bridge_clone = bridge.clone();
                            let is_running_clone = Arc::clone(&is_running);
                            tokio::spawn(async move {
                                let mut interval = tokio::time::interval(
                                    tokio::time::Duration::from_secs(HEALTH_CHECK_INTERVAL_SECONDS)
                                );
                                loop {
                                    interval.tick().await;
                                    if !*is_running_clone.read().await { break; }

                                    match bridge_clone.health_check().await {
                                        Ok(status) => debug!("Health check OK: {}", status),
                                        Err(e) => error!("Health check failed: {}", e),
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => error!("Failed to connect to gRPC server: {}", e),
                }

                // Boucle d'événements
                while *is_running.read().await {
                    if let Some(event_msg) = receiver.recv().await {
                        if let Some(bridge) = grpc_bridge.read().await.as_ref() {
                            let mut event_data = json!({
                                "category": event_msg.category,
                                "event": event_msg.event,
                                "data": event_msg.data,
                                "timestamp": event_msg.timestamp,
                            });
                            if let Some(ns) = event_msg.namespace {
                                event_data["namespace"] = json!(ns);
                            }
                            if let Some(plugin) = event_msg.plugin {
                                event_data["plugin"] = json!(plugin);
                            }

                            if let Err(e) = bridge.forward_event(event_data).await {
                                error!("Failed to forward event: {}", e);
                            } else {
                                debug!("Event forwarded successfully");
                            }
                        }
                    }
                }

                // Shutdown côté serveur Go
                if let Some(bridge) = grpc_bridge.read().await.as_ref() {
                    match bridge.shutdown().await {
                        Ok(msg) => info!("Bridge shutdown on server: {}", msg),
                        Err(e) => error!("Failed to shutdown bridge on server: {}", e),
                    }
                }
            });
        });

        worker_clone
    }
}

/// Plugin principal
pub struct BridgeGrpcPluginPlugin {
    name: String,
    event_sender: Option<mpsc::Sender<EventMessage>>,
}

impl BridgeGrpcPluginPlugin {
    pub fn new() -> Self {
        info!("BridgeGrpcPlugin: Creating universal bridge to {}", GRPC_ENDPOINT);
        Self {
            name: "bridge_grpc_plugin".to_string(),
            event_sender: None,
        }
    }

    fn start_event_processor(&mut self) -> mpsc::Sender<EventMessage> {
        let (sender, receiver) = mpsc::channel::<EventMessage>(100);
        self.event_sender = Some(sender.clone());

        // Spawn worker gRPC avec runtime Tokio
        GrpcWorker::spawn(receiver);

        sender
    }

    async fn register_client_handlers(
        &self,
        events: Arc<EventSystem>,
        sender: mpsc::Sender<EventMessage>,
    ) -> Result<(), PluginError> {
        for (namespace, event_type) in CLIENT_EVENTS {
            let sender = sender.clone();
            let ns = namespace.to_string();
            let et = event_type.to_string();

            let handler = move |data: Value| -> Result<(), EventError> {
                let event_msg = EventMessage {
                    category: "client".to_string(),
                    namespace: Some(ns.clone()),
                    plugin: None,
                    event: et.clone(),
                    data,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                };
                sender.try_send(event_msg).map_err(|e| {
                    error!("Failed to send event: {}", e);
                    EventError::HandlerExecution(format!("Failed to send event: {}", e))
                })?;
                Ok(())
            };

            events.on_client(namespace, event_type, handler).await.map_err(|e| {
                PluginError::ExecutionError(format!("Failed to register client handler {}.{}: {:?}", namespace, event_type, e))
            })?;
        }

        Ok(())
    }

    async fn register_core_handlers(
        &self,
        events: Arc<EventSystem>,
        sender: mpsc::Sender<EventMessage>,
    ) -> Result<(), PluginError> {
        for event_type in CORE_EVENTS {
            let sender = sender.clone();
            let et = event_type.to_string();
            let handler = move |data: Value| -> Result<(), EventError> {
                let event_msg = EventMessage {
                    category: "core".to_string(),
                    namespace: None,
                    plugin: None,
                    event: et.clone(),
                    data,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                };
                sender.try_send(event_msg).map_err(|e| {
                    error!("Failed to send event: {}", e);
                    EventError::HandlerExecution(format!("Failed to send event: {}", e))
                })?;
                Ok(())
            };
            events.on_core(event_type, handler).await.map_err(|e| {
                PluginError::ExecutionError(format!("Failed to register core handler {}: {:?}", event_type, e))
            })?;
        }

        Ok(())
    }

    async fn register_plugin_handlers(
        &self,
        events: Arc<EventSystem>,
        sender: mpsc::Sender<EventMessage>,
    ) -> Result<(), PluginError> {
        for (plugin_name, event_type) in PLUGIN_EVENTS {
            let sender = sender.clone();
            let pn = plugin_name.to_string();
            let et = event_type.to_string();
            let handler = move |data: Value| -> Result<(), EventError> {
                let event_msg = EventMessage {
                    category: "plugin".to_string(),
                    namespace: None,
                    plugin: Some(pn.clone()),
                    event: et.clone(),
                    data,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                };
                sender.try_send(event_msg).map_err(|e| {
                    error!("Failed to send event: {}", e);
                    EventError::HandlerExecution(format!("Failed to send event: {}", e))
                })?;
                Ok(())
            };

            events.on_plugin(plugin_name, event_type, handler).await.map_err(|e| {
                PluginError::ExecutionError(format!("Failed to register plugin handler {}.{}: {:?}", plugin_name, event_type, e))
            })?;
        }

        Ok(())
    }

    async fn register_all_handlers(&mut self, events: Arc<EventSystem>) -> Result<(), PluginError> {
        let sender = self.start_event_processor();
        self.register_client_handlers(events.clone(), sender.clone()).await?;
        self.register_core_handlers(events.clone(), sender.clone()).await?;
        self.register_plugin_handlers(events.clone(), sender.clone()).await?;
        Ok(())
    }
}

#[async_trait]
impl SimplePlugin for BridgeGrpcPluginPlugin {
    fn name(&self) -> &str { &self.name }
    fn version(&self) -> &str { "2.2.0" }

    async fn register_handlers(&mut self, events: Arc<EventSystem>, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("BridgeGrpcPlugin: Registering all event handlers...");
        self.register_all_handlers(events).await?;
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "BridgeGrpcPlugin: Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(LogLevel::Info, "BridgeGrpcPlugin: Shutdown complete!");
        Ok(())
    }
}

create_simple_plugin!(BridgeGrpcPluginPlugin);

use async_trait::async_trait;
use horizon_event_system::{
    EventSystem, LogLevel, PluginError, ServerContext, SimplePlugin, EventError
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

pub mod grpc;
use crate::grpc::client::GrpcBridge;

/// Configuration pour un plugin gRPC
#[derive(Debug, Clone)]
pub struct GrpcBridgeConfig {
    /// Nom du plugin
    pub name: String,
    /// Version du plugin
    pub version: String,
    /// Endpoint gRPC (ex: "http://172.22.0.1:50051")
    pub grpc_endpoint: String,
    /// Activer les health checks
    pub enable_health_check: bool,
    /// Intervalle des health checks en secondes
    pub health_check_interval_seconds: u64,
    /// Events client à écouter (namespace, event)
    pub client_events: Vec<(String, String)>,
    /// Events core à écouter
    pub core_events: Vec<String>,
    /// Events plugin à écouter (plugin_name, event)
    pub plugin_events: Vec<(String, String)>,
    /// Taille du buffer pour les events
    pub event_buffer_size: usize,
    /// Nombre de worker threads pour le runtime Tokio
    pub worker_threads: usize,
}

impl Default for GrpcBridgeConfig {
    fn default() -> Self {
        Self {
            name: "grpc_bridge_plugin".to_string(),
            version: "1.0.0".to_string(),
            grpc_endpoint: "http://localhost:50051".to_string(),
            enable_health_check: true,
            health_check_interval_seconds: 30,
            client_events: vec![],
            core_events: vec![],
            plugin_events: vec![],
            event_buffer_size: 100,
            worker_threads: 2,
        }
    }
}

impl GrpcBridgeConfig {
    pub fn new(name: impl Into<String>, grpc_endpoint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            grpc_endpoint: grpc_endpoint.into(),
            ..Default::default()
        }
    }

    /// Builder pattern pour configurer les events client
    pub fn with_client_events(mut self, events: Vec<(&str, &str)>) -> Self {
        self.client_events = events.into_iter()
            .map(|(ns, evt)| (ns.to_string(), evt.to_string()))
            .collect();
        self
    }

    /// Builder pattern pour configurer les events core
    pub fn with_core_events(mut self, events: Vec<&str>) -> Self {
        self.core_events = events.into_iter()
            .map(|evt| evt.to_string())
            .collect();
        self
    }

    /// Builder pattern pour configurer les events plugin
    pub fn with_plugin_events(mut self, events: Vec<(&str, &str)>) -> Self {
        self.plugin_events = events.into_iter()
            .map(|(plugin, evt)| (plugin.to_string(), evt.to_string()))
            .collect();
        self
    }

    /// Builder pattern pour configurer la version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Builder pattern pour configurer les health checks
    pub fn with_health_check(mut self, enable: bool, interval_seconds: u64) -> Self {
        self.enable_health_check = enable;
        self.health_check_interval_seconds = interval_seconds;
        self
    }

    /// Builder pattern pour configurer la taille du buffer
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.event_buffer_size = size;
        self
    }

    /// Builder pattern pour configurer le nombre de worker threads
    pub fn with_worker_threads(mut self, threads: usize) -> Self {
        self.worker_threads = threads;
        self
    }
}

#[derive(Debug, Clone)]
struct EventMessage {
    category: String,
    namespace: Option<String>,
    plugin: Option<String>,
    event: String,
    data: Value,
    timestamp: u64,
}

/// Worker gRPC générique avec runtime Tokio et support bidirectionnel
struct GrpcWorker {
    grpc_bridge: Arc<RwLock<Option<GrpcBridge>>>,
    is_running: Arc<RwLock<bool>>,
    event_system: Arc<EventSystem>,
    config: GrpcBridgeConfig,
}

impl GrpcWorker {
    fn spawn(
        mut receiver: mpsc::Receiver<EventMessage>,
        event_system: Arc<EventSystem>,
        config: GrpcBridgeConfig,
    ) -> Arc<Self> {
        let grpc_bridge = Arc::new(RwLock::new(None));
        let is_running = Arc::new(RwLock::new(true));

        let worker = Arc::new(Self {
            grpc_bridge: Arc::clone(&grpc_bridge),
            is_running: Arc::clone(&is_running),
            event_system,
            config: config.clone(),
        });

        let worker_for_thread = Arc::clone(&worker);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(config.worker_threads)
                .build()
                .expect("Failed to build Tokio runtime");

            rt.block_on(async move {
                // Connexion au serveur gRPC
                match GrpcBridge::connect(&config.grpc_endpoint).await {
                    Ok(bridge) => {
                        *grpc_bridge.write().await = Some(bridge.clone());
                        info!("[{}] gRPC bridge initialized successfully", config.name);

                        // Appel d'initialisation
                        match bridge.initialize().await {
                            Ok(msg) => info!("[{}] Bridge initialized on server: {}", config.name, msg),
                            Err(e) => error!("[{}] Failed to initialize bridge on server: {}", config.name, e),
                        }

                        // Health check task
                        if config.enable_health_check {
                            let bridge_clone = bridge.clone();
                            let is_running_clone = Arc::clone(&is_running);
                            let plugin_name = config.name.clone();
                            let interval_secs = config.health_check_interval_seconds;
                            
                            tokio::spawn(async move {
                                let mut interval = tokio::time::interval(
                                    tokio::time::Duration::from_secs(interval_secs)
                                );
                                loop {
                                    interval.tick().await;
                                    if !*is_running_clone.read().await { break; }

                                    match bridge_clone.health_check().await {
                                        Ok(status) => debug!("[{}] Health check OK: {}", plugin_name, status),
                                        Err(e) => error!("[{}] Health check failed: {}", plugin_name, e),
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => error!("[{}] Failed to connect to gRPC server: {}", config.name, e),
                }

                // Boucle principale de traitement des événements
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

                            match bridge.forward_event(event_data).await {
                                Ok(response_data) => {
                                    debug!("[{}] Event forwarded successfully", config.name);
                                    
                                    // Traitement de la réponse
                                    if let Some(response_json) = response_data {
                                        if let Err(e) = worker_for_thread.process_grpc_response(response_json).await {
                                            error!("[{}] Failed to process gRPC response: {}", config.name, e);
                                        }
                                    }
                                }
                                Err(e) => error!("[{}] Failed to forward event: {}", config.name, e),
                            }
                        }
                    }
                }

                // Shutdown sur le serveur Go
                if let Some(bridge) = grpc_bridge.read().await.as_ref() {
                    match bridge.shutdown().await {
                        Ok(msg) => info!("[{}] Bridge shutdown on server: {}", config.name, msg),
                        Err(e) => error!("[{}] Failed to shutdown bridge on server: {}", config.name, e),
                    }
                }
            });
        });

        worker
    }

    async fn process_grpc_response(&self, response_json: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!("[{}] Processing gRPC response: {}", self.config.name, response_json);
        
        let response: Value = serde_json::from_str(&response_json)?;
        
        if let Some(events_to_trigger) = response.get("trigger_events") {
            if let Some(events_array) = events_to_trigger.as_array() {
                for event_json in events_array {
                    if let Err(e) = self.trigger_horizon_event(event_json).await {
                        error!("[{}] Failed to trigger Horizon event: {}", self.config.name, e);
                    }
                }
            }
        }

        Ok(())
    }

    async fn trigger_horizon_event(&self, event_json: &Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let event_type = event_json.get("type")
            .and_then(|t| t.as_str())
            .ok_or("Missing 'type' field in event")?;

        let category = event_json.get("category")
            .and_then(|c| c.as_str())
            .unwrap_or("plugin");

        let data = event_json.get("data")
            .cloned()
            .unwrap_or(json!({}));

        info!("[{}] Triggering Horizon event: {} (category: {})", self.config.name, event_type, category);

        match category {
            "client" => {
                if let Some(namespace) = event_json.get("namespace").and_then(|n| n.as_str()) {
                    self.event_system.emit_client(namespace, event_type, &data).await
                        .map_err(|e| format!("Failed to emit client event: {:?}", e))?;
                } else {
                    warn!("[{}] Client event missing namespace: {}", self.config.name, event_type);
                }
            }
            "core" => {
                self.event_system.emit_core(event_type, &data).await
                    .map_err(|e| format!("Failed to emit core event: {:?}", e))?;
            }
            "plugin" => {
                let plugin_name = event_json.get("plugin")
                    .and_then(|p| p.as_str())
                    .unwrap_or(&self.config.name);
                
                self.event_system.emit_plugin(plugin_name, event_type, &data).await
                    .map_err(|e| format!("Failed to emit plugin event: {:?}", e))?;
            }
            _ => {
                warn!("[{}] Unknown event category: {}", self.config.name, category);
                self.event_system.emit_plugin(&self.config.name, event_type, &data).await
                    .map_err(|e| format!("Failed to emit default plugin event: {:?}", e))?;
            }
        }

        debug!("[{}] Successfully triggered Horizon event: {}", self.config.name, event_type);
        Ok(())
    }
}

/// Plugin gRPC générique configurable
pub struct GrpcBridgePlugin {
    config: GrpcBridgeConfig,
    event_sender: Option<mpsc::Sender<EventMessage>>,
}

impl GrpcBridgePlugin {
    pub fn new(config: GrpcBridgeConfig) -> Self {
        info!("[{}] Creating gRPC bridge to {}", config.name, config.grpc_endpoint);
        Self {
            config,
            event_sender: None,
        }
    }

    fn start_event_processor(&mut self, event_system: Arc<EventSystem>) -> mpsc::Sender<EventMessage> {
        let (sender, receiver) = mpsc::channel::<EventMessage>(self.config.event_buffer_size);
        self.event_sender = Some(sender.clone());

        GrpcWorker::spawn(receiver, event_system, self.config.clone());

        sender
    }

    async fn register_client_handlers(
        &self,
        events: Arc<EventSystem>,
        sender: mpsc::Sender<EventMessage>,
    ) -> Result<(), PluginError> {
        for (namespace, event_type) in &self.config.client_events {
            let sender = sender.clone();
            let ns = namespace.clone();
            let et = event_type.clone();

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
        for event_type in &self.config.core_events {
            let sender = sender.clone();
            let et = event_type.clone();
            
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
        for (plugin_name, event_type) in &self.config.plugin_events {
            let sender = sender.clone();
            let pn = plugin_name.clone();
            let et = event_type.clone();
            
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
        let sender = self.start_event_processor(events.clone());
        self.register_client_handlers(events.clone(), sender.clone()).await?;
        self.register_core_handlers(events.clone(), sender.clone()).await?;
        self.register_plugin_handlers(events.clone(), sender.clone()).await?;
        Ok(())
    }
}

#[async_trait]
impl SimplePlugin for GrpcBridgePlugin {
    fn name(&self) -> &str { 
        &self.config.name 
    }
    
    fn version(&self) -> &str { 
        &self.config.version 
    }

    async fn register_handlers(&mut self, events: Arc<EventSystem>, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("[{}] Registering gRPC bridge event handlers...", self.config.name);
        self.register_all_handlers(events).await?;
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(LogLevel::Info, &format!("[{}] gRPC bridge initialization complete!", self.config.name));
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(LogLevel::Info, &format!("[{}] gRPC bridge shutdown complete!", self.config.name));
        Ok(())
    }
}

/// Fonction utilitaire pour créer facilement un plugin gRPC
pub fn create_grpc_plugin(config: GrpcBridgeConfig) -> Box<dyn SimplePlugin> {
    Box::new(GrpcBridgePlugin::new(config))
}
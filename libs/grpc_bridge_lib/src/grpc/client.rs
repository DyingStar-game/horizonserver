use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::transport::Channel;
use tonic::Request;
use tracing::{debug, error, info, warn};
use std::sync::Arc;
use uuid::Uuid;

pub mod bridge_service {
    tonic::include_proto!("bridge");
}

use bridge_service::{
    bridge_service_client::BridgeServiceClient,
    EventMessage, EventType, HealthRequest,
    InitRequest, ShutdownRequest,
};

#[derive(Debug, Clone)]
pub struct GrpcBridge {
    client: BridgeServiceClient<Channel>,
    event_sender: Arc<RwLock<Option<mpsc::Sender<EventMessage>>>>,
    is_connected: Arc<RwLock<bool>>,
}

impl GrpcBridge {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        info!("🔗 Connecting to gRPC server at {}", endpoint);
        
        let channel = Channel::from_shared(endpoint.to_string())
            .context("Invalid gRPC endpoint")?
            .connect()
            .await
            .context("Failed to connect to gRPC server")?;

        let client = BridgeServiceClient::new(channel);

        Ok(Self { 
            client,
            event_sender: Arc::new(RwLock::new(None)),
            is_connected: Arc::new(RwLock::new(false)),
        })
    }

    pub async fn initialize(&self) -> Result<String> {
        debug!("🚀 Sending initialization request to Go server");
        
        let request = Request::new(InitRequest {
            plugin_name: "bridge_grpc_plugin".to_string(),
            plugin_version: "1.0.0".to_string(),
        });

        let mut client = self.client.clone();
        let response = client.initialize(request).await?;
        
        let init_response = response.into_inner();
        info!("✅ Initialization successful: {}", init_response.message);
        
        Ok(format!("{} (server: {})", init_response.message, init_response.server_version))
    }

    /// Démarre le stream bidirectionnel
    pub async fn start_event_stream<F>(&self, on_go_event: F) -> Result<()>
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        info!("🌊 Starting bidirectional event stream");

        let (tx, rx) = mpsc::channel::<EventMessage>(100);
        
        // Stocke le sender pour pouvoir envoyer des événements
        *self.event_sender.write().await = Some(tx.clone());
        
        let request_stream = ReceiverStream::new(rx);
        let mut client = self.client.clone();
        
        let response = client.event_stream(Request::new(request_stream)).await?;
        let mut response_stream = response.into_inner();
        
        *self.is_connected.write().await = true;
        info!("✅ Bidirectional stream established");

        // Démarre la tâche de lecture du stream
        let is_connected = Arc::clone(&self.is_connected);
        tokio::spawn(async move {
            while let Some(result) = response_stream.next().await {
                match result {
                    Ok(event_msg) => {
                        Self::handle_incoming_event(event_msg, &on_go_event);
                    }
                    Err(e) => {
                        error!("❌ Stream error: {}", e);
                        *is_connected.write().await = false;
                        break;
                    }
                }
            }
            
            info!("📡 Event stream ended");
            *is_connected.write().await = false;
        });

        // Envoie un ping initial pour tester la connexion
        self.send_ping().await?;
        
        Ok(())
    }

    /// Gère les événements entrants depuis Go
    fn handle_incoming_event<F>(event_msg: EventMessage, on_go_event: &F)
    where
        F: Fn(Value),
    {
        match EventType::try_from(event_msg.r#type).unwrap_or(EventType::Unspecified) {
            EventType::GoEvent => {
                debug!("📥 Received event from Go: {}", event_msg.message_id);
                
                if !event_msg.event_json.is_empty() {
                    match serde_json::from_str(&event_msg.event_json) {
                        Ok(event_data) => {
                            on_go_event(event_data);
                        }
                        Err(e) => {
                            error!("Failed to parse Go event JSON: {}", e);
                        }
                    }
                }
            }
            EventType::Ping => {
                debug!("🏓 Received ping from Go: {}", event_msg.message_id);
            }
            EventType::Pong => {
                debug!("🏓 Received pong from Go: {}", event_msg.message_id);
            }
            EventType::Error => {
                error!("❌ Received error from Go: {}", event_msg.event_json);
            }
            _ => {
                warn!("❓ Received unknown event type: {:?}", event_msg.r#type);
            }
        }
    }

    /// Envoie un événement Horizon vers Go
    pub async fn forward_event(&self, event_data: Value) -> Result<()> {
        let event_json = serde_json::to_string(&event_data)
            .context("Failed to serialize event data")?;

        let event_msg = EventMessage {
            message_id: Uuid::new_v4().to_string(),
            r#type: EventType::HorizonEvent as i32,
            event_json,
            timestamp: chrono::Utc::now().timestamp(),
            source: "rust".to_string(),
        };

        self.send_event_message(event_msg).await
    }

    /// Envoie un ping pour tester la connexion
    pub async fn send_ping(&self) -> Result<()> {
        let ping_msg = EventMessage {
            message_id: Uuid::new_v4().to_string(),
            r#type: EventType::Ping as i32,
            event_json: "".to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            source: "rust".to_string(),
        };

        self.send_event_message(ping_msg).await
    }

    /// Envoie un message via le stream
    async fn send_event_message(&self, event_msg: EventMessage) -> Result<()> {
        if !*self.is_connected.read().await {
            return Err(anyhow::anyhow!("Stream not connected"));
        }

        if let Some(sender) = self.event_sender.read().await.as_ref() {
            sender.send(event_msg).await
                .context("Failed to send event through stream")?;
            debug!("📤 Event sent through stream");
        } else {
            return Err(anyhow::anyhow!("Event sender not initialized"));
        }

        Ok(())
    }

    /// Vérifie si la connexion est active
    pub async fn is_connected(&self) -> bool {
        *self.is_connected.read().await
    }

    /// Health check traditionnel (optionnel)
    pub async fn health_check(&self) -> Result<String> {
        debug!("💚 Performing health check");
        
        let request = Request::new(HealthRequest {});
        let mut client = self.client.clone();
        let response = client.health_check(request).await?;
        
        let health_response = response.into_inner();
        debug!("💚 Health check result: {}", health_response.status);
        
        Ok(health_response.status)
    }

    /// Notification de shutdown
    pub async fn shutdown(&self) -> Result<String> {
        info!("🔌 Notifying Go server of shutdown");
        
        // Ferme la connexion stream
        *self.is_connected.write().await = false;
        *self.event_sender.write().await = None;
        
        let request = Request::new(ShutdownRequest {});
        let mut client = self.client.clone();
        let response = client.shutdown(request).await?;
        
        let shutdown_response = response.into_inner();
        info!("✅ Shutdown notification sent: {}", shutdown_response.message);
        
        Ok(shutdown_response.message)
    }
}
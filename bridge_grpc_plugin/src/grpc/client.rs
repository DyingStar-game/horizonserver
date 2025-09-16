use anyhow::{Context, Result};
use serde_json::Value;
use tonic::transport::Channel;
use tonic::Request;
use tracing::{debug, error, info};

// Utiliser les types générés par tonic directement
pub mod bridge_service {
    tonic::include_proto!("bridgeservice");
}

use bridge_service::{
    bridge_service_client::BridgeServiceClient,
    EventRequest, EventResponse, HealthRequest, HealthResponse,
    InitRequest, InitResponse, ShutdownRequest, ShutdownResponse,
};

#[derive(Debug, Clone)]
pub struct GrpcBridge {
    client: BridgeServiceClient<Channel>,
}

impl GrpcBridge {
    /// Connexion au serveur gRPC
    pub async fn connect(endpoint: &str) -> Result<Self> {
        info!("🔗 Connecting to gRPC server at {}", endpoint);
        
        let channel = Channel::from_shared(endpoint.to_string())
            .context("Invalid gRPC endpoint")?
            .connect()
            .await
            .context("Failed to connect to gRPC server")?;

        let client = BridgeServiceClient::new(channel);

        Ok(Self { client })
    }

    /// Initialise le bridge côté Go
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

    /// Forward un événement vers le serveur Go
    pub async fn forward_event(&self, event_data: Value) -> Result<Option<String>> {
        debug!("📤 Forwarding event to Go server");
        
        let event_json = serde_json::to_string(&event_data)
            .context("Failed to serialize event data")?;

        let request = Request::new(EventRequest {
            event_json,
        });

        let mut client = self.client.clone();
        let response = client.forward_event(request).await?;
        
        let event_response = response.into_inner();
        
        if event_response.success {
            debug!("✅ Event forwarded successfully: {}", event_response.message);
            
            // Retourner les données de réponse si elles existent
            if !event_response.response_data.is_empty() {
                Ok(Some(event_response.response_data))
            } else {
                Ok(None)
            }
        } else {
            error!("❌ Event forwarding failed: {}", event_response.message);
            Err(anyhow::anyhow!("Event forwarding failed: {}", event_response.message))
        }
    }

    /// Vérifie la santé du serveur Go
    pub async fn health_check(&self) -> Result<String> {
        debug!("💚 Performing health check");
        
        let request = Request::new(HealthRequest {});

        let mut client = self.client.clone();
        let response = client.health_check(request).await?;
        
        let health_response = response.into_inner();
        debug!("💚 Health check result: {}", health_response.status);
        
        Ok(health_response.status)
    }

    /// Notifie le serveur Go de la fermeture
    pub async fn shutdown(&self) -> Result<String> {
        info!("🔌 Notifying Go server of shutdown");
        
        let request = Request::new(ShutdownRequest {});
        
        let mut client = self.client.clone();
        let response = client.shutdown(request).await?;
        
        let shutdown_response = response.into_inner();
        info!("✅ Shutdown notification sent: {}", shutdown_response.message);
        
        Ok(shutdown_response.message)
    }
}
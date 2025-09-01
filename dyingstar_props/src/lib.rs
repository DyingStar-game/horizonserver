use async_trait::async_trait;
use horizon_event_system::{
    create_simple_plugin, current_timestamp, defObject, CompressionType, EventSystem, GorcEvent, GorcObject, GorcObjectRegistry, LogLevel, PluginError, register_handlers, ReplicationLayer, ReplicationPriority, ServerContext, SimplePlugin, Vec3
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

// Define the Box50cm
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Box50cm {
    pub position: Vec3,
    pub rotation: Vec3,
    pub health: f32,
    pub weight: f32,
    // pub mineral_type: MineralType,
}

impl Box50cm {
    pub fn new(position: Vec3, rotation: Vec3) -> Self {
        Self {
            position,
            rotation,
            health: 100.0,
            weight: 0.25
            // mineral_type: MineralType::Iron,
        }
    }
}

impl GorcObject for Box50cm {
    fn type_name(&self) -> &'static str {
        "Box50cm"
    }
    
    fn position(&self) -> Vec3 {
        self.position
    }
        
    fn get_layers(&self) -> Vec<ReplicationLayer> {
        vec![
            ReplicationLayer::new(
                0,
                50.0,
                30.0,
                vec!["position".to_string(), "rotation".to_string(), "health".to_string()],
                CompressionType::Delta,
            ),
            ReplicationLayer::new(
                1,
                200.0,
                10.0,
                vec!["position".to_string(), "rotation".to_string()],
                CompressionType::Quantized,
            ),
            ReplicationLayer::new(
                2,
                1000.0,
                2.0,
                vec!["position".to_string(), "rotation".to_string()],
                CompressionType::High,
            ),
        ]
    }
    
    fn get_priority(&self, observer_pos: Vec3) -> ReplicationPriority {
        let distance = self.position.distance(observer_pos);
        match distance {
            d if d < 25.0 => ReplicationPriority::Critical,
            d if d < 100.0 => ReplicationPriority::High,
            d if d < 500.0 => ReplicationPriority::Normal,
            _ => ReplicationPriority::Low,
        }
    }

    fn serialize_for_layer(&self, layer: &ReplicationLayer) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        match layer.channel {
            0 => {
                let data = serde_json::json!({
                    "position": self.position,
                    "rotation": self.rotation,
                    "health": self.health
                });
                Ok(serde_json::to_vec(&data)?)
            },
            1 => {
                let data = serde_json::json!({
                    "position": self.position,
                    "rotation": self.rotation
                });
                Ok(serde_json::to_vec(&data)?)
            },
            2 => {
                let data = serde_json::json!({
                    "position": self.position,
                    "rotation": self.rotation
                });
                Ok(serde_json::to_vec(&data)?)
            },
            _ => Ok(vec![]),
        }
    }
    
    fn update_position(&mut self, new_position: Vec3) {
        self.position = new_position;
    }
    
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    
    fn clone_object(&self) -> Box<dyn GorcObject> {
        Box::new(self.clone())
    }
}

// Register Box50cm object with GORC
defObject!(Box50cm);






/// DyingstarProps Plugin
pub struct DyingstarPropsPlugin {
    name: String,
    boxes50cm: Arc<RwLock<HashMap<String, Box50cm>>>,
    object_registry: Arc<GorcObjectRegistry>,
}

impl DyingstarPropsPlugin {
    pub fn new() -> Self {
        info!("🔧 DyingstarPropsPlugin: Creating new instance");
        Self {
            name: "dyingstar_props".to_string(),
            boxes50cm: Arc::new(RwLock::new(HashMap::new())),
            object_registry: Arc::new(GorcObjectRegistry::new()),
        }
    }

    async fn setup_object_registry(&self) -> Result<(), String> {
        // Register Box5ocm object types
        Box50cm::register_with_gorc(self.object_registry.clone()).await
            .map_err(|e| e.to_string())?;
        
        let objects = self.object_registry.list_objects().await;
        info!("📦 Registered GORC objects: {:?}", objects);
        
        Ok(())
    }

    async fn setup_gorc_handlers(&self, events: Arc<EventSystem>) -> Result<(), PluginError> {
        // Register GORC event handlers for Box50cm objects
        events.on_gorc_instance("Box50cm", 2, "cosmetic_update", |event: GorcEvent, _instance| {
            info!("✨ Box50cm cosmetic update: {}", event.object_id);
            Ok(())
        }).await.map_err(|e| PluginError::ExecutionError(e.to_string()))?;

        Ok(())
    }
}

#[async_trait]
impl SimplePlugin for DyingstarPropsPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn register_handlers(&mut self, events: Arc<EventSystem>, _context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        info!("🔧 DyingstarPropsPlugin: Registering event handlers...");
        

        // // Setup object registry
        self.setup_object_registry().await
            .map_err(|e| PluginError::ExecutionError(e))?;

        // Setup GORC handlers
        self.setup_gorc_handlers(events.clone()).await?;

        events.on_client("box50cm", "spawn", |event: serde_json::Value| {
            println!("Received prop event: {:?}", event);
            // let box50cm = Box50cm::new(Vec3::new(500.0, 100.0, 300.0), Vec3::new(0.0, 0.0, 0.0));
            // let box50cm_id = "box50cm_001".to_string();
            // {
            //     let mut boxes50cm = self.boxes50cm.write();
            //     boxes50cm.insert(box50cm_id.clone(), box50cm.clone());
            // }
            Ok(())
        }).await.unwrap();
        
        info!("🔧 DyingstarPropsPlugin: ✅ All handlers registered successfully!");
        Ok(())
    }

    async fn on_init(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPropsPlugin: Starting up!",
        );

        // TODO: Add your initialization logic here
        
        info!("🔧 DyingstarPropsPlugin: ✅ Initialization complete!");
        Ok(())
    }

    async fn on_shutdown(&mut self, context: Arc<dyn ServerContext>) -> Result<(), PluginError> {
        context.log(
            LogLevel::Info,
            "🔧 DyingstarPropsPlugin: Shutting down!",
        );

        // TODO: Add your cleanup logic here

        info!("🔧 DyingstarPropsPlugin: ✅ Shutdown complete!");
        Ok(())
    }
}

// Create the plugin using the macro
create_simple_plugin!(DyingstarPropsPlugin);

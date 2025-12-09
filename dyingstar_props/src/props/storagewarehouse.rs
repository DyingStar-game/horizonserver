use rand::Rng;

/// Container types for container storage
const CONTAINER_TYPES: &[&str] = &["liquid", "standard_a", "standard_b"];

/// Pallet types for pallet storage
const PALLET_TYPES: &[&str] = &["benne", "crate", "liquid"];

/// Storage type enum
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageType {
    Container,
    Pallet,
}

/// Result of randomization containing the storage type and the items
#[derive(Debug, Clone)]
pub struct StorageConfiguration {
    pub storage_type: StorageType,
    pub items: Vec<StorageItem>,
}

/// Individual storage item with type and position
#[derive(Debug, Clone)]
pub struct StorageItem {
    pub item_type: String,
    pub position: (usize, usize, usize), // (row, column, height) or (index, 0, height) for containers
}

/// Randomize the type of storage and generate the configuration
/// 50% chance for containers, 50% chance for pallets
pub fn randomize_storage_warehouse() -> StorageConfiguration {
    let mut rng = rand::thread_rng();
    
    // 50% chance for container, 50% chance for pallet
    let storage_type = if rng.gen_bool(0.5) {
        StorageType::Container
    } else {
        StorageType::Pallet
    };
    
    let items = match storage_type {
        StorageType::Container => generate_container_storage(&mut rng),
        StorageType::Pallet => generate_pallet_storage(&mut rng),
    };
    
    StorageConfiguration {
        storage_type,
        items,
    }
}

/// Generate container storage configuration
/// Randomize number of containers between 0 and 4
/// For each container, randomize type from CONTAINER_TYPES
fn generate_container_storage(rng: &mut impl Rng) -> Vec<StorageItem> {
    let num_containers = rng.gen_range(0..=4);
    let mut items = Vec::new();
    
    for i in 0..num_containers {
        let container_type_idx = rng.gen_range(0..CONTAINER_TYPES.len());
        let container_type = CONTAINER_TYPES[container_type_idx].to_string();
        
        items.push(StorageItem {
            item_type: container_type,
            position: (0, 0, i), // Simple linear positioning for containers
        });
    }
    
    items
}

/// Generate pallet storage configuration
/// For each column (3) and each line (10):
/// Randomize number of pallets between 0 and 9
/// For each pallet, randomize type from PALLET_TYPES
fn generate_pallet_storage(rng: &mut impl Rng) -> Vec<StorageItem> {
    let mut items = Vec::new();
    
    const NUM_COLUMNS: usize = 3;
    const NUM_LINES: usize = 10;
    
    for column in 0..NUM_COLUMNS {
        for line in 0..NUM_LINES {
            let num_pallets = rng.gen_range(0..=9);
            
            for height in 0..num_pallets {
                let pallet_type_idx = rng.gen_range(0..PALLET_TYPES.len());
                let pallet_type = PALLET_TYPES[pallet_type_idx].to_string();
                
                items.push(StorageItem {
                    item_type: pallet_type,
                    position: (line, column, height),
                });
            }
        }
    }
    
    items
}

/// Get the scene path for a container type
pub fn get_container_scene_path(container_type: &str) -> String {
    format!("scenes/props/StorageBoxes/container_{}_1200x240x240.tscn", container_type)
}

/// Get the scene path for a pallet type
pub fn get_pallet_scene_path(pallet_type: &str) -> String {
    format!("scenes/props/StorageBoxes/pallet_{}_120x80x100.tscn", pallet_type)
}

/// Get the scene path for a storage item
pub fn get_item_scene_path(storage_type: StorageType, item_type: &str) -> String {
    match storage_type {
        StorageType::Container => get_container_scene_path(item_type),
        StorageType::Pallet => get_pallet_scene_path(item_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_randomize_storage_warehouse() {
        // Test multiple times to ensure randomization works
        for _ in 0..10 {
            let config = randomize_storage_warehouse();
            println!("Storage type: {:?}", config.storage_type);
            println!("Number of items: {}", config.items.len());
            
            match config.storage_type {
                StorageType::Container => {
                    assert!(config.items.len() <= 4);
                    for item in &config.items {
                        assert!(CONTAINER_TYPES.contains(&item.item_type.as_str()));
                    }
                }
                StorageType::Pallet => {
                    // Max 3 columns * 10 lines * 9 pallets = 270
                    assert!(config.items.len() <= 270);
                    for item in &config.items {
                        assert!(PALLET_TYPES.contains(&item.item_type.as_str()));
                        assert!(item.position.0 < 10); // line
                        assert!(item.position.1 < 3);  // column
                        assert!(item.position.2 < 9);  // height
                    }
                }
            }
        }
    }
    
    #[test]
    fn test_scene_paths() {
        assert_eq!(
            get_container_scene_path("liquid"),
            "scenes/props/StorageBoxes/container_liquid_1200x240x240.tscn"
        );
        assert_eq!(
            get_pallet_scene_path("crate"),
            "scenes/props/StorageBoxes/pallet_crate_120x80x100.tscn"
        );
    }
}

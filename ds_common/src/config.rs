use std::fs;
use std::path::Path;
use toml::value::Table;
use std::panic;

pub struct Config {
    pub plugin_name: String,
    pub configuration: Table,
}

impl Config {
    pub fn new(plugin_name: &str) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            configuration: Config::read_config_url(plugin_name),
        }
    }

    pub fn get_value(&self, key: &str) -> Option<&toml::Value> {
        self.configuration.get(key)
    }

    pub fn get_vector(&self, key: &str) -> Vec<String> {
        match self.get_value(key) {
            Some(value) => value.as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            None => vec![],
        }
    }

    fn read_config_url(plugin_name: &str) -> Table {
        // Try multiple possible paths for the plugins.toml file
        let possible_paths = vec![
            "plugins.toml",
            "Horizon/plugins.toml",
            "../Horizon/plugins.toml",
            "../../Horizon/plugins.toml",
            "../../plugins.toml",
        ];

        for path in possible_paths {
            if Path::new(path).exists() {
                let contents = fs::read_to_string(path)
                    .map_err(|e| panic!("Failed to read {}: {}", path, e)).unwrap();
                
                let config: toml::Value = toml::from_str(&contents)
                    .map_err(|e| panic!("Failed to parse TOML: {}", e)).unwrap();
                
                match config.get(plugin_name) {
                    None => panic!("plugin part ({}) not found in the config file", plugin_name),
                    Some(value) => return value.as_table().unwrap().clone(),
                };
            };
        }
        panic!("plugins.toml file not found in any expected location")
    }

}
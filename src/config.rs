use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub default_provider: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub type_name: String, // "gemini", "openai_compat"
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

impl AppConfig {
    pub fn load() -> Self {
        let paths = vec![
            PathBuf::from("config.toml"),
            dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("rusty-claw/config.toml"),
            dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".rusty-claw/config.toml"),
        ];

        for path in paths {
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        match toml::from_str(&content) {
                            Ok(config) => {
                                tracing::info!("Loaded config from {}", path.display());
                                return config;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse config at {}: {}", path.display(), e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read config at {}: {}", path.display(), e);
                    }
                }
            }
        }

        tracing::info!("No config file found, using defaults");
        Self::default()
    }

    pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }
}

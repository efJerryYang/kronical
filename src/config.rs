use anyhow::Result;
use config::{Config, Environment, File};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub workspace_dir: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        let workspace_dir = if let Some(home) = dirs::home_dir() {
            home.join(".chronicle")
        } else {
            panic!("Failed to determine home directory")
        };

        Self { workspace_dir }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let workspace_dir = Self::default().workspace_dir;
        let config_path = workspace_dir.join("config.toml");

        let mut builder = Config::builder()
            .set_default("workspace_dir", workspace_dir.to_str().unwrap())?
            .set_default("polling_interval_seconds", 2)?;

        // Load config file if it exists
        if config_path.exists() {
            builder = builder.add_source(File::from(config_path));
        }

        // Allow environment variables to override config
        builder = builder.add_source(Environment::with_prefix("CHRONICLE"));

        let config = builder.build()?;
        let app_config: AppConfig = config.try_deserialize()?;

        Ok(app_config)
    }
}

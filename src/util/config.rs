use anyhow::Result;
use config::{Config, Environment, File};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub workspace_dir: PathBuf,
    pub retention_minutes: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub max_windows_per_app: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        let workspace_dir = if let Some(home) = dirs::home_dir() {
            home.join(".chronicle")
        } else {
            panic!("Failed to determine home directory")
        };

        Self {
            workspace_dir,
            retention_minutes: 10,
            ephemeral_max_duration_secs: 60,
            ephemeral_min_distinct_ids: 3,
            max_windows_per_app: 30,
            ephemeral_app_max_duration_secs: 60,
            ephemeral_app_min_distinct_procs: 3,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let workspace_dir = Self::default().workspace_dir;
        let config_path = workspace_dir.join("config.toml");

        let mut builder = Config::builder()
            .set_default("workspace_dir", workspace_dir.to_str().unwrap())?
            .set_default("polling_interval_seconds", 2)?
            .set_default("retention_minutes", 10)?
            .set_default("ephemeral_max_duration_secs", 60)?
            .set_default("ephemeral_min_distinct_ids", 3)?
            .set_default("max_windows_per_app", 30)?
            .set_default("ephemeral_app_max_duration_secs", 60)?
            .set_default("ephemeral_app_min_distinct_procs", 3)?;

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

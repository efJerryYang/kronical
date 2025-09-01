use anyhow::Result;
use config::{Config, Environment, File};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub workspace_dir: PathBuf,
    pub retention_minutes: u64,
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub max_windows_per_app: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
    pub pid_cache_capacity: usize,
    pub title_cache_capacity: usize,
    pub title_cache_ttl_secs: u64,
    pub focus_interner_max_strings: usize,
    pub tracker_enabled: bool,
    pub tracker_interval_secs: f64,
    pub tracker_batch_size: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        let workspace_dir = if let Some(home) = dirs::home_dir() {
            home.join(".kronical")
        } else {
            panic!("Failed to determine home directory")
        };

        Self {
            workspace_dir,
            retention_minutes: 72 * 60,
            active_grace_secs: 30,
            idle_threshold_secs: 300,
            ephemeral_max_duration_secs: 60,
            ephemeral_min_distinct_ids: 3,
            max_windows_per_app: 30,
            ephemeral_app_max_duration_secs: 60,
            ephemeral_app_min_distinct_procs: 3,
            pid_cache_capacity: 1024,
            title_cache_capacity: 512,
            title_cache_ttl_secs: 0,
            focus_interner_max_strings: 4096,
            tracker_enabled: false,
            tracker_interval_secs: 1.0,
            tracker_batch_size: 60,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let workspace_dir = Self::default().workspace_dir;
        let config_path = workspace_dir.join("config.toml");

        let mut builder = Config::builder()
            .set_default("workspace_dir", workspace_dir.to_str().unwrap())?
            .set_default("retention_minutes", 72 * 60)?
            .set_default("active_grace_secs", 30)?
            .set_default("idle_threshold_secs", 300)?
            .set_default("ephemeral_max_duration_secs", 60)?
            .set_default("ephemeral_min_distinct_ids", 3)?
            .set_default("max_windows_per_app", 30)?
            .set_default("ephemeral_app_max_duration_secs", 60)?
            .set_default("ephemeral_app_min_distinct_procs", 3)?
            .set_default("pid_cache_capacity", 1024)?
            .set_default("title_cache_capacity", 512)?
            .set_default("title_cache_ttl_secs", 0)?
            .set_default("focus_interner_max_strings", 4096)?
            .set_default("tracker_enabled", false)?
            .set_default("tracker_interval_secs", 1.0)?
            .set_default("tracker_batch_size", 60)?;

        // Load config file if it exists
        if config_path.exists() {
            builder = builder.add_source(File::from(config_path));
        }

        // Allow environment variables to override config
        builder = builder.add_source(Environment::with_prefix("KRONICAL"));

        let config = builder.build()?;
        let app_config: AppConfig = config.try_deserialize()?;

        Ok(app_config)
    }
}

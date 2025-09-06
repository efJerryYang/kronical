use anyhow::Result;
use config::{Config, Environment, File};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackendConfig {
    Duckdb,
    Sqlite3,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub workspace_dir: PathBuf,
    pub db_backend: DatabaseBackendConfig,
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
    pub tracker_refresh_secs: f64,
}

impl Default for AppConfig {
    fn default() -> Self {
        // Be resilient in environments without HOME by falling back to CWD.
        let base_dir = dirs::home_dir()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace_dir = base_dir.join(".kronical");

        Self {
            workspace_dir,
            db_backend: DatabaseBackendConfig::Duckdb,
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
            tracker_refresh_secs: 1.0,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let workspace_dir = Self::default().workspace_dir;
        let config_path = workspace_dir.join("config.toml");

        let mut builder = Config::builder()
            // Avoid panics on non-UTF8 paths by using lossy conversion.
            .set_default("workspace_dir", workspace_dir.to_string_lossy().as_ref())?
            .set_default("db_backend", "duckdb")?
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
            .set_default("tracker_batch_size", 60)?
            .set_default("tracker_refresh_secs", 1.0)?;

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

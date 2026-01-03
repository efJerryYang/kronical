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
    // Tracker storage backend (separate from main DB)
    pub tracker_db_backend: DatabaseBackendConfig,
    pub persist_raw_events: bool,
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
    pub focus_window_coalesce_ms: u64,
    pub tracker_enabled: bool,
    pub tracker_interval_secs: f64,
    pub tracker_batch_size: usize,
    pub tracker_refresh_secs: f64,
    // DuckDB memory caps (MB). Applied when DuckDB is used.
    pub duckdb_memory_limit_mb_main: u64,
    pub duckdb_memory_limit_mb_tracker: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        let base_dir = dirs::home_dir()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace_dir = base_dir.join(".kronical");

        Self {
            workspace_dir,
            db_backend: DatabaseBackendConfig::Duckdb,
            tracker_db_backend: DatabaseBackendConfig::Duckdb,
            persist_raw_events: false,
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
            focus_window_coalesce_ms: 100,
            tracker_enabled: false,
            tracker_interval_secs: 1.0,
            tracker_batch_size: 60,
            tracker_refresh_secs: 1.0,
            duckdb_memory_limit_mb_main: 10,
            duckdb_memory_limit_mb_tracker: 10,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let workspace_dir = Self::default().workspace_dir;
        let config_path = workspace_dir.join("config.toml");

        let mut builder = Config::builder()
            .set_default("workspace_dir", workspace_dir.to_string_lossy().as_ref())?
            .set_default("db_backend", "duckdb")?
            .set_default("tracker_db_backend", "duckdb")?
            .set_default("persist_raw_events", false)?
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
            .set_default("focus_window_coalesce_ms", 100)?
            .set_default("tracker_enabled", false)?
            .set_default("tracker_interval_secs", 1.0)?
            .set_default("tracker_batch_size", 60)?
            .set_default("tracker_refresh_secs", 1.0)?
            .set_default("duckdb_memory_limit_mb_main", 10)?
            .set_default("duckdb_memory_limit_mb_tracker", 10)?;

        if config_path.exists() {
            builder = builder.add_source(File::from(config_path));
        }

        builder = builder.add_source(Environment::with_prefix("KRONICAL"));

        let config = builder.build()?;
        let app_config: AppConfig = config.try_deserialize()?;
        Ok(app_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{Mutex, OnceLock},
    };

    fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
        unsafe { std::env::set_var(key, val) };
    }

    fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key) };
    }

    fn with_env_lock<T>(f: impl FnOnce() -> T) -> T {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned");
        let result = f();
        drop(guard);
        result
    }

    #[test]
    fn default_values_match_expected_profile() {
        with_env_lock(|| {
            let cfg = AppConfig::default();
            assert!(cfg.workspace_dir.ends_with(".kronical"));
            assert_eq!(cfg.db_backend, DatabaseBackendConfig::Duckdb);
            assert_eq!(cfg.tracker_db_backend, DatabaseBackendConfig::Duckdb);
            assert!(!cfg.persist_raw_events);
            assert_eq!(cfg.retention_minutes, 72 * 60);
            assert_eq!(cfg.active_grace_secs, 30);
            assert_eq!(cfg.idle_threshold_secs, 300);
            assert_eq!(cfg.ephemeral_min_distinct_ids, 3);
            assert_eq!(cfg.max_windows_per_app, 30);
            assert!(!cfg.tracker_enabled);
            assert_eq!(cfg.tracker_batch_size, 60);
            assert_eq!(cfg.duckdb_memory_limit_mb_main, 10);
            assert_eq!(cfg.duckdb_memory_limit_mb_tracker, 10);
        });
    }

    #[test]
    fn load_merges_config_file_and_environment_overrides() {
        with_env_lock(|| {
            use tempfile::tempdir;

            let saved_home = std::env::var_os("HOME");
            if saved_home.is_some() {
                remove_env("HOME");
            }

            let original_home = std::env::var_os("HOME");
            let dir = tempdir().expect("tempdir");
            set_env("HOME", dir.path());

            let workspace_dir = dir.path().join(".kronical");
            fs::create_dir_all(&workspace_dir).expect("create workspace");
            let config_path = workspace_dir.join("config.toml");
            let config_contents =
                format!("workspace_dir = \"{}\"\n", workspace_dir.to_string_lossy())
                    + "db_backend = \"sqlite3\"\n"
                    + "tracker_db_backend = \"duckdb\"\n"
                    + "retention_minutes = 45\n"
                    + "tracker_enabled = true\n"
                    + "title_cache_capacity = 1024\n"
                    + "duckdb_memory_limit_mb_main = 256\n";
            fs::write(&config_path, config_contents).expect("write config");

            // Environment vars override the file.
            set_env("KRONICAL_TRACKER_ENABLED", "false");
            set_env("KRONICAL_TITLE_CACHE_CAPACITY", "2048");

            let cfg = AppConfig::load().expect("load config");

            assert_eq!(cfg.workspace_dir, workspace_dir);
            assert_eq!(cfg.db_backend, DatabaseBackendConfig::Sqlite3);
            assert_eq!(cfg.tracker_db_backend, DatabaseBackendConfig::Duckdb);
            assert_eq!(cfg.retention_minutes, 45);
            assert!(!cfg.tracker_enabled, "env override should win");
            assert_eq!(cfg.title_cache_capacity, 2048);
            assert_eq!(cfg.duckdb_memory_limit_mb_main, 256);

            remove_env("KRONICAL_TRACKER_ENABLED");
            remove_env("KRONICAL_TITLE_CACHE_CAPACITY");

            if let Some(val) = original_home {
                set_env("HOME", val);
            } else {
                remove_env("HOME");
            }

            if let Some(val) = saved_home {
                set_env("HOME", val);
            }
        });
    }

    #[test]
    fn load_restores_home_when_it_was_unset() {
        with_env_lock(|| {
            use tempfile::tempdir;

            let saved_home = std::env::var_os("HOME");
            if saved_home.is_some() {
                remove_env("HOME");
            }

            let original_home = std::env::var_os("HOME");

            let dir = tempdir().expect("tempdir");
            set_env("HOME", dir.path());

            let workspace_dir = dir.path().join(".kronical");
            std::fs::create_dir_all(&workspace_dir).expect("create workspace");
            let config_path = workspace_dir.join("config.toml");
            std::fs::write(&config_path, "db_backend = \"duckdb\"\n").expect("write config");

            let cfg = AppConfig::load().expect("load config");
            assert_eq!(cfg.workspace_dir, workspace_dir);

            // Removing HOME in the loader should leave it unset here.
            remove_env("HOME");
            assert!(std::env::var_os("HOME").is_none());

            if let Some(val) = original_home {
                set_env("HOME", val);
            }

            if let Some(val) = saved_home {
                set_env("HOME", val);
            }
        });
    }
}

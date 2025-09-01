use anyhow::{Context, Result};
use kronical::daemon::coordinator::EventCoordinator;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::storage::StorageBackend;
use kronical::util::config::AppConfig;
use log::{error, info};
use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn ensure_workspace_dir(workspace_dir: &PathBuf) -> Result<()> {
    if !workspace_dir.exists() {
        std::fs::create_dir_all(workspace_dir).context("Failed to create workspace directory")?;
    }
    Ok(())
}

fn write_pid_file(pid_file: &PathBuf) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(pid_file, pid.to_string()).context("Failed to write PID file")?;
    Ok(())
}

fn setup_file_logging(log_dir: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(log_dir).context("Failed to create log directory")?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kronid")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
        .context("Failed to create log appender")?;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_target(true)
                .with_thread_ids(true)
                .with_level(true)
                .with_ansi(false)
                .with_timer(fmt::time::ChronoUtc::new(
                    "%Y-%m-%dT%H:%M:%S%.6fZ".to_string(),
                )),
        )
        .with(env_filter)
        .init();

    Ok(())
}

fn main() {
    let config = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = ensure_workspace_dir(&config.workspace_dir) {
        eprintln!("Failed to create workspace directory: {}", e);
        std::process::exit(1);
    }

    let log_dir = config.workspace_dir.join("logs");
    if let Err(e) = setup_file_logging(&log_dir) {
        eprintln!("Failed to setup logging: {}", e);
        std::process::exit(1);
    }

    let data_file = config.workspace_dir.join("data.db");
    let pid_file = config.workspace_dir.join("kronid.pid");
    if let Err(e) = write_pid_file(&pid_file) {
        error!("Failed to write PID: {}", e);
        std::process::exit(1);
    }

    info!("Starting Kronicle daemon (kronid)");

    let data_store: Box<dyn StorageBackend> = match SqliteStorage::new(&data_file) {
        Ok(s) => Box::new(s),
        Err(e) => {
            error!("Failed to initialize SQLite store: {}", e);
            std::process::exit(1);
        }
    };

    let coordinator = EventCoordinator::new(
        config.retention_minutes,
        config.active_grace_secs,
        config.idle_threshold_secs,
        config.ephemeral_max_duration_secs,
        config.ephemeral_min_distinct_ids,
        config.max_windows_per_app,
        config.ephemeral_app_max_duration_secs,
        config.ephemeral_app_min_distinct_procs,
        config.pid_cache_capacity,
        config.title_cache_capacity,
        config.title_cache_ttl_secs,
        config.focus_interner_max_strings,
    );

    info!("Kronicle daemon will run on MAIN THREAD (required by macOS hooks)");
    let result = coordinator.start_main_thread(data_store, pid_file.clone());
    let _ = std::fs::remove_file(&pid_file);
    if let Err(e) = result {
        error!("Kronicle daemon error: {}", e);
        std::process::exit(1);
    }
}

use anyhow::{Context, Result};
use kronical::daemon::coordinator::EventCoordinator;
use kronical::storage::StorageBackend;
use kronical::storage::duckdb::DuckDbStorage;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::util::config::AppConfig;
use kronical::util::config::DatabaseBackendConfig;
use log::{error, info};
use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn ensure_workspace_dir(workspace_dir: &PathBuf) -> Result<()> {
    if !workspace_dir.exists() {
        std::fs::create_dir_all(workspace_dir).context("Failed to create workspace directory")?;
    }
    Ok(())
}

// TODO: bad code, we should use certain library to check
fn is_process_running(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn write_pid_file(pid_file: &PathBuf) -> Result<()> {
    if pid_file.exists() {
        match std::fs::read_to_string(pid_file) {
            Ok(content) => {
                if let Ok(existing_pid) = content.trim().parse::<u32>() {
                    if is_process_running(existing_pid) {
                        return Err(anyhow::anyhow!(
                            "Kronical daemon is already running (PID: {})",
                            existing_pid
                        ));
                    } else {
                        info!(
                            "Removing stale PID file (process {} no longer exists)",
                            existing_pid
                        );
                        let _ = std::fs::remove_file(pid_file);
                    }
                }
            }
            Err(_) => {
                info!("Removing unreadable PID file");
                let _ = std::fs::remove_file(pid_file);
            }
        }
    }

    let current_pid = std::process::id();
    std::fs::write(pid_file, current_pid.to_string()).context("Failed to write PID file")?;
    Ok(())
}

fn verify_pid_file(pid_file: &PathBuf) -> Result<()> {
    let current_pid = std::process::id();
    let content =
        std::fs::read_to_string(pid_file).context("Failed to read PID file for verification")?;
    let file_pid: u32 = content
        .trim()
        .parse()
        .context("PID file contains invalid PID format")?;

    if file_pid != current_pid {
        return Err(anyhow::anyhow!(
            "PID file verification failed: expected {}, found {}",
            current_pid,
            file_pid
        ));
    }

    info!("PID file verification successful (PID: {})", current_pid);
    Ok(())
}

fn cleanup_pid_file(pid_file: &PathBuf) {
    if !pid_file.exists() {
        error!(
            "PID file disappeared during runtime! This indicates a problem with process management."
        );
        return;
    }

    let current_pid = std::process::id();
    match std::fs::read_to_string(pid_file) {
        Ok(content) => match content.trim().parse::<u32>() {
            Ok(file_pid) => {
                if file_pid == current_pid {
                    if let Err(e) = std::fs::remove_file(pid_file) {
                        error!("Failed to remove PID file: {}", e);
                    } else {
                        info!("Successfully cleaned up PID file");
                    }
                } else {
                    error!(
                        "PID file contains different PID ({}) than current process ({}). Not removing PID file!",
                        file_pid, current_pid
                    );
                }
            }
            Err(e) => {
                error!("PID file contains invalid PID: {}. Error: {}", content, e);
            }
        },
        Err(e) => {
            error!("Failed to read PID file for cleanup: {}", e);
        }
    }
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

    let data_file = match config.db_backend {
        DatabaseBackendConfig::Duckdb => config.workspace_dir.join("data.duckdb"),
        DatabaseBackendConfig::Sqlite3 => config.workspace_dir.join("data.sqlite3"),
    };
    let pid_file = kronical::util::paths::pid_file(&config.workspace_dir);
    if let Err(e) = write_pid_file(&pid_file) {
        error!("Failed to write PID: {}", e);
        std::process::exit(1);
    }

    if let Err(e) = verify_pid_file(&pid_file) {
        error!("PID file verification failed: {}", e);
        std::process::exit(1);
    }

    info!("Starting Kronical daemon (kronid)");

    let data_store: Box<dyn StorageBackend> = match config.db_backend {
        DatabaseBackendConfig::Duckdb => match DuckDbStorage::new(&data_file) {
            Ok(s) => Box::new(s),
            Err(e) => {
                error!("Failed to initialize DuckDB store: {}", e);
                std::process::exit(1);
            }
        },
        DatabaseBackendConfig::Sqlite3 => match SqliteStorage::new(&data_file) {
            Ok(s) => Box::new(s),
            Err(e) => {
                error!("Failed to initialize SQLite store: {}", e);
                std::process::exit(1);
            }
        },
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
        config.tracker_enabled,
        config.tracker_interval_secs,
        config.tracker_batch_size,
    );

    info!("Kronical daemon will run on MAIN THREAD (required by macOS hooks)");
    let result = coordinator.start_main_thread(data_store, config.workspace_dir.clone());

    cleanup_pid_file(&pid_file);

    if let Err(e) = result {
        error!("Kronical daemon error: {}", e);
        std::process::exit(1);
    }
}

use kronical::daemon::coordinator::EventCoordinator;
use kronical::daemon::snapshot::SnapshotBus;
use kronical::storage::StorageBackend;
use kronical::storage::duckdb::DuckDbStorage;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::util::config::AppConfig;
use kronical::util::config::DatabaseBackendConfig;
use log::{error, info};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn ensure_workspace_dir(workspace_dir: &PathBuf) {
    if !workspace_dir.exists() {
        std::fs::create_dir_all(workspace_dir).unwrap_or_else(|e| {
            eprintln!("Failed to create workspace directory: {}", e);
            std::process::exit(1);
        });
    }
}

fn is_process_running(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn write_pid_file(pid_file: &PathBuf) {
    if pid_file.exists() {
        match std::fs::read_to_string(pid_file) {
            Ok(content) => {
                if let Ok(existing_pid) = content.trim().parse::<u32>() {
                    if is_process_running(existing_pid) {
                        eprintln!("Kronical daemon is already running (PID: {})", existing_pid);
                        std::process::exit(1);
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
    std::fs::write(pid_file, current_pid.to_string()).unwrap_or_else(|e| {
        eprintln!("Failed to write PID file: {}", e);
        std::process::exit(1);
    });
}

fn verify_pid_file(pid_file: &PathBuf) {
    let current_pid = std::process::id();
    let content = std::fs::read_to_string(pid_file).unwrap_or_else(|e| {
        eprintln!("Failed to read PID file for verification: {}", e);
        std::process::exit(1);
    });

    let file_pid: u32 = content.trim().parse().unwrap_or_else(|_| {
        eprintln!("PID file contains invalid PID format: {}", content);
        std::process::exit(1);
    });

    if file_pid != current_pid {
        eprintln!(
            "PID file verification failed: expected {}, found {}",
            current_pid, file_pid
        );
        std::process::exit(1);
    }

    info!("PID file verification successful (PID: {})", current_pid);
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

fn setup_file_logging(log_dir: &PathBuf) {
    std::fs::create_dir_all(log_dir).unwrap_or_else(|e| {
        eprintln!("Failed to create log directory: {}", e);
        std::process::exit(1);
    });

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kronid")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
        .unwrap_or_else(|e| {
            eprintln!("Failed to create log appender: {}", e);
            std::process::exit(1);
        });

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
}

fn load_app_config() -> AppConfig {
    match AppConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    }
}

fn create_data_store(
    config: &AppConfig,
    snapshot_bus: &Arc<SnapshotBus>,
    thread_registry: &kronical_common::threading::ThreadRegistry,
) -> Box<dyn StorageBackend> {
    let data_file = match config.db_backend {
        DatabaseBackendConfig::Duckdb => config.workspace_dir.join("data.duckdb"),
        DatabaseBackendConfig::Sqlite3 => config.workspace_dir.join("data.sqlite3"),
    };

    match config.db_backend {
        DatabaseBackendConfig::Duckdb => DuckDbStorage::new_with_limit(
            &data_file,
            config.duckdb_memory_limit_mb_main,
            Arc::clone(snapshot_bus),
            thread_registry.clone(),
        )
        .map(|s| Box::new(s) as Box<dyn StorageBackend>)
        .unwrap_or_else(|e| {
            error!("Failed to initialize DuckDB store: {}", e);
            std::process::exit(1);
        }),
        DatabaseBackendConfig::Sqlite3 => SqliteStorage::new(
            &data_file,
            Arc::clone(snapshot_bus),
            thread_registry.clone(),
        )
        .map(|s| Box::new(s) as Box<dyn StorageBackend>)
        .unwrap_or_else(|e| {
            error!("Failed to initialize SQLite store: {}", e);
            std::process::exit(1);
        }),
    }
}

fn main() {
    let config = load_app_config();
    ensure_workspace_dir(&config.workspace_dir);

    let log_dir = config.workspace_dir.join("logs");
    setup_file_logging(&log_dir);

    let pid_file = kronical::util::paths::pid_file(&config.workspace_dir);
    write_pid_file(&pid_file);
    verify_pid_file(&pid_file);

    info!("Starting Kronical daemon (kronid)");

    let snapshot_bus = Arc::new(SnapshotBus::new());
    let coordinator = EventCoordinator::from_app_config(&config);
    let thread_registry = coordinator.thread_registry();
    let data_store = create_data_store(&config, &snapshot_bus, &thread_registry);

    info!("Kronical daemon will run on MAIN THREAD (required by macOS hooks)");
    let result = coordinator.start_main_thread(
        data_store,
        config.workspace_dir.clone(),
        Arc::clone(&snapshot_bus),
    );

    cleanup_pid_file(&pid_file);

    if let Err(e) = result {
        error!("Kronical daemon error: {}", e);
        std::process::exit(1);
    }
}

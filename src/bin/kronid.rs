use anyhow::{Context, Result};
use kronical::daemon::coordinator::EventCoordinator;
use kronical::daemon::snapshot::SnapshotBus;
use kronical::storage::StorageBackend;
use kronical::storage::duckdb::DuckDbStorage;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::util::config::{AppConfig, DatabaseBackendConfig};
use log::{error, info};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn ensure_workspace_dir(workspace_dir: &Path) -> Result<()> {
    if !workspace_dir.exists() {
        fs::create_dir_all(workspace_dir)
            .with_context(|| format!("creating workspace dir {}", workspace_dir.display()))?;
    }
    Ok(())
}

fn is_process_running(pid: u32) -> Result<bool> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .with_context(|| format!("invoking 'ps -p {}'", pid))?;
    Ok(out.status.success())
}

fn write_pid_file(pid_file: &Path) -> Result<()> {
    if pid_file.exists() {
        match fs::read_to_string(pid_file) {
            Ok(content) => {
                if let Ok(existing_pid) = content.trim().parse::<u32>() {
                    if is_process_running(existing_pid).context("checking existing pid")? {
                        anyhow::bail!("Kronical daemon is already running (PID: {})", existing_pid);
                    } else {
                        info!(
                            "Removing stale PID file (process {} no longer exists)",
                            existing_pid
                        );
                        let _ = fs::remove_file(pid_file);
                    }
                } else {
                    info!("Removing unreadable PID file (invalid format)");
                    let _ = fs::remove_file(pid_file);
                }
            }
            Err(_) => {
                info!("Removing unreadable PID file");
                let _ = fs::remove_file(pid_file);
            }
        }
    }

    let current_pid = std::process::id().to_string();
    let mut tmp = pid_file.to_path_buf();
    tmp.set_extension("pid.tmp");

    fs::write(&tmp, current_pid.as_bytes())
        .with_context(|| format!("writing temp pid file {}", tmp.display()))?;
    fs::rename(&tmp, pid_file)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), pid_file.display()))?;

    Ok(())
}

fn verify_pid_file(pid_file: &Path) -> Result<()> {
    let current_pid = std::process::id();
    let content = fs::read_to_string(pid_file)
        .with_context(|| format!("reading pid file {}", pid_file.display()))?;

    let file_pid: u32 = content
        .trim()
        .parse()
        .with_context(|| format!("parsing pid from {}", content))?;

    if file_pid != current_pid {
        anyhow::bail!(
            "PID file verification failed: expected {}, found {}",
            current_pid,
            file_pid
        );
    }

    info!("PID file verification successful (PID: {})", current_pid);
    Ok(())
}

fn cleanup_pid_file(pid_file: &Path) -> Result<()> {
    if !pid_file.exists() {
        error!("PID file disappeared during runtime");
        return Ok(());
    }

    let current_pid = std::process::id();
    let content = fs::read_to_string(pid_file)
        .with_context(|| format!("reading pid file {}", pid_file.display()))?;

    match content.trim().parse::<u32>() {
        Ok(file_pid) => {
            if file_pid == current_pid {
                fs::remove_file(pid_file)
                    .with_context(|| format!("removing pid file {}", pid_file.display()))?;
                info!("Successfully cleaned up PID file");
            } else {
                anyhow::bail!(
                    "PID file belongs to a different process ({}), current is {}",
                    file_pid,
                    current_pid
                );
            }
        }
        Err(e) => {
            anyhow::bail!("PID file contains invalid PID '{}': {}", content, e);
        }
    }

    Ok(())
}

fn setup_file_logging(log_dir: &Path) -> Result<()> {
    fs::create_dir_all(log_dir)
        .with_context(|| format!("creating log directory {}", log_dir.display()))?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kronid")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
        .with_context(|| format!("creating file appender in {}", log_dir.display()))?;

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
        .try_init()
        .ok();

    Ok(())
}

fn load_app_config() -> Result<AppConfig> {
    AppConfig::load().context("loading application configuration")
}

fn create_data_store(
    config: &AppConfig,
    snapshot_bus: &Arc<SnapshotBus>,
    thread_registry: &kronical_common::threading::ThreadRegistry,
) -> Result<Box<dyn StorageBackend>> {
    let data_file = match config.db_backend {
        DatabaseBackendConfig::Duckdb => config.workspace_dir.join("data.duckdb"),
        DatabaseBackendConfig::Sqlite3 => config.workspace_dir.join("data.sqlite3"),
    };

    let boxed: Box<dyn StorageBackend> = match config.db_backend {
        DatabaseBackendConfig::Duckdb => DuckDbStorage::new_with_limit(
            &data_file,
            config.duckdb_memory_limit_mb_main,
            Arc::clone(snapshot_bus),
            thread_registry.clone(),
        )
        .with_context(|| format!("initializing DuckDB store at {}", data_file.display()))?
        .into(),
        DatabaseBackendConfig::Sqlite3 => SqliteStorage::new(
            &data_file,
            Arc::clone(snapshot_bus),
            thread_registry.clone(),
        )
        .with_context(|| format!("initializing SQLite store at {}", data_file.display()))?
        .into(),
    };

    Ok(boxed)
}

struct PidFileGuard {
    path: PathBuf,
    active: bool,
}
impl PidFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }
    fn disarm(mut self) {
        self.active = false;
    }
}
impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(content) = fs::read_to_string(&self.path) {
            if let Ok(file_pid) = content.trim().parse::<u32>() {
                if file_pid == std::process::id() {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
    }
}

fn run() -> Result<()> {
    let config = load_app_config()?;

    ensure_workspace_dir(&config.workspace_dir)?;
    let log_dir = config.workspace_dir.join("logs");
    setup_file_logging(&log_dir)?;

    let pid_file = kronical::util::paths::pid_file(&config.workspace_dir);
    write_pid_file(&pid_file)?;
    verify_pid_file(&pid_file)?;
    let guard = PidFileGuard::new(pid_file.clone());

    info!("Starting Kronical daemon (kronid)");

    let snapshot_bus = Arc::new(SnapshotBus::new());
    let coordinator = EventCoordinator::initialize(&config);
    let thread_registry = coordinator.thread_registry();
    let data_store = create_data_store(&config, &snapshot_bus, &thread_registry)?;

    info!("Kronical daemon will run on MAIN THREAD (required by macOS hooks)");

    coordinator
        .start_main_thread(
            data_store,
            config.workspace_dir.clone(),
            Arc::clone(&snapshot_bus),
        )
        .context("coordinator main thread exited with error")?;

    cleanup_pid_file(&pid_file)?;
    guard.disarm();

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        error!("Kronical daemon error: {:#}", e);
        eprintln!("fatal: {}", e);
        std::process::exit(1);
    }
}

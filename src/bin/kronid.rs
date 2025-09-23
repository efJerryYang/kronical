use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{error, info};
use tracing::subscriber::SetGlobalDefaultError;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use kronical::daemon::coordinator::EventCoordinator;
use kronical::daemon::snapshot::SnapshotBus;
use kronical::storage::StorageBackend;
use kronical::storage::duckdb::DuckDbStorage;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::util::config::{AppConfig, DatabaseBackendConfig};

fn ensure_workspace_dir(workspace_dir: &Path) -> Result<()> {
    fs::create_dir_all(workspace_dir)
        .with_context(|| format!("creating workspace dir {}", workspace_dir.display()))
}

fn is_process_running(pid: u32) -> Result<bool> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .with_context(|| format!("invoking 'ps -p {}'", pid))?;
    Ok(out.status.success())
}

fn write_pid_file(pid_file: &Path) -> Result<PathBuf> {
    let lock_path = pid_file.with_extension("pid.lock");

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
                        let _ = fs::remove_file(&lock_path);
                    }
                } else {
                    info!("Removing unreadable PID file (invalid format)");
                    let _ = fs::remove_file(pid_file);
                    let _ = fs::remove_file(&lock_path);
                }
            }
            Err(_) => {
                info!("Removing unreadable PID file");
                let _ = fs::remove_file(pid_file);
                let _ = fs::remove_file(&lock_path);
            }
        }
    } else if lock_path.exists() {
        info!("Removing stale PID lock file");
        let _ = fs::remove_file(&lock_path);
    }

    let mut lock_opts = OpenOptions::new();
    lock_opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        lock_opts.mode(0o644);
    }
    let lock = lock_opts
        .open(&lock_path)
        .with_context(|| format!("creating lock {}", lock_path.display()))?;
    drop(lock);

    let mut tmp = pid_file.to_path_buf();
    tmp.set_extension("pid.tmp");
    let current_pid = std::process::id();

    let write_result = (|| -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            opts.mode(0o644);
        }
        let mut tmp_file = opts
            .open(&tmp)
            .with_context(|| format!("opening temp pid file {}", tmp.display()))?;
        write!(tmp_file, "{}", current_pid)
            .with_context(|| format!("writing pid {}", current_pid))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("syncing temp pid file {}", tmp.display()))?;
        drop(tmp_file);
        fs::rename(&tmp, pid_file)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), pid_file.display()))?;
        Ok(())
    })();

    match write_result {
        Ok(()) => Ok(lock_path),
        Err(e) => {
            let _ = fs::remove_file(&lock_path);
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn verify_pid_file(pid_file: &Path) -> Result<()> {
    let current_pid = std::process::id();
    let content = fs::read_to_string(pid_file)
        .with_context(|| format!("reading pid file {}", pid_file.display()))?;

    let file_pid: u32 = content
        .trim()
        .parse()
        .with_context(|| format!("parsing pid (len {})", content.len()))?;

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

    match tracing_subscriber::registry()
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
    {
        Ok(()) => Ok(()),
        Err(e) => {
            if e.source()
                .and_then(|source| source.downcast_ref::<SetGlobalDefaultError>())
                .is_some()
            {
                Ok(())
            } else {
                Err(e.into())
            }
        }
    }
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

    match config.db_backend {
        DatabaseBackendConfig::Duckdb => Ok(Box::new(
            DuckDbStorage::new_with_limit(
                &data_file,
                config.duckdb_memory_limit_mb_main,
                Arc::clone(snapshot_bus),
                thread_registry.clone(),
            )
            .with_context(|| format!("initializing DuckDB store at {}", data_file.display()))?,
        ) as Box<dyn StorageBackend>),
        DatabaseBackendConfig::Sqlite3 => Ok(Box::new(
            SqliteStorage::new(
                &data_file,
                Arc::clone(snapshot_bus),
                thread_registry.clone(),
            )
            .with_context(|| format!("initializing SQLite store at {}", data_file.display()))?,
        ) as Box<dyn StorageBackend>),
    }
}

struct PidFileGuard {
    pid_path: PathBuf,
    lock_path: PathBuf,
    active: bool,
}
impl PidFileGuard {
    fn new(pid_path: PathBuf, lock_path: PathBuf) -> Self {
        Self {
            pid_path,
            lock_path,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}
impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if self.active {
            if let Ok(content) = fs::read_to_string(&self.pid_path) {
                if let Ok(file_pid) = content.trim().parse::<u32>() {
                    if file_pid == std::process::id() {
                        let _ = fs::remove_file(&self.pid_path);
                    }
                }
            }
        }
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn run() -> Result<()> {
    let config = load_app_config()?;

    ensure_workspace_dir(&config.workspace_dir)?;
    let log_dir = config.workspace_dir.join("logs");
    setup_file_logging(&log_dir)?;

    let pid_file = kronical::util::paths::pid_file(&config.workspace_dir);
    let lock_path = write_pid_file(&pid_file)?;
    verify_pid_file(&pid_file)?;
    let mut guard = PidFileGuard::new(pid_file.clone(), lock_path);

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

    // start_main_thread blocks until all workers stop, so it is safe to remove the PID now.
    cleanup_pid_file(&pid_file)?;
    guard.disarm();

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        error!("Kronical daemon error: {}", e);
        eprintln!("fatal: {}", e);
        std::process::exit(1);
    }
}

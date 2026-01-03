use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::util::logging::{debug, error, info, warn};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::{Sender, TryRecvError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

pub use kronical_storage::system_metrics::{
    DuckDbSystemMetricsStore, SqliteSystemMetricsStore, SystemMetrics, SystemMetricsStore,
};

// Query channel types to run reads on the same in-process DuckDB instance
// that the tracker uses.

#[derive(Debug)]
pub enum MetricsQuery {
    ByLimit {
        pid: u32,
        limit: usize,
    },
    ByRange {
        pid: u32,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
}

#[derive(Debug)]
pub struct MetricsQueryReq {
    pub query: MetricsQuery,
    pub reply: oneshot::Sender<Result<Vec<SystemMetrics>, String>>,
}

use std::thread;

pub struct SystemTracker {
    pid: u32,
    interval: Duration,
    batch_size: usize,
    db_path: PathBuf,
    started: AtomicBool,
    control_tx: Option<Sender<ControlMsg>>,
    query_tx: Option<Sender<MetricsQueryReq>>,
    control_bridge: Option<ThreadHandle>,
    query_bridge: Option<ThreadHandle>,
    backend: crate::util::config::DatabaseBackendConfig,
    duckdb_memory_limit_mb: u64,
    thread_handle: Option<ThreadHandle>,
}

#[derive(Debug, Clone)]
pub enum ControlMsg {
    Stop,
    /// Request an immediate flush of buffered samples. The tracker will
    /// capture the current iteration's sample then persist all buffered rows
    /// and acknowledge via the provided channel.
    Flush(Sender<()>),
}

#[derive(Debug)]
pub enum ControlRequest {
    Stop,
    Flush(oneshot::Sender<()>),
}

impl SystemTracker {
    pub fn new(
        pid: u32,
        interval_secs: f64,
        batch_size: usize,
        db_path: PathBuf,
        backend: crate::util::config::DatabaseBackendConfig,
        duckdb_memory_limit_mb: u64,
    ) -> Self {
        Self {
            pid,
            interval: Duration::from_secs_f64(interval_secs),
            batch_size,
            db_path,
            started: AtomicBool::new(false),
            control_tx: None,
            query_tx: None,
            control_bridge: None,
            query_bridge: None,
            backend,
            duckdb_memory_limit_mb,
            thread_handle: None,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        self.start_with_registry(ThreadRegistry::new())
    }

    pub fn start_with_registry(&mut self, threads: ThreadRegistry) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(anyhow::anyhow!("System tracker is already running"));
        }

        info!(
            "Starting system tracker for PID {} with interval {:?} (backend: {:?})",
            self.pid, self.interval, self.backend
        );

        let pid = self.pid;
        let interval = self.interval;
        let batch_size = self.batch_size;
        let db_path = self.db_path.clone();
        let backend = self.backend.clone();
        let duckdb_limit = self.duckdb_memory_limit_mb;
        let (control_tx, control_rx) = crossbeam_channel::unbounded::<ControlMsg>();
        self.control_tx = Some(control_tx.clone());

        let (query_tx, query_rx) = crossbeam_channel::unbounded::<MetricsQueryReq>();
        self.query_tx = Some(query_tx.clone());

        // Bridge async API requests into the tracker thread via tokio channels.
        let (async_control_tx, mut async_control_rx) = tokio_mpsc::channel::<ControlRequest>(32);
        crate::daemon::api::set_system_tracker_control_tx(async_control_tx.clone());
        let control_forward = threads.spawn("system-tracker-control-bridge", move || {
            while let Some(msg) = async_control_rx.blocking_recv() {
                match msg {
                    ControlRequest::Stop => {
                        if control_tx.send(ControlMsg::Stop).is_err() {
                            break;
                        }
                    }
                    ControlRequest::Flush(ack) => {
                        let (cb_tx, cb_rx) = crossbeam_channel::bounded(1);
                        match control_tx.send(ControlMsg::Flush(cb_tx)) {
                            Ok(_) => {
                                if cb_rx.recv().is_err() {
                                    let _ = ack.send(());
                                    break;
                                } else {
                                    let _ = ack.send(());
                                }
                            }
                            Err(err) => {
                                if let ControlMsg::Flush(_) = err.into_inner() {
                                    let _ = ack.send(());
                                }
                                break;
                            }
                        }
                    }
                }
            }
        })?;
        self.control_bridge = Some(control_forward);

        let (async_query_tx, mut async_query_rx) = tokio_mpsc::channel::<MetricsQueryReq>(64);
        crate::daemon::api::set_system_tracker_query_tx(async_query_tx.clone());
        let query_forward = threads.spawn("system-tracker-query-bridge", move || {
            while let Some(msg) = async_query_rx.blocking_recv() {
                match query_tx.send(msg) {
                    Ok(_) => {}
                    Err(err) => {
                        let msg = err.into_inner();
                        let _ = msg.reply.send(Err("tracker offline".to_string()));
                        break;
                    }
                }
            }
        })?;
        self.query_bridge = Some(query_forward);

        let handle = threads
            .spawn("system-tracker", move || {
                let mut store: Box<dyn SystemMetricsStore + Send> = match backend {
                    crate::util::config::DatabaseBackendConfig::Duckdb => {
                        match DuckDbSystemMetricsStore::new_file_with_limit(&db_path, duckdb_limit)
                        {
                            Ok(s) => Box::new(s),
                            Err(e) => {
                                error!("Failed to create DuckDB store: {}", e);
                                return;
                            }
                        }
                    }
                    crate::util::config::DatabaseBackendConfig::Sqlite3 => {
                        match SqliteSystemMetricsStore::new_file(&db_path) {
                            Ok(s) => Box::new(s),
                            Err(e) => {
                                error!("Failed to create SQLite store: {}", e);
                                return;
                            }
                        }
                    }
                };

                let mut metrics_buffer = Vec::new();
                let mut last_disk_io = 0u64;
                let mut batch_count = 0;

                let flush_buffer =
                    |buffer: &mut Vec<SystemMetrics>,
                     store: &mut Box<dyn SystemMetricsStore + Send>| {
                        if !buffer.is_empty() {
                            if let Err(e) = store.insert_metrics_batch(buffer, pid) {
                                error!("Failed to flush metrics batch: {}", e);
                            } else {
                                debug!(
                                    "Flushed {} metrics to metrics store",
                                    buffer.len()
                                );
                                buffer.clear();
                            }
                        }
                    };

                if let Ok((cpu_percent, memory_bytes, disk_io_delta, current_total_disk_io)) =
                    collect_system_metrics(pid, last_disk_io)
                {
                    last_disk_io = current_total_disk_io;
                    metrics_buffer.push(SystemMetrics {
                        timestamp: Utc::now(),
                        cpu_percent,
                        memory_bytes,
                        disk_io_bytes: disk_io_delta,
                    });
                    batch_count += 1;
                }

                let mut running = true;
                let mut pending_flush_acks: Vec<Sender<()>> = Vec::new();
                while running {
                    loop {
                        match control_rx.try_recv() {
                            Ok(ControlMsg::Stop) => {
                                running = false;
                            }
                            Ok(ControlMsg::Flush(ack_tx)) => {
                                pending_flush_acks.push(ack_tx);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                running = false;
                                break;
                            }
                        }
                    }

                    let start_time = std::time::Instant::now();

                    match collect_system_metrics(pid, last_disk_io) {
                        Ok((cpu_percent, memory_bytes, disk_io_delta, current_total_disk_io)) => {
                            last_disk_io = current_total_disk_io;

                            let metrics = SystemMetrics {
                                timestamp: Utc::now(),
                                cpu_percent,
                                memory_bytes,
                                disk_io_bytes: disk_io_delta,
                            };

                            metrics_buffer.push(metrics);
                            batch_count += 1;

                            if batch_count % batch_size == 0 {
                                if let Err(e) = store.insert_metrics_batch(&metrics_buffer, pid) {
                                    error!("Failed to insert metrics batch: {}", e);
                                } else {
                                    debug!(
                                        "Inserted {} metrics to metrics store",
                                        metrics_buffer.len()
                                    );
                                    metrics_buffer.clear();
                                }
                            }

                            if !pending_flush_acks.is_empty() {
                                info!(
                                    "Tracker flush requested; flushing {} buffered samples",
                                    metrics_buffer.len()
                                );
                                flush_buffer(&mut metrics_buffer, &mut store);
                                for ack in pending_flush_acks.drain(..) {
                                    let _ = ack.send(());
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to collect system metrics: {}", e);
                            if !pending_flush_acks.is_empty() {
                                info!(
                                    "Tracker flush requested (error path); flushing {} buffered samples",
                                    metrics_buffer.len()
                                );
                                flush_buffer(&mut metrics_buffer, &mut store);
                                for ack in pending_flush_acks.drain(..) {
                                    let _ = ack.send(());
                                }
                            }
                        }
                    }

                    loop {
                        match query_rx.try_recv() {
                            Ok(req) => {
                                flush_buffer(&mut metrics_buffer, &mut store);
                                let result = match req.query {
                                    MetricsQuery::ByLimit { pid, limit } => {
                                        let mut rows = match store.get_metrics_for_pid(pid, Some(limit))
                                        {
                                            Ok(v) => v,
                                            Err(e) => {
                                                let _ = req.reply.send(Err(e.to_string()));
                                                continue;
                                            }
                                        };
                                        rows.reverse();
                                        Ok(rows)
                                    }
                                    MetricsQuery::ByRange { pid, start, end } => {
                                        match store.get_metrics_in_time_range(pid, start, end) {
                                            Ok(v) => Ok(v),
                                            Err(e) => Err(e.to_string()),
                                        }
                                    }
                                };
                                let _ = req.reply.send(result);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }

                    let elapsed = start_time.elapsed();
                    if elapsed < interval {
                        thread::sleep(interval - elapsed);
                    }
                }

                if !metrics_buffer.is_empty() {
                    if let Err(e) = store.insert_metrics_batch(&metrics_buffer, pid) {
                        error!("Failed to flush final metrics batch: {}", e);
                    } else {
                        debug!(
                            "Flushed final {} metrics to metrics store",
                            metrics_buffer.len()
                        );
                    }
                }
            })
            .context("spawn system tracker thread")?;

        self.thread_handle = Some(handle);

        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        if !self.started.swap(false, Ordering::SeqCst) {
            return Err(anyhow::anyhow!("System tracker is not running"));
        }
        if let Some(tx) = &self.control_tx {
            let _ = tx.send(ControlMsg::Stop);
        }
        if let Some(handle) = self.thread_handle.take() {
            if let Err(e) = handle.join() {
                error!("System tracker thread panicked: {:?}", e);
            }
        }
        if let Some(handle) = self.control_bridge.take() {
            if let Err(e) = handle.join() {
                error!("Control bridge thread panicked: {:?}", e);
            }
        }
        if let Some(handle) = self.query_bridge.take() {
            if let Err(e) = handle.join() {
                error!("Query bridge thread panicked: {:?}", e);
            }
        }
        self.control_tx = None;
        self.query_tx = None;
        info!("System tracker stop requested");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    pub fn flush(&self) -> Result<()> {
        let tx = self
            .control_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Tracker control channel not available"))?;
        let (ack_tx, ack_rx) = crossbeam_channel::bounded(1);
        tx.send(ControlMsg::Flush(ack_tx))
            .map_err(|_| anyhow::anyhow!("Failed to send flush control message"))?;
        let _ = ack_rx
            .recv_timeout(std::time::Duration::from_millis(2000))
            .map_err(|_| anyhow::anyhow!("Timed out waiting for tracker flush ack"))?;
        Ok(())
    }
}

fn collect_system_metrics(pid: u32, last_disk_io: u64) -> Result<(f64, u64, u64, u64)> {
    let cpu_percent = get_cpu_usage(pid)?;
    let memory_bytes = get_memory_usage(pid)?;
    let current_disk_io = get_disk_io(pid)?;

    let disk_io_delta = if current_disk_io >= last_disk_io {
        current_disk_io - last_disk_io
    } else {
        current_disk_io
    };

    Ok((cpu_percent, memory_bytes, disk_io_delta, current_disk_io))
}

fn get_cpu_usage(pid: u32) -> Result<f64> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-o", "%cpu=", "-p", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("ps command failed"));
    }

    let cpu_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    cpu_str
        .parse::<f64>()
        .map_err(|e| anyhow::anyhow!("Failed to parse CPU usage: {}", e))
}

fn get_memory_usage(pid: u32) -> Result<u64> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("ps command failed"));
    }

    let rss_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let rss_kb: u64 = rss_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse RSS: {}", e))?;

    Ok(rss_kb * 1024)
}

use libc::pid_t;

#[allow(non_camel_case_types)]
mod libproc_bindings {
    include!(concat!(env!("OUT_DIR"), "/libproc_bindings.rs"));
}

fn get_disk_io(pid: u32) -> Result<u64> {
    use libproc_bindings::*;
    use std::mem::MaybeUninit;

    unsafe {
        let mut rusage_info = MaybeUninit::<rusage_info_v4>::uninit();
        let mut ptr = rusage_info.as_mut_ptr() as rusage_info_t;
        let result = proc_pid_rusage(pid as pid_t, RUSAGE_INFO_V4 as i32, &mut ptr);

        if result == 0 {
            let info = rusage_info.assume_init();
            Ok(info.ri_diskio_bytesread + info.ri_diskio_byteswritten)
        } else {
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_tracker_lifecycle() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("tracker_test.duckdb");

        let mut tracker = SystemTracker::new(
            1234,
            1.0,
            10,
            db_path.clone(),
            crate::util::config::DatabaseBackendConfig::Duckdb,
            10,
        );

        assert!(!tracker.is_running());

        tracker.start()?;
        assert!(tracker.is_running());

        tracker.flush()?;

        tracker.stop()?;
        assert!(!tracker.is_running());

        Ok(())
    }
}

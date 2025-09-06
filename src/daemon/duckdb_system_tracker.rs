use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

use duckdb::{Connection, params};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_io_bytes: u64,
}

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
    pub reply: std::sync::mpsc::Sender<Result<Vec<SystemMetrics>, String>>,
}

use std::thread;

pub struct DuckDbSystemTracker {
    pid: u32,
    interval: Duration,
    batch_size: usize,
    db_path: PathBuf,
    running: Arc<Mutex<bool>>,
    flush_signal_path: PathBuf,
}

impl DuckDbSystemTracker {
    pub fn new(pid: u32, interval_secs: f64, batch_size: usize, db_path: PathBuf) -> Self {
        let flush_signal_path = db_path.with_file_name("tracker-flush-signal");
        Self {
            pid,
            interval: Duration::from_secs_f64(interval_secs),
            batch_size,
            db_path,
            running: Arc::new(Mutex::new(false)),
            flush_signal_path,
        }
    }

    pub fn start(&self) -> Result<()> {
        {
            let mut running = self.running.lock().unwrap();
            if *running {
                return Err(anyhow::anyhow!("DuckDB system tracker is already running"));
            }
            *running = true;
        }

        info!(
            "Starting DuckDB system tracker for PID {} with interval {:?}",
            self.pid, self.interval
        );

        let pid = self.pid;
        let interval = self.interval;
        let batch_size = self.batch_size;
        let db_path = self.db_path.clone();
        let flush_signal_path = self.flush_signal_path.clone();
        let running = Arc::clone(&self.running);

        // Create the DuckDB store instance once within this thread.
        // Also set up a query channel so the gRPC server can ask this
        // thread to execute reads on the same in-process instance.
        let (query_tx, query_rx) = mpsc::channel::<MetricsQueryReq>();

        crate::daemon::kroni_server::set_system_tracker_query_tx(query_tx.clone());

        thread::spawn(move || {
            let mut store = match DuckDbSystemMetricsStore::new_file(&db_path) {
                Ok(store) => store,
                Err(e) => {
                    tracing::error!("Failed to create DuckDB store: {}", e);
                    return;
                }
            };

            let mut metrics_buffer = Vec::new();
            let mut last_disk_io = 0u64;
            let mut batch_count = 0;

            let flush_buffer =
                |buffer: &mut Vec<SystemMetrics>, store: &mut DuckDbSystemMetricsStore| {
                    if !buffer.is_empty() {
                        if let Err(e) = store.insert_metrics_batch(buffer, pid) {
                            tracing::error!("Failed to flush metrics batch: {}", e);
                        } else {
                            tracing::debug!("Flushed {} metrics to DuckDB", buffer.len());
                            buffer.clear();
                        }
                    }
                };

            while *running.lock().unwrap() {
                // Detect flush request and defer removing the signal until after we
                // have also captured this iteration's sample, so the caller sees it.
                let flush_requested = flush_signal_path.exists();

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
                                tracing::error!("Failed to insert metrics batch: {}", e);
                            } else {
                                tracing::debug!(
                                    "Inserted {} metrics to DuckDB",
                                    metrics_buffer.len()
                                );
                                metrics_buffer.clear();
                            }
                        }

                        if flush_requested {
                            tracing::info!(
                                "Tracker flush requested (normal path); flushing {} buffered samples",
                                metrics_buffer.len()
                            );
                            flush_buffer(&mut metrics_buffer, &mut store);
                            let _ = std::fs::remove_file(&flush_signal_path);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to collect system metrics: {}", e);
                        // Even if collection failed, honor a pending flush to persist any buffered samples
                        if flush_requested {
                            tracing::info!(
                                "Tracker flush requested (error path); flushing {} buffered samples",
                                metrics_buffer.len()
                            );
                            flush_buffer(&mut metrics_buffer, &mut store);
                            let _ = std::fs::remove_file(&flush_signal_path);
                        }
                    }
                }

                // Process any pending queries from the gRPC server quickly.
                loop {
                    match query_rx.try_recv() {
                        Ok(req) => {
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
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }

                let elapsed = start_time.elapsed();
                if elapsed < interval {
                    thread::sleep(interval - elapsed);
                }
            }

            if !metrics_buffer.is_empty() {
                if let Err(e) = store.insert_metrics_batch(&metrics_buffer, pid) {
                    tracing::error!("Failed to flush final metrics batch: {}", e);
                } else {
                    tracing::debug!("Flushed final {} metrics to DuckDB", metrics_buffer.len());
                }
            }
        });

        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        {
            let mut running = self.running.lock().unwrap();
            if !*running {
                return Err(anyhow::anyhow!("DuckDB system tracker is not running"));
            }
            *running = false;
        }

        info!("DuckDB system tracker stop requested");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().unwrap()
    }

    pub fn flush(&self) -> Result<()> {
        std::fs::write(&self.flush_signal_path, "")
            .map_err(|e| anyhow::anyhow!("Failed to create flush signal file: {}", e))?;
        Ok(())
    }
}

pub fn trigger_tracker_flush(workspace_dir: &PathBuf) -> Result<()> {
    let flush_signal_path = workspace_dir.join("tracker-flush-signal");
    std::fs::write(&flush_signal_path, "")
        .map_err(|e| anyhow::anyhow!("Failed to create flush signal file: {}", e))?;
    Ok(())
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

pub struct DuckDbSystemMetricsStore {
    connection: Connection,
}

impl DuckDbSystemMetricsStore {
    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;

        conn.execute_batch(
            r"CREATE TABLE system_metrics (
                timestamp TIMESTAMPTZ NOT NULL,
                pid INTEGER NOT NULL,
                cpu_percent DOUBLE NOT NULL,
                memory_bytes BIGINT NOT NULL,
                disk_io_bytes BIGINT NOT NULL,
                PRIMARY KEY (timestamp, pid)
            );
            CREATE INDEX idx_system_metrics_timestamp ON system_metrics(timestamp);
            CREATE INDEX idx_system_metrics_pid ON system_metrics(pid);
            ",
        )?;

        Ok(Self { connection: conn })
    }

    pub fn new_file(db_path: &PathBuf) -> Result<Self> {
        let conn = Connection::open(db_path)?;

        conn.execute_batch(
            r"CREATE TABLE IF NOT EXISTS system_metrics (
                timestamp TIMESTAMPTZ NOT NULL,
                pid INTEGER NOT NULL,
                cpu_percent DOUBLE NOT NULL,
                memory_bytes BIGINT NOT NULL,
                disk_io_bytes BIGINT NOT NULL,
                PRIMARY KEY (timestamp, pid)
            );
            CREATE INDEX IF NOT EXISTS idx_system_metrics_timestamp ON system_metrics(timestamp);
            CREATE INDEX IF NOT EXISTS idx_system_metrics_pid ON system_metrics(pid);
            ",
        )?;

        Ok(Self { connection: conn })
    }

    pub fn insert_metrics_batch(&mut self, metrics: &[SystemMetrics], pid: u32) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }

        // Use an explicit transaction to avoid per-row autocommit overhead
        self.connection.execute_batch("BEGIN TRANSACTION;")?;
        let mut stmt = self.connection.prepare(
            "INSERT INTO system_metrics (timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes) 
             VALUES (?, ?, ?, ?, ?)",
        )?;

        for metric in metrics {
            stmt.execute(params![
                metric.timestamp,
                pid,
                metric.cpu_percent,
                metric.memory_bytes,
                metric.disk_io_bytes
            ])?;
        }
        self.connection.execute_batch("COMMIT;")?;

        Ok(())
    }

    pub fn get_metrics_for_pid(
        &self,
        pid: u32,
        limit: Option<usize>,
    ) -> Result<Vec<SystemMetrics>> {
        let mut sql = "SELECT timestamp, cpu_percent, memory_bytes, disk_io_bytes 
                       FROM system_metrics WHERE pid = ? ORDER BY timestamp DESC"
            .to_string();

        if let Some(limit_val) = limit {
            sql.push_str(&format!(" LIMIT {}", limit_val));
        }

        let mut stmt = self.connection.prepare(&sql)?;
        let rows = stmt.query_map(params![pid], |row| {
            Ok(SystemMetrics {
                timestamp: row.get(0)?,
                cpu_percent: row.get(1)?,
                memory_bytes: row.get(2)?,
                disk_io_bytes: row.get(3)?,
            })
        })?;

        let mut metrics = Vec::new();
        for row in rows {
            metrics.push(row?);
        }

        Ok(metrics)
    }

    pub fn get_metrics_in_time_range(
        &self,
        pid: u32,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Result<Vec<SystemMetrics>> {
        let mut stmt = self.connection.prepare(
            "SELECT timestamp, cpu_percent, memory_bytes, disk_io_bytes 
             FROM system_metrics 
             WHERE pid = ? AND timestamp >= ? AND timestamp <= ? 
             ORDER BY timestamp ASC",
        )?;

        let rows = stmt.query_map(params![pid, start_time, end_time], |row| {
            Ok(SystemMetrics {
                timestamp: row.get(0)?,
                cpu_percent: row.get(1)?,
                memory_bytes: row.get(2)?,
                disk_io_bytes: row.get(3)?,
            })
        })?;

        let mut metrics = Vec::new();
        for row in rows {
            metrics.push(row?);
        }

        Ok(metrics)
    }

    pub fn get_aggregated_metrics(
        &self,
        pid: u32,
        since_timestamp: DateTime<Utc>,
    ) -> Result<(f64, u64, u64)> {
        // AVG over BIGINT yields DOUBLE in DuckDB; cast the integer averages back to BIGINT
        let mut stmt = self.connection.prepare(
            "SELECT 
                AVG(cpu_percent)                AS avg_cpu,
                CAST(AVG(memory_bytes) AS BIGINT) AS avg_mem,
                CAST(AVG(disk_io_bytes) AS BIGINT) AS avg_io
             FROM system_metrics 
             WHERE pid = ? AND timestamp >= ?",
        )?;

        let row = stmt.query_row(params![pid, since_timestamp], |row| {
            Ok((
                row.get::<_, f64>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;

        Ok(row)
    }

    pub fn cleanup_old_metrics(&mut self, cutoff_timestamp: DateTime<Utc>) -> Result<usize> {
        let mut stmt = self
            .connection
            .prepare("DELETE FROM system_metrics WHERE timestamp < ?")?;

        let deleted_count = stmt.execute(params![cutoff_timestamp])?;

        Ok(deleted_count)
    }

    pub fn get_total_metrics_count(&self) -> Result<i64> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM system_metrics", [], |row| row.get(0))?;

        Ok(count)
    }

    pub fn vacuum_database(&mut self) -> Result<()> {
        self.connection.execute("VACUUM", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};
    use std::time::Instant;
    use tempfile::tempdir;

    fn create_test_metrics(count: usize, start_time: DateTime<Utc>) -> Vec<SystemMetrics> {
        (0..count)
            .map(|i| SystemMetrics {
                timestamp: start_time + ChronoDuration::seconds(i as i64),
                cpu_percent: 10.0 + (i as f64 * 0.5),
                memory_bytes: 1024 * 1024 * (100 + i as u64),
                disk_io_bytes: 4096 * (10 + i as u64),
            })
            .collect()
    }

    #[test]
    fn test_in_memory_store_creation() -> Result<()> {
        let store = DuckDbSystemMetricsStore::new_in_memory()?;
        let count = store.get_total_metrics_count()?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn test_file_store_creation() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("test_metrics.duckdb");

        let store = DuckDbSystemMetricsStore::new_file(&db_path)?;
        let count = store.get_total_metrics_count()?;
        assert_eq!(count, 0);

        assert!(db_path.exists());
        Ok(())
    }

    #[test]
    fn test_insert_and_retrieve_metrics() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 1234u32;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let metrics = create_test_metrics(5, start_time);

        store.insert_metrics_batch(&metrics, test_pid)?;

        let retrieved_metrics = store.get_metrics_for_pid(test_pid, None)?;
        assert_eq!(retrieved_metrics.len(), 5);

        let first_metric = &retrieved_metrics[4];
        assert_eq!(first_metric.timestamp, start_time);
        assert_eq!(first_metric.cpu_percent, 10.0);
        assert_eq!(first_metric.memory_bytes, 1024 * 1024 * 100);
        assert_eq!(first_metric.disk_io_bytes, 4096 * 10);

        Ok(())
    }

    #[test]
    fn test_metrics_retrieval_with_limit() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 5678u32;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let metrics = create_test_metrics(10, start_time);

        store.insert_metrics_batch(&metrics, test_pid)?;

        let limited_metrics = store.get_metrics_for_pid(test_pid, Some(3))?;
        assert_eq!(limited_metrics.len(), 3);

        let last_metric = &limited_metrics[0];
        assert_eq!(
            last_metric.timestamp,
            start_time + ChronoDuration::seconds(9)
        );

        Ok(())
    }

    #[test]
    fn test_time_range_query() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 9999u32;
        let base_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let metrics = create_test_metrics(60, base_time);

        store.insert_metrics_batch(&metrics, test_pid)?;

        let start_time = base_time + ChronoDuration::seconds(10);
        let end_time = base_time + ChronoDuration::seconds(20);
        let range_metrics = store.get_metrics_in_time_range(test_pid, start_time, end_time)?;

        assert_eq!(range_metrics.len(), 11);
        assert_eq!(range_metrics[0].timestamp, start_time);
        assert_eq!(range_metrics[10].timestamp, end_time);

        Ok(())
    }

    #[test]
    fn test_multiple_pids() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();

        let metrics_pid_1 = create_test_metrics(3, start_time);
        let metrics_pid_2 = create_test_metrics(5, start_time + ChronoDuration::minutes(1));

        store.insert_metrics_batch(&metrics_pid_1, 1001)?;
        store.insert_metrics_batch(&metrics_pid_2, 1002)?;

        let pid_1_metrics = store.get_metrics_for_pid(1001, None)?;
        let pid_2_metrics = store.get_metrics_for_pid(1002, None)?;

        assert_eq!(pid_1_metrics.len(), 3);
        assert_eq!(pid_2_metrics.len(), 5);

        let total_count = store.get_total_metrics_count()?;
        assert_eq!(total_count, 8);

        Ok(())
    }

    #[test]
    fn test_batch_insert_performance() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 4321u32;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();

        let large_batch = create_test_metrics(1000, start_time);

        let insert_start = Instant::now();
        store.insert_metrics_batch(&large_batch, test_pid)?;
        let insert_duration = insert_start.elapsed();

        println!("Inserted 1000 metrics in {:?}", insert_duration);
        assert!(insert_duration < Duration::from_secs(1));

        let count = store.get_total_metrics_count()?;
        assert_eq!(count, 1000);

        Ok(())
    }

    #[test]
    fn test_aggregated_metrics() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 7777u32;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();

        let metrics = vec![
            SystemMetrics {
                timestamp: start_time,
                cpu_percent: 10.0,
                memory_bytes: 1000,
                disk_io_bytes: 100,
            },
            SystemMetrics {
                timestamp: start_time + ChronoDuration::seconds(30),
                cpu_percent: 20.0,
                memory_bytes: 2000,
                disk_io_bytes: 200,
            },
            SystemMetrics {
                timestamp: start_time + ChronoDuration::seconds(60),
                cpu_percent: 30.0,
                memory_bytes: 3000,
                disk_io_bytes: 300,
            },
        ];

        store.insert_metrics_batch(&metrics, test_pid)?;

        let (avg_cpu, avg_memory, avg_disk_io) =
            store.get_aggregated_metrics(test_pid, start_time)?;
        assert_eq!(avg_cpu, 20.0);
        assert_eq!(avg_memory, 2000);
        assert_eq!(avg_disk_io, 200);

        Ok(())
    }

    #[test]
    fn test_cleanup_old_metrics() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 8888u32;
        let old_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let recent_time = Utc.with_ymd_and_hms(2024, 1, 2, 12, 0, 0).unwrap();

        let old_metrics = create_test_metrics(5, old_time);
        let recent_metrics = create_test_metrics(3, recent_time);

        store.insert_metrics_batch(&old_metrics, test_pid)?;
        store.insert_metrics_batch(&recent_metrics, test_pid)?;

        let total_before = store.get_total_metrics_count()?;
        assert_eq!(total_before, 8);

        let cutoff_time = Utc.with_ymd_and_hms(2024, 1, 1, 18, 0, 0).unwrap();
        let deleted_count = store.cleanup_old_metrics(cutoff_time)?;
        assert_eq!(deleted_count, 5);

        let remaining_count = store.get_total_metrics_count()?;
        assert_eq!(remaining_count, 3);

        Ok(())
    }

    #[test]
    fn test_vacuum_operation() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let test_pid = 6666u32;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let metrics = create_test_metrics(100, start_time);

        store.insert_metrics_batch(&metrics, test_pid)?;
        store.vacuum_database()?;

        let count = store.get_total_metrics_count()?;
        assert_eq!(count, 100);

        Ok(())
    }

    #[test]
    fn test_concurrent_access_simulation() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let start_time = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();

        for pid in 1000..1005 {
            let metrics =
                create_test_metrics(10, start_time + ChronoDuration::minutes(pid as i64 - 1000));
            store.insert_metrics_batch(&metrics, pid)?;
        }

        let total_count = store.get_total_metrics_count()?;
        assert_eq!(total_count, 50);

        for pid in 1000..1005 {
            let pid_metrics = store.get_metrics_for_pid(pid, None)?;
            assert_eq!(pid_metrics.len(), 10);
        }

        Ok(())
    }

    #[test]
    fn test_tracker_lifecycle() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("tracker_test.duckdb");

        let tracker = DuckDbSystemTracker::new(1234, 1.0, 10, db_path.clone());

        assert!(!tracker.is_running());

        tracker.start()?;
        assert!(tracker.is_running());

        tracker.flush()?;

        tracker.stop()?;
        assert!(!tracker.is_running());

        Ok(())
    }

    #[test]
    fn test_error_handling_invalid_pid() -> Result<()> {
        let store = DuckDbSystemMetricsStore::new_in_memory()?;
        let nonexistent_pid = 999999u32;

        let metrics = store.get_metrics_for_pid(nonexistent_pid, None)?;
        assert_eq!(metrics.len(), 0);

        Ok(())
    }

    #[test]
    fn test_empty_batch_insert() -> Result<()> {
        let mut store = DuckDbSystemMetricsStore::new_in_memory()?;
        let empty_metrics: Vec<SystemMetrics> = vec![];

        store.insert_metrics_batch(&empty_metrics, 1234)?;

        let count = store.get_total_metrics_count()?;
        assert_eq!(count, 0);

        Ok(())
    }
}

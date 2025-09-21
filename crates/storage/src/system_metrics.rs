use anyhow::Result;
use chrono::{DateTime, Utc};
use duckdb::{Connection, params};
use rusqlite as rsql;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_io_bytes: u64,
}

pub trait SystemMetricsStore {
    fn insert_metrics_batch(&mut self, metrics: &[SystemMetrics], pid: u32) -> Result<()>;
    fn get_metrics_for_pid(&self, pid: u32, limit: Option<usize>) -> Result<Vec<SystemMetrics>>;
    fn get_metrics_in_time_range(
        &self,
        pid: u32,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Result<Vec<SystemMetrics>>;
}

pub struct DuckDbSystemMetricsStore {
    connection: Connection,
}

impl DuckDbSystemMetricsStore {
    pub fn new_in_memory() -> Result<Self> {
        Self::new_in_memory_with_limit(10)
    }

    pub fn new_file(db_path: &PathBuf) -> Result<Self> {
        Self::new_file_with_limit(db_path, 10)
    }

    pub fn new_in_memory_with_limit(limit_mb: u64) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(&format!(
            "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
            limit_mb
        ))?;

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

    pub fn new_file_with_limit(db_path: &PathBuf, limit_mb: u64) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(&format!(
            "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
            limit_mb
        ))?;

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

    pub fn get_aggregated_metrics(
        &self,
        pid: u32,
        since_timestamp: DateTime<Utc>,
    ) -> Result<(f64, u64, u64)> {
        let mut stmt = self.connection.prepare(
            "SELECT 
                AVG(cpu_percent)                  AS avg_cpu,
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

impl SystemMetricsStore for DuckDbSystemMetricsStore {
    fn insert_metrics_batch(&mut self, metrics: &[SystemMetrics], pid: u32) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }

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

    fn get_metrics_for_pid(&self, pid: u32, limit: Option<usize>) -> Result<Vec<SystemMetrics>> {
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

    fn get_metrics_in_time_range(
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
}

pub struct SqliteSystemMetricsStore {
    connection: rsql::Connection,
}

impl SqliteSystemMetricsStore {
    pub fn new_file(db_path: &PathBuf) -> Result<Self> {
        let conn = rsql::Connection::open(db_path)?;
        conn.execute_batch(
            r#"CREATE TABLE IF NOT EXISTS system_metrics (
                timestamp TEXT NOT NULL,
                pid INTEGER NOT NULL,
                cpu_percent REAL NOT NULL,
                memory_bytes INTEGER NOT NULL,
                disk_io_bytes INTEGER NOT NULL,
                PRIMARY KEY (timestamp, pid)
            );
            CREATE INDEX IF NOT EXISTS idx_system_metrics_timestamp ON system_metrics(timestamp);
            CREATE INDEX IF NOT EXISTS idx_system_metrics_pid ON system_metrics(pid);
            "#,
        )?;
        Ok(Self { connection: conn })
    }
}

impl SystemMetricsStore for SqliteSystemMetricsStore {
    fn insert_metrics_batch(&mut self, metrics: &[SystemMetrics], pid: u32) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let tx = self.connection.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO system_metrics (timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes) VALUES (?, ?, ?, ?, ?)",
            )?;
            for m in metrics {
                stmt.execute(rsql::params![
                    m.timestamp.to_rfc3339(),
                    pid as i64,
                    m.cpu_percent,
                    m.memory_bytes as i64,
                    m.disk_io_bytes as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn get_metrics_for_pid(&self, pid: u32, limit: Option<usize>) -> Result<Vec<SystemMetrics>> {
        let mut sql = String::from(
            "SELECT timestamp, cpu_percent, memory_bytes, disk_io_bytes FROM system_metrics WHERE pid = ? ORDER BY timestamp DESC",
        );
        if let Some(n) = limit {
            sql.push_str(&format!(" LIMIT {}", n));
        }
        let mut stmt = self.connection.prepare(&sql)?;
        let rows = stmt.query_map([pid as i64], |row| {
            let ts_s: String = row.get(0)?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts_s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rsql::Error::FromSqlConversionFailure(0, rsql::types::Type::Text, Box::new(e))
                })?;
            Ok(SystemMetrics {
                timestamp: ts,
                cpu_percent: row.get::<_, f64>(1)?,
                memory_bytes: row.get::<_, i64>(2)? as u64,
                disk_io_bytes: row.get::<_, i64>(3)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    fn get_metrics_in_time_range(
        &self,
        pid: u32,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Result<Vec<SystemMetrics>> {
        let mut stmt = self.connection.prepare(
            "SELECT timestamp, cpu_percent, memory_bytes, disk_io_bytes FROM system_metrics WHERE pid = ? AND timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC",
        )?;
        let rows = stmt.query_map(
            rsql::params![pid as i64, start_time.to_rfc3339(), end_time.to_rfc3339()],
            |row| {
                let ts_s: String = row.get(0)?;
                let ts = chrono::DateTime::parse_from_rfc3339(&ts_s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        rsql::Error::FromSqlConversionFailure(
                            0,
                            rsql::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                Ok(SystemMetrics {
                    timestamp: ts,
                    cpu_percent: row.get::<_, f64>(1)?,
                    memory_bytes: row.get::<_, i64>(2)? as u64,
                    disk_io_bytes: row.get::<_, i64>(3)? as u64,
                })
            },
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};
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

        Ok(())
    }

    #[test]
    fn test_sqlite_store_basic_flow() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("metrics.sqlite3");
        let mut store = SqliteSystemMetricsStore::new_file(&db_path)?;
        let test_pid = 42u32;
        let start_time = Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap();
        let metrics = create_test_metrics(4, start_time);

        store.insert_metrics_batch(&metrics, test_pid)?;

        let fetched = store.get_metrics_for_pid(test_pid, None)?;
        assert_eq!(fetched.len(), 4);
        assert_eq!(
            fetched[0].timestamp,
            start_time + ChronoDuration::seconds(3)
        );

        let range = store.get_metrics_in_time_range(
            test_pid,
            start_time + ChronoDuration::seconds(1),
            start_time + ChronoDuration::seconds(2),
        )?;
        assert_eq!(range.len(), 2);

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

#![allow(dead_code)]
use std::path::{Path, PathBuf};

const PID_FILE_NAME: &str = "kronid.pid";
const GRPC_UDS_NAME: &str = "kroni.sock";
const HTTP_UDS_NAME: &str = "kroni.http.sock";
const DATA_DB_NAME_DUCKDB: &str = "data.duckdb";
const DATA_DB_NAME_SQLITE: &str = "data.sqlite3";
const TRACKER_DB_NAME_DUCKDB: &str = "system-tracker.duckdb";
const TRACKER_DB_NAME_SQLITE: &str = "system-tracker.sqlite3";

pub fn pid_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PID_FILE_NAME)
}

// TODO: UDS on macOS is actually not faster than TCP loopback if the message size is large
pub fn grpc_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(GRPC_UDS_NAME)
}

pub fn http_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(HTTP_UDS_NAME)
}

pub fn tracker_db_with_backend(
    workspace_dir: &Path,
    backend: &crate::config::DatabaseBackendConfig,
) -> PathBuf {
    match backend {
        crate::config::DatabaseBackendConfig::Duckdb => workspace_dir.join(TRACKER_DB_NAME_DUCKDB),
        crate::config::DatabaseBackendConfig::Sqlite3 => workspace_dir.join(TRACKER_DB_NAME_SQLITE),
    }
}

// Backward-compat helper: default to DuckDB if not specified
pub fn tracker_db(workspace_dir: &Path) -> PathBuf {
    tracker_db_with_backend(workspace_dir, &crate::config::DatabaseBackendConfig::Duckdb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn pid_and_socket_paths_append_expected_names() {
        let base = Path::new("/tmp/workspace");
        assert_eq!(pid_file(base), base.join("kronid.pid"));
        assert_eq!(grpc_uds(base), base.join("kroni.sock"));
        assert_eq!(http_uds(base), base.join("kroni.http.sock"));
    }

    #[test]
    fn tracker_db_resolves_per_backend() {
        let base = Path::new("/opt/kronical");
        use crate::config::DatabaseBackendConfig::{Duckdb, Sqlite3};
        assert_eq!(
            tracker_db_with_backend(base, &Duckdb),
            base.join("system-tracker.duckdb")
        );
        assert_eq!(
            tracker_db_with_backend(base, &Sqlite3),
            base.join("system-tracker.sqlite3")
        );
    }

    #[test]
    fn tracker_db_defaults_to_duckdb_variant() {
        let base = Path::new("/tmp/kronical");
        assert_eq!(tracker_db(base), base.join("system-tracker.duckdb"));
    }
}

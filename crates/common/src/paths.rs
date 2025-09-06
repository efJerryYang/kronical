use std::path::{Path, PathBuf};

const PID_FILE_NAME: &str = "kronid.pid";
const GRPC_UDS_NAME: &str = "kroni.sock";
const HTTP_UDS_NAME: &str = "kroni.http.sock";
const TRACKER_DB_NAME_DUCKDB: &str = "system-tracker.duckdb";
const TRACKER_DB_NAME_SQLITE: &str = "system-tracker.sqlite3";

pub fn pid_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PID_FILE_NAME)
}

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

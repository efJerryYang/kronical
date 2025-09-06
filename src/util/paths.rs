use std::path::{Path, PathBuf};

// Well-known filenames used within the workspace directory
const PID_FILE_NAME: &str = "kronid.pid";
const GRPC_UDS_NAME: &str = "kroni.sock";
const HTTP_UDS_NAME: &str = "kroni.http.sock";
const TRACKER_DB_NAME: &str = "system-tracker.duckdb";

/// Path to the daemon PID file inside the workspace.
pub fn pid_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PID_FILE_NAME)
}

/// Path to the gRPC UDS socket inside the workspace.
pub fn grpc_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(GRPC_UDS_NAME)
}

/// Path to the HTTP/SSE UDS socket inside the workspace.
pub fn http_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(HTTP_UDS_NAME)
}

/// Path to the system tracker DuckDB database.
pub fn tracker_db(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(TRACKER_DB_NAME)
}

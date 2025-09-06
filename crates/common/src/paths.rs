use std::path::{Path, PathBuf};

const PID_FILE_NAME: &str = "kronid.pid";
const GRPC_UDS_NAME: &str = "kroni.sock";
const HTTP_UDS_NAME: &str = "kroni.http.sock";
const TRACKER_DB_NAME: &str = "system-tracker.duckdb";

pub fn pid_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PID_FILE_NAME)
}

pub fn grpc_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(GRPC_UDS_NAME)
}

pub fn http_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(HTTP_UDS_NAME)
}

pub fn tracker_db(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(TRACKER_DB_NAME)
}

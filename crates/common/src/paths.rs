#![allow(dead_code)]
use std::path::{Path, PathBuf};

const PID_FILE_NAME: &str = "kronid.pid";
const RUN_ID_FILE_NAME: &str = "kronid.run_id";
const GRPC_UDS_NAME: &str = "kronid.sock";
const HTTP_UDS_NAME: &str = "kronid.http.sock";
const DATA_DB_NAME_DUCKDB: &str = "data.duckdb";
const DATA_DB_NAME_SQLITE: &str = "data.sqlite3";

pub fn pid_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(PID_FILE_NAME)
}

pub fn run_id_file(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(RUN_ID_FILE_NAME)
}

// TODO: UDS on macOS is actually not faster than TCP loopback if the message size is large
pub fn grpc_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(GRPC_UDS_NAME)
}

pub fn http_uds(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(HTTP_UDS_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn pid_and_socket_paths_append_expected_names() {
        let base = Path::new("/tmp/workspace");
        assert_eq!(pid_file(base), base.join("kronid.pid"));
        assert_eq!(run_id_file(base), base.join("kronid.run_id"));
        assert_eq!(grpc_uds(base), base.join("kronid.sock"));
        assert_eq!(http_uds(base), base.join("kronid.http.sock"));
    }
}

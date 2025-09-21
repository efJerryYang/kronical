use anyhow::Result;
use log::info;
use std::path::PathBuf;
use std::sync::Arc;

/// Join handles for running API transports.
pub struct ApiHandles {
    pub grpc: Option<std::thread::JoinHandle<()>>,
    pub http: Option<std::thread::JoinHandle<()>>,
}

/// Spawn both gRPC and HTTP/SSE API servers over Unix Domain Sockets.
/// Returns join handles for both servers when successful.
pub fn spawn_all(
    grpc_uds: PathBuf,
    http_uds: PathBuf,
    snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
) -> Result<ApiHandles> {
    let grpc = crate::daemon::server::grpc::spawn_server(grpc_uds, Arc::clone(&snapshot_bus))?;
    info!("Kroni gRPC API server started");
    let http = crate::daemon::server::http::spawn_http_server(http_uds, Arc::clone(&snapshot_bus))?;
    info!("HTTP admin/SSE server started");
    Ok(ApiHandles {
        grpc: Some(grpc),
        http: Some(http),
    })
}

/// Re-export helper to set the system tracker database path for the gRPC API.
pub fn set_system_tracker_db_path(db_path: PathBuf) {
    crate::daemon::server::grpc::set_system_tracker_db_path(db_path)
}

/// Re-export helper to set the system tracker query channel for the gRPC API.
pub fn set_system_tracker_query_tx(
    tx: std::sync::mpsc::Sender<crate::daemon::tracker::MetricsQueryReq>,
) {
    crate::daemon::server::grpc::set_system_tracker_query_tx(tx)
}

/// Re-export helper to set the system tracker control channel for the gRPC API.
pub fn set_system_tracker_control_tx(
    tx: std::sync::mpsc::Sender<crate::daemon::tracker::ControlMsg>,
) {
    crate::daemon::server::grpc::set_system_tracker_control_tx(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::server::grpc::trigger_grpc_shutdown;
    use crate::daemon::server::http::{reset_http_accept_budget, set_http_accept_budget};
    use crate::daemon::snapshot::SnapshotBus;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    #[ignore = "requires UDS permissions"]
    fn spawn_all_returns_joinable_handles() {
        fn test_socket_path(prefix: &str) -> PathBuf {
            let mut base = std::env::current_dir().expect("cwd");
            base.push("target/test-sockets");
            std::fs::create_dir_all(&base).expect("mkdir test sockets");
            let unique = format!(
                "{}-{}-{}.sock",
                prefix,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            base.join(unique)
        }

        let grpc_path = test_socket_path("grpc");
        let http_path = test_socket_path("http");
        let bus = Arc::new(SnapshotBus::new());

        set_http_accept_budget(0);
        let handles = match spawn_all(grpc_path.clone(), http_path.clone(), Arc::clone(&bus)) {
            Ok(handles) => handles,
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Operation not permitted")
                        || msg.contains("failed to signal readiness"),
                    "unexpected spawn error: {}",
                    msg
                );
                reset_http_accept_budget();
                let _ = std::fs::remove_file(&grpc_path);
                let _ = std::fs::remove_file(&http_path);
                return;
            }
        };

        trigger_grpc_shutdown();

        let grpc_handle = handles.grpc.expect("grpc handle present");
        grpc_handle.join().expect("join grpc");

        let http_handle = handles.http.expect("http handle present");
        http_handle.join().expect("join http");

        let _ = std::fs::remove_file(&grpc_path);
        let _ = std::fs::remove_file(&http_path);

        reset_http_accept_budget();
    }
}

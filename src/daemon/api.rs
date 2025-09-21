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
    let grpc = crate::daemon::server::grpc::spawn_server(
        grpc_uds,
        Arc::clone(&snapshot_bus),
    )?;
    info!("Kroni gRPC API server started");
    let http = crate::daemon::server::http::spawn_http_server(
        http_uds,
        Arc::clone(&snapshot_bus),
    )?;
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

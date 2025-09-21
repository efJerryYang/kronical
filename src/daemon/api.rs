use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use anyhow::Result;
use log::info;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) trait ApiServerSpawner {
    fn spawn_grpc(
        &self,
        grpc_uds: PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<ThreadHandle>;

    fn spawn_http(
        &self,
        http_uds: PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<ThreadHandle>;
}

struct DefaultApiServerSpawner;

impl ApiServerSpawner for DefaultApiServerSpawner {
    fn spawn_grpc(
        &self,
        grpc_uds: PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<ThreadHandle> {
        crate::daemon::server::grpc::spawn_server(grpc_uds, snapshot_bus, threads)
    }

    fn spawn_http(
        &self,
        http_uds: PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<ThreadHandle> {
        crate::daemon::server::http::spawn_http_server(http_uds, snapshot_bus, threads)
    }
}

/// Join handles for running API transports.
pub struct ApiHandles {
    pub grpc: Option<ThreadHandle>,
    pub http: Option<ThreadHandle>,
}

impl ApiHandles {
    pub fn join_all(self) {
        if let Some(handle) = self.grpc {
            if let Err(e) = handle.join() {
                log::error!("gRPC server thread panicked: {:?}", e);
            }
        }
        if let Some(handle) = self.http {
            if let Err(e) = handle.join() {
                log::error!("HTTP server thread panicked: {:?}", e);
            }
        }
    }
}

/// Spawn both gRPC and HTTP/SSE API servers over Unix Domain Sockets.
/// Returns join handles for both servers when successful.
pub fn spawn_all(
    grpc_uds: PathBuf,
    http_uds: PathBuf,
    snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
    threads: ThreadRegistry,
) -> Result<ApiHandles> {
    spawn_all_with(
        &DefaultApiServerSpawner,
        grpc_uds,
        http_uds,
        snapshot_bus,
        threads,
    )
}

fn spawn_all_with<S: ApiServerSpawner + ?Sized>(
    spawner: &S,
    grpc_uds: PathBuf,
    http_uds: PathBuf,
    snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
    threads: ThreadRegistry,
) -> Result<ApiHandles> {
    let grpc = spawner.spawn_grpc(grpc_uds, Arc::clone(&snapshot_bus), threads.clone())?;
    info!("Kroni gRPC API server started");
    let http = spawner.spawn_http(http_uds, Arc::clone(&snapshot_bus), threads)?;
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
    use crate::daemon::runtime::ThreadRegistry;
    use crate::daemon::snapshot::SnapshotBus;
    use mockall::mock;
    use std::path::PathBuf;
    use std::sync::Arc;

    mock! {
        pub Spawner {}
        impl ApiServerSpawner for Spawner {
            fn spawn_grpc(
                &self,
                grpc_uds: PathBuf,
                snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
                threads: ThreadRegistry,
            ) -> Result<ThreadHandle>;

            fn spawn_http(
                &self,
                http_uds: PathBuf,
                snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
                threads: ThreadRegistry,
            ) -> Result<ThreadHandle>;
        }
    }

    #[test]
    fn spawn_all_with_uses_spawner() {
        let grpc_path = PathBuf::from("/tmp/grpc.sock");
        let http_path = PathBuf::from("/tmp/http.sock");
        let bus = Arc::new(SnapshotBus::new());
        let threads = ThreadRegistry::new();

        let mut spawner = MockSpawner::new();
        let grpc_path_clone = grpc_path.clone();
        spawner
            .expect_spawn_grpc()
            .withf(move |p, _, _| p == &grpc_path_clone)
            .times(1)
            .returning(|_, _, registry| registry.spawn("mock-grpc", || {}).map_err(Into::into));

        let http_path_clone = http_path.clone();
        spawner
            .expect_spawn_http()
            .withf(move |p, _, _| p == &http_path_clone)
            .times(1)
            .returning(|_, _, registry| registry.spawn("mock-http", || {}).map_err(Into::into));

        let handles = spawn_all_with(&spawner, grpc_path, http_path, Arc::clone(&bus), threads)
            .expect("spawn succeeds");

        handles.join_all();
    }

    #[test]
    fn spawn_all_with_grpc_failure_bubbles_error() {
        let grpc_path = PathBuf::from("/tmp/grpc.sock");
        let http_path = PathBuf::from("/tmp/http.sock");
        let bus = Arc::new(SnapshotBus::new());
        let threads = ThreadRegistry::new();

        let mut spawner = MockSpawner::new();
        spawner
            .expect_spawn_grpc()
            .times(1)
            .returning(|_, _, _| Err(anyhow::anyhow!("boom")));
        spawner.expect_spawn_http().times(0);

        let err = match spawn_all_with(&spawner, grpc_path, http_path, bus, threads) {
            Ok(_) => panic!("expected error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn spawn_all_with_http_failure_bubbles_error() {
        let grpc_path = PathBuf::from("/tmp/grpc.sock");
        let http_path = PathBuf::from("/tmp/http.sock");
        let bus = Arc::new(SnapshotBus::new());
        let threads = ThreadRegistry::new();

        let mut spawner = MockSpawner::new();
        spawner
            .expect_spawn_grpc()
            .times(1)
            .returning(|_, _, registry| registry.spawn("mock-grpc", || {}).map_err(Into::into));
        spawner
            .expect_spawn_http()
            .times(1)
            .returning(|_, _, _| Err(anyhow::anyhow!("http failed")));

        let err = match spawn_all_with(&spawner, grpc_path, http_path, bus, threads) {
            Ok(_) => panic!("expected error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("http failed"));
    }

    #[test]
    fn join_all_waits_for_active_handles() {
        let threads = ThreadRegistry::new();
        let grpc_handle = threads
            .spawn("grpc-thread", || {})
            .expect("spawn grpc thread");
        let http_handle = threads
            .spawn("http-thread", || {})
            .expect("spawn http thread");

        ApiHandles {
            grpc: Some(grpc_handle),
            http: Some(http_handle),
        }
        .join_all();
    }
}

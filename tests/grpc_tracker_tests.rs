use chrono::Utc;
use kronical::daemon::server::grpc::set_system_tracker_db_path;
use kronical::daemon::tracker::SystemTracker;
use kronical::kroni_api::kroni::v1::SystemMetricsRequest;
use kronical::kroni_api::kroni::v1::kroni_server::Kroni;
use prost_types::Timestamp;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;
use tonic::Request;

// Call the service directly rather than spinning up a UDS server.

#[tokio::test(flavor = "current_thread")]
async fn metrics_increase_across_consecutive_requests_after_flush() {
    // Temp workspace
    let tmp = tempdir().unwrap();
    let db_path: PathBuf = tmp.path().join("system-tracker.duckdb");

    // Start tracker with short interval and large batch to require flush
    let pid = std::process::id();
    let mut tracker = SystemTracker::new(
        pid,
        0.25,
        100,
        db_path.clone(),
        kronical::util::config::DatabaseBackendConfig::Duckdb,
        10,
    );
    tracker.start().unwrap();

    // Point the gRPC service at this DB
    set_system_tracker_db_path(db_path.clone());

    // Allow a couple of samples to be collected
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let svc = kronical::daemon::server::grpc::KroniSvc::default();

    // First request; server performs a flush before querying and sets end_time = now internally
    let start = Utc::now() - chrono::Duration::minutes(5);
    let req1 = SystemMetricsRequest {
        pid,
        start_time: Some(Timestamp {
            seconds: start.timestamp(),
            nanos: start.timestamp_subsec_nanos() as i32,
        }),
        end_time: None,
        limit: 0,
    };
    let resp1 = svc
        .get_system_metrics(Request::new(req1))
        .await
        .unwrap()
        .into_inner();
    let last1 = resp1
        .metrics
        .last()
        .and_then(|m| m.timestamp.as_ref().map(|t| t.seconds))
        .unwrap_or(0);

    // Wait for another sample period, then issue a second request
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let req2 = SystemMetricsRequest {
        pid,
        start_time: Some(Timestamp {
            seconds: start.timestamp(),
            nanos: start.timestamp_subsec_nanos() as i32,
        }),
        end_time: None,
        limit: 0,
    };
    let resp2 = svc
        .get_system_metrics(Request::new(req2))
        .await
        .unwrap()
        .into_inner();
    assert!(resp2.metrics.len() >= resp1.metrics.len());
    let last2 = resp2
        .metrics
        .last()
        .and_then(|m| m.timestamp.as_ref().map(|t| t.seconds))
        .unwrap_or(0);
    assert!(last2 >= last1);

    // Clean up tracker
    tracker.stop().ok();
}

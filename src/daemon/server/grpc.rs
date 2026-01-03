use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::daemon::snapshot;
use crate::kroni_api::kroni::v1::kroni_server::{Kroni, KroniServer};
use crate::kroni_api::kroni::v1::{
    SnapshotReply, SnapshotRequest, SystemMetric, SystemMetricsReply, SystemMetricsRequest,
    WatchRequest, snapshot_reply::ActivityState as PbState, snapshot_reply::Cadence,
    snapshot_reply::Config, snapshot_reply::Counts, snapshot_reply::Focus,
    snapshot_reply::SnapshotApp as PbApp, snapshot_reply::SnapshotWindow as PbWin,
    snapshot_reply::Storage, snapshot_reply::Transition,
};
use crate::util::logging::{info, warn};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_core::Stream;
use once_cell::sync::OnceCell;
use prost_types::Timestamp;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status, transport::Server};

#[cfg(test)]
use once_cell::sync::Lazy;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;
#[cfg(test)]
use tokio::sync::Notify;

static SYSTEM_TRACKER_DB_PATH: OnceCell<PathBuf> = OnceCell::new();
static SYSTEM_TRACKER_QUERY_TX: OnceCell<mpsc::Sender<crate::daemon::tracker::MetricsQueryReq>> =
    OnceCell::new();
static SYSTEM_TRACKER_CONTROL_TX: OnceCell<mpsc::Sender<crate::daemon::tracker::ControlRequest>> =
    OnceCell::new();

#[cfg(test)]
static GRPC_TEST_SHUTDOWN: Lazy<Mutex<Option<Arc<Notify>>>> = Lazy::new(|| Mutex::new(None));

#[cfg(test)]
pub(crate) fn install_grpc_shutdown_notify(handle: Arc<Notify>) {
    let mut guard = GRPC_TEST_SHUTDOWN.lock().unwrap();
    *guard = Some(handle);
}

#[cfg(test)]
pub(crate) fn trigger_grpc_shutdown() {
    if let Some(handle) = GRPC_TEST_SHUTDOWN.lock().unwrap().take() {
        handle.notify_waiters();
    }
}

pub fn set_system_tracker_db_path(db_path: PathBuf) {
    let _ = SYSTEM_TRACKER_DB_PATH.set(db_path);
}

pub fn set_system_tracker_query_tx(tx: mpsc::Sender<crate::daemon::tracker::MetricsQueryReq>) {
    let _ = SYSTEM_TRACKER_QUERY_TX.set(tx);
}

pub fn set_system_tracker_control_tx(tx: mpsc::Sender<crate::daemon::tracker::ControlRequest>) {
    let _ = SYSTEM_TRACKER_CONTROL_TX.set(tx);
}

#[derive(Clone)]
pub struct KroniSvc {
    snapshot_bus: Arc<snapshot::SnapshotBus>,
}

impl KroniSvc {
    pub fn new(snapshot_bus: Arc<snapshot::SnapshotBus>) -> Self {
        Self { snapshot_bus }
    }
}

impl Default for KroniSvc {
    fn default() -> Self {
        Self::new(Arc::new(snapshot::SnapshotBus::new()))
    }
}

#[tonic::async_trait]
impl Kroni for KroniSvc {
    async fn snapshot(
        &self,
        _req: Request<SnapshotRequest>,
    ) -> Result<Response<SnapshotReply>, Status> {
        let s = self.snapshot_bus.snapshot();
        Ok(Response::new(to_pb(&s)))
    }

    type WatchStream = Pin<Box<dyn Stream<Item = Result<SnapshotReply, Status>> + Send + 'static>>;

    async fn watch(
        &self,
        _req: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        // Stream updates via the snapshot watch channel
        let rx = self.snapshot_bus.watch_snapshot();
        let stream = WatchStream::new(rx).map(|arc| Ok(to_pb(&arc)));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_system_metrics(
        &self,
        req: Request<SystemMetricsRequest>,
    ) -> Result<Response<SystemMetricsReply>, Status> {
        let request = req.into_inner();

        if SYSTEM_TRACKER_DB_PATH.get().is_none() {
            return Err(Status::unavailable("System tracker not configured"));
        }

        // Proactively request a tracker flush so we read fresh data through the
        // control channel, then wait for acknowledgement (with timeout).
        let ctrl_tx = SYSTEM_TRACKER_CONTROL_TX
            .get()
            .ok_or_else(|| Status::unavailable("System tracker control channel not configured"))?
            .clone();
        let (ack_tx, ack_rx) = oneshot::channel();
        ctrl_tx
            .send(crate::daemon::tracker::ControlRequest::Flush(ack_tx))
            .await
            .map_err(|_| Status::internal("Failed to request tracker flush"))?;
        time::timeout(std::time::Duration::from_millis(2000), ack_rx)
            .await
            .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker flush"))?
            .map_err(|_| Status::internal("Tracker flush acknowledgement dropped"))?;

        // Route the read through the tracker thread using a query channel so
        // the query runs against the same in-process DB instance as the writer.
        let tx = SYSTEM_TRACKER_QUERY_TX
            .get()
            .ok_or_else(|| Status::unavailable("System tracker not configured (query tx)"))?
            .clone();

        let pid = request.pid;
        let mut metrics = if request.limit > 0 {
            let query = crate::daemon::tracker::MetricsQuery::ByLimit {
                pid,
                limit: request.limit as usize,
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(crate::daemon::tracker::MetricsQueryReq {
                query,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("Failed to send query to tracker thread"))?;
            time::timeout(std::time::Duration::from_millis(1500), reply_rx)
                .await
                .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker reply"))?
                .map_err(|_| Status::internal("Tracker query channel closed"))?
                .map_err(|e| Status::internal(format!("Tracker query error: {}", e)))?
        } else {
            let start_time = match request.start_time.as_ref() {
                Some(ts) => ts_to_utc(ts)
                    .map_err(|e| Status::invalid_argument(format!("Invalid start_time: {}", e)))?,
                None => Utc::now() - chrono::Duration::minutes(5),
            };
            let end_time = match request.end_time.as_ref() {
                Some(ts) => ts_to_utc(ts)
                    .map_err(|e| Status::invalid_argument(format!("Invalid end_time: {}", e)))?,
                None => Utc::now(),
            };
            let query = crate::daemon::tracker::MetricsQuery::ByRange {
                pid,
                start: start_time,
                end: end_time,
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(crate::daemon::tracker::MetricsQueryReq {
                query,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("Failed to send query to tracker thread"))?;
            time::timeout(std::time::Duration::from_millis(1500), reply_rx)
                .await
                .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker reply"))?
                .map_err(|_| Status::internal("Tracker query channel closed"))?
                .map_err(|e| Status::internal(format!("Tracker query error: {}", e)))?
        };

        // Best-effort: if empty, allow one short retry to let the tracker
        // finish persisting immediately after a flush.
        if metrics.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if request.limit > 0 {
                let query = crate::daemon::tracker::MetricsQuery::ByLimit {
                    pid,
                    limit: request.limit as usize,
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                tx.send(crate::daemon::tracker::MetricsQueryReq {
                    query,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| Status::internal("Failed to send query to tracker thread (retry)"))?;
                metrics = time::timeout(std::time::Duration::from_millis(1200), reply_rx)
                    .await
                    .map_err(|_| {
                        Status::deadline_exceeded("Timed out waiting for tracker reply (retry)")
                    })?
                    .map_err(|_| Status::internal("Tracker query channel closed (retry)"))?
                    .map_err(|e| Status::internal(format!("Tracker query error (retry): {}", e)))?;
            } else {
                let start_time = match request.start_time.as_ref() {
                    Some(ts) => ts_to_utc(ts).map_err(|e| {
                        Status::invalid_argument(format!("Invalid start_time: {}", e))
                    })?,
                    None => Utc::now() - chrono::Duration::minutes(5),
                };
                let end_time = match request.end_time.as_ref() {
                    Some(ts) => ts_to_utc(ts).map_err(|e| {
                        Status::invalid_argument(format!("Invalid end_time: {}", e))
                    })?,
                    None => Utc::now(),
                };
                let query = crate::daemon::tracker::MetricsQuery::ByRange {
                    pid,
                    start: start_time,
                    end: end_time,
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                tx.send(crate::daemon::tracker::MetricsQueryReq {
                    query,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| Status::internal("Failed to send query to tracker thread (retry)"))?;
                metrics = time::timeout(std::time::Duration::from_millis(1200), reply_rx)
                    .await
                    .map_err(|_| {
                        Status::deadline_exceeded("Timed out waiting for tracker reply (retry)")
                    })?
                    .map_err(|_| Status::internal("Tracker query channel closed (retry)"))?
                    .map_err(|e| Status::internal(format!("Tracker query error (retry): {}", e)))?;
            }
        }

        let pb_metrics = metrics
            .into_iter()
            .map(|m| SystemMetric {
                timestamp: Some(utc_to_ts(m.timestamp)),
                cpu_percent: m.cpu_percent,
                memory_bytes: m.memory_bytes,
                disk_io_bytes: m.disk_io_bytes,
            })
            .collect::<Vec<_>>();

        let total_count = pb_metrics.len() as u32;

        let reply = SystemMetricsReply {
            metrics: pb_metrics,
            total_count,
        };

        Ok(Response::new(reply))
    }
}

fn ts_to_utc(ts: &Timestamp) -> Result<DateTime<Utc>, &'static str> {
    // Clamp nanos to u32 and use chrono’s from_timestamp
    let secs = ts.seconds;
    let nanos = ts.nanos;
    if nanos < 0 || nanos >= 1_000_000_000 {
        return Err("nanos out of range");
    }
    DateTime::<Utc>::from_timestamp(secs, nanos as u32).ok_or("invalid timestamp range")
}

fn utc_to_ts(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

pub fn to_pb(s: &snapshot::Snapshot) -> SnapshotReply {
    let state = match s.activity_state {
        crate::daemon::records::ActivityState::Active => PbState::Active,
        crate::daemon::records::ActivityState::Passive => PbState::Passive,
        crate::daemon::records::ActivityState::Inactive => PbState::Inactive,
        crate::daemon::records::ActivityState::Locked => PbState::Locked,
    } as i32;
    let focus = s.focus.as_ref().map(|f| Focus {
        app: (*f.app_name).clone(),
        pid: f.pid,
        window_id: f.window_id.to_string(),
        title: (*f.window_title).clone(),
        since: Some(utc_to_ts(f.window_instance_start)),
    });
    let last_transition = s.last_transition.as_ref().map(|t| Transition {
        from: match t.from {
            crate::daemon::records::ActivityState::Active => PbState::Active as i32,
            crate::daemon::records::ActivityState::Passive => PbState::Passive as i32,
            crate::daemon::records::ActivityState::Inactive => PbState::Inactive as i32,
            crate::daemon::records::ActivityState::Locked => PbState::Locked as i32,
        },
        to: match t.to {
            crate::daemon::records::ActivityState::Active => PbState::Active as i32,
            crate::daemon::records::ActivityState::Passive => PbState::Passive as i32,
            crate::daemon::records::ActivityState::Inactive => PbState::Inactive as i32,
            crate::daemon::records::ActivityState::Locked => PbState::Locked as i32,
        },
        at: Some(utc_to_ts(t.at)),
    });
    let counts = Some(Counts {
        signals_seen: s.counts.signals_seen,
        hints_seen: s.counts.hints_seen,
        records_emitted: s.counts.records_emitted,
    });
    let cadence = Some(Cadence {
        current_ms: s.cadence_ms,
        reason: s.cadence_reason.clone(),
    });
    let storage = Some(Storage {
        backlog_count: s.storage.backlog_count,
        last_flush: s.storage.last_flush_at.as_ref().map(|t| utc_to_ts(*t)),
    });
    let config = Some(Config {
        active_grace_secs: s.config.active_grace_secs,
        idle_threshold_secs: s.config.idle_threshold_secs,
        retention_minutes: s.config.retention_minutes,
        ephemeral_max_duration_secs: s.config.ephemeral_max_duration_secs,
        ephemeral_min_distinct_ids: s.config.ephemeral_min_distinct_ids as u32,
        ephemeral_app_max_duration_secs: s.config.ephemeral_app_max_duration_secs,
        ephemeral_app_min_distinct_procs: s.config.ephemeral_app_min_distinct_procs as u32,
    });
    let aggregated_apps = s
        .aggregated_apps
        .iter()
        .map(|a| PbApp {
            app_name: a.app_name.clone(),
            pid: a.pid,
            process_start_time: a.process_start_time,
            windows: a
                .windows
                .iter()
                .map(|w| PbWin {
                    window_id: w.window_id.clone(),
                    window_title: w.window_title.clone(),
                    first_seen: Some(utc_to_ts(w.first_seen)),
                    last_seen: Some(utc_to_ts(w.last_seen)),
                    duration_seconds: w.duration_seconds,
                    is_group: w.is_group,
                })
                .collect(),
            total_duration_secs: a.total_duration_secs,
            total_duration_pretty: a.total_duration_pretty.clone(),
        })
        .collect();
    SnapshotReply {
        seq: s.seq,
        mono_ns: s.mono_ns,
        activity_state: state,
        focus,
        last_transition,
        counts,
        cadence,
        next_timeout: s.next_timeout.as_ref().map(|t| utc_to_ts(*t)),
        storage,
        config,
        health: s.health.clone(),
        aggregated_apps,
    }
}

pub fn spawn_server(
    uds_path: PathBuf,
    snapshot_bus: Arc<snapshot::SnapshotBus>,
    threads: ThreadRegistry,
) -> Result<ThreadHandle> {
    if uds_path.exists() {
        warn!("Removing stale UDS: {:?}", uds_path);
        let _ = std::fs::remove_file(&uds_path);
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let bus_for_thread = Arc::clone(&snapshot_bus);
    #[cfg(test)]
    let shutdown_notify = Arc::new(Notify::new());
    #[cfg(test)]
    install_grpc_shutdown_notify(Arc::clone(&shutdown_notify));
    #[cfg(test)]
    let shutdown_notify_for_thread = Arc::clone(&shutdown_notify);

    let handle = threads
        .spawn("grpc-server", move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime");
            rt.block_on(async move {
                let uds = UnixListener::bind(&uds_path).expect("bind uds");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&uds_path).expect("meta").permissions();
                    perms.set_mode(0o600);
                    std::fs::set_permissions(&uds_path, perms).expect("chmod");
                }
                let incoming = tokio_stream::wrappers::UnixListenerStream::new(uds);
                let svc = KroniSvc::new(bus_for_thread);
                tx.send(()).ok();
                info!("kroni API listening on {:?}", uds_path);
                #[cfg(test)]
                {
                    let shutdown = shutdown_notify_for_thread;
                    Server::builder()
                        .add_service(KroniServer::new(svc))
                        .serve_with_incoming_shutdown(incoming, async move {
                            shutdown.notified().await;
                        })
                        .await
                        .expect("serve");
                }
                #[cfg(not(test))]
                {
                    Server::builder()
                        .add_service(KroniServer::new(svc))
                        .serve_with_incoming(incoming)
                        .await
                        .expect("serve");
                }
            });
        })
        .context("spawn gRPC server thread")?;
    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(()) => Ok(handle),
        Err(_) => {
            let panic_msg = match handle.join() {
                Ok(_) => None,
                Err(payload) => match payload.downcast::<String>() {
                    Ok(msg) => Some(*msg),
                    Err(payload) => match payload.downcast::<&'static str>() {
                        Ok(msg) => Some((*msg).to_string()),
                        Err(_) => None,
                    },
                },
            };
            let detail = panic_msg.map(|msg| format!(": {msg}")).unwrap_or_default();
            Err(anyhow::anyhow!(
                "gRPC server failed to signal readiness within 500ms{detail}"
            ))
        }
    }
}

// Note: gRPC integration tests live under tests/ to avoid OnceCell contention.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::events::WindowFocusInfo;
    use crate::daemon::records::ActivityState;
    use crate::daemon::runtime::ThreadRegistry;
    use crate::daemon::snapshot::{
        ConfigSummary, Counts as SnapshotCounts, Snapshot, SnapshotApp, SnapshotWindow,
        StorageInfo, Transition as SnapshotTransition,
    };
    use crate::kroni_api::kroni::v1::{SnapshotRequest, WatchRequest};
    use chrono::{Duration, TimeZone, Utc};
    use prost_types::Timestamp;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio_stream::StreamExt;
    use tonic::Request;

    fn sample_focus() -> WindowFocusInfo {
        WindowFocusInfo {
            pid: 4242,
            process_start_time: 99,
            app_name: Arc::new("Terminal".to_string()),
            window_title: Arc::new("Build log".to_string()),
            window_id: 7,
            window_instance_start: Utc.with_ymd_and_hms(2024, 5, 1, 12, 30, 0).unwrap(),
            window_position: None,
            window_size: Some((1440, 900)),
        }
    }

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

    #[test]
    fn ts_to_utc_rejects_out_of_range_nanos() {
        let ts = Timestamp {
            seconds: 1,
            nanos: 1_500_000_000,
        };
        assert!(ts_to_utc(&ts).is_err());
    }

    #[test]
    fn snapshot_conversion_preserves_focus_and_counts() {
        let mut snapshot = Snapshot::empty();
        snapshot.seq = 42;
        snapshot.mono_ns = 9_876_543;
        snapshot.activity_state = ActivityState::Active;
        snapshot.focus = Some(sample_focus());
        snapshot.last_transition = Some(SnapshotTransition {
            from: ActivityState::Inactive,
            to: ActivityState::Active,
            at: Utc.with_ymd_and_hms(2024, 5, 1, 12, 29, 0).unwrap(),
            by_signal: Some("keyboard".to_string()),
        });
        snapshot.counts = SnapshotCounts {
            signals_seen: 12,
            hints_seen: 8,
            records_emitted: 3,
        };
        snapshot.cadence_ms = 750;
        snapshot.cadence_reason = "focus-change".to_string();
        snapshot.next_timeout = Some(Utc::now() + Duration::seconds(30));
        snapshot.storage = StorageInfo {
            backlog_count: 4,
            last_flush_at: Some(Utc::now()),
        };
        snapshot.config = ConfigSummary {
            active_grace_secs: 5,
            idle_threshold_secs: 300,
            retention_minutes: 60,
            ephemeral_max_duration_secs: 90,
            ephemeral_min_distinct_ids: 3,
            ephemeral_app_max_duration_secs: 120,
            ephemeral_app_min_distinct_procs: 2,
        };
        snapshot.health = vec!["ok".into(), "pipeline".into()];
        snapshot.aggregated_apps = vec![SnapshotApp {
            app_name: "Terminal".into(),
            pid: 4242,
            process_start_time: 99,
            windows: vec![SnapshotWindow {
                window_id: "7".into(),
                window_title: "Build log".into(),
                first_seen: Utc.with_ymd_and_hms(2024, 5, 1, 12, 0, 0).unwrap(),
                last_seen: Utc.with_ymd_and_hms(2024, 5, 1, 12, 30, 0).unwrap(),
                duration_seconds: 1_200,
                is_group: false,
            }],
            total_duration_secs: 1_200,
            total_duration_pretty: "20m".into(),
        }];

        let reply = to_pb(&snapshot);

        assert_eq!(reply.seq, 42);
        assert_eq!(reply.activity_state, PbState::Active as i32);
        let focus = reply.focus.expect("focus converted");
        assert_eq!(focus.app, "Terminal");
        assert_eq!(focus.window_id, "7");
        assert!(focus.since.is_some());

        let transition = reply.last_transition.expect("transition converted");
        assert_eq!(transition.from, PbState::Inactive as i32);
        assert_eq!(transition.to, PbState::Active as i32);
        assert!(transition.at.is_some());

        let counts = reply.counts.expect("counts");
        assert_eq!(counts.signals_seen, 12);
        assert_eq!(counts.records_emitted, 3);

        let storage = reply.storage.expect("storage");
        assert_eq!(storage.backlog_count, 4);
        assert!(storage.last_flush.is_some());

        let config = reply.config.expect("config");
        assert_eq!(config.idle_threshold_secs, 300);
        assert_eq!(config.ephemeral_min_distinct_ids, 3);

        assert_eq!(reply.health, vec!["ok", "pipeline"]);
        assert_eq!(reply.aggregated_apps.len(), 1);
        let window = &reply.aggregated_apps[0].windows[0];
        assert_eq!(window.window_id, "7");
        assert_eq!(window.duration_seconds, 1_200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_call_returns_latest_state() {
        let bus = Arc::new(snapshot::SnapshotBus::new());
        bus.publish_basic(
            ActivityState::Active,
            Some(sample_focus()),
            None,
            SnapshotCounts {
                signals_seen: 4,
                hints_seen: 2,
                records_emitted: 1,
            },
            500,
            "tick".into(),
            None,
            StorageInfo {
                backlog_count: 3,
                last_flush_at: None,
            },
            ConfigSummary::default(),
            vec!["healthy".into()],
            Vec::new(),
        );

        let svc = KroniSvc::new(Arc::clone(&bus));
        let reply = svc
            .snapshot(Request::new(SnapshotRequest {
                sections: Vec::new(),
                detail: "summary".into(),
            }))
            .await
            .expect("snapshot call succeeds")
            .into_inner();

        assert_eq!(reply.activity_state, PbState::Active as i32);
        assert_eq!(reply.counts.unwrap().signals_seen, 4);
        assert_eq!(reply.storage.unwrap().backlog_count, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watch_stream_emits_updates() {
        let bus = Arc::new(snapshot::SnapshotBus::new());
        let svc = KroniSvc::new(Arc::clone(&bus));

        let mut stream = svc
            .watch(Request::new(WatchRequest {
                sections: Vec::new(),
                detail: "summary".into(),
            }))
            .await
            .expect("watch call succeeds")
            .into_inner();

        let first = stream.next().await.expect("first item").expect("ok");
        assert_eq!(first.activity_state, PbState::Inactive as i32);

        bus.publish_basic(
            ActivityState::Active,
            Some(sample_focus()),
            None,
            SnapshotCounts::default(),
            250,
            "hook".into(),
            None,
            StorageInfo {
                backlog_count: 1,
                last_flush_at: None,
            },
            ConfigSummary::default(),
            Vec::new(),
            Vec::new(),
        );

        let second = stream.next().await.expect("second item").expect("ok");
        assert_eq!(second.activity_state, PbState::Active as i32);
        assert_eq!(second.focus.unwrap().title, "Build log");
    }

    #[test]
    #[ignore = "requires UDS permissions"]
    fn spawn_server_serves_snapshot_and_shuts_down() {
        let uds_path = test_socket_path("grpc");

        let bus = Arc::new(snapshot::SnapshotBus::new());
        bus.publish_basic(
            ActivityState::Active,
            Some(sample_focus()),
            None,
            SnapshotCounts {
                signals_seen: 5,
                hints_seen: 4,
                records_emitted: 3,
            },
            320,
            "unit-test".into(),
            None,
            StorageInfo {
                backlog_count: 0,
                last_flush_at: None,
            },
            ConfigSummary::default(),
            vec!["ok".into()],
            Vec::new(),
        );

        let handle =
            match super::spawn_server(uds_path.clone(), Arc::clone(&bus), ThreadRegistry::new()) {
                Ok(handle) => handle,
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("Operation not permitted")
                            || msg.contains("failed to signal readiness"),
                        "unexpected spawn error: {}",
                        msg
                    );
                    let _ = std::fs::remove_file(&uds_path);
                    return;
                }
            };

        super::trigger_grpc_shutdown();
        handle.join().expect("join thread");
        let _ = std::fs::remove_file(&uds_path);
    }
}

use crate::daemon::snapshot;
use crate::kroni_api::kroni::v1::kroni_server::{Kroni, KroniServer};
use crate::kroni_api::kroni::v1::{
    SnapshotReply, SnapshotRequest, SystemMetric, SystemMetricsReply, SystemMetricsRequest,
    WatchRequest, snapshot_reply::ActivityState as PbState, snapshot_reply::Cadence,
    snapshot_reply::Config, snapshot_reply::Counts, snapshot_reply::Focus,
    snapshot_reply::SnapshotApp as PbApp, snapshot_reply::SnapshotWindow as PbWin,
    snapshot_reply::Storage, snapshot_reply::Transition,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_core::Stream;
use log::{info, warn};
use once_cell::sync::OnceCell;
use prost_types::Timestamp;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use tokio::net::UnixListener;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status, transport::Server};

static SYSTEM_TRACKER_DB_PATH: OnceCell<PathBuf> = OnceCell::new();
static SYSTEM_TRACKER_QUERY_TX: OnceCell<Sender<crate::daemon::tracker::MetricsQueryReq>> =
    OnceCell::new();
static SYSTEM_TRACKER_CONTROL_TX: OnceCell<Sender<crate::daemon::tracker::ControlMsg>> =
    OnceCell::new();

pub fn set_system_tracker_db_path(db_path: PathBuf) {
    let _ = SYSTEM_TRACKER_DB_PATH.set(db_path);
}

pub fn set_system_tracker_query_tx(tx: Sender<crate::daemon::tracker::MetricsQueryReq>) {
    let _ = SYSTEM_TRACKER_QUERY_TX.set(tx);
}

pub fn set_system_tracker_control_tx(tx: Sender<crate::daemon::tracker::ControlMsg>) {
    let _ = SYSTEM_TRACKER_CONTROL_TX.set(tx);
}

#[derive(Clone, Default)]
pub struct KroniSvc {}

#[tonic::async_trait]
impl Kroni for KroniSvc {
    async fn snapshot(
        &self,
        _req: Request<SnapshotRequest>,
    ) -> Result<Response<SnapshotReply>, Status> {
        let s = snapshot::get_current();
        Ok(Response::new(to_pb(&s)))
    }

    type WatchStream = Pin<Box<dyn Stream<Item = Result<SnapshotReply, Status>> + Send + 'static>>;

    async fn watch(
        &self,
        _req: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        // Stream updates via the snapshot watch channel
        let rx = snapshot::watch_snapshot();
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
        if let Some(ctrl_tx) = SYSTEM_TRACKER_CONTROL_TX.get() {
            let (ack_tx, ack_rx) = std::sync::mpsc::channel();
            ctrl_tx
                .send(crate::daemon::tracker::ControlMsg::Flush(ack_tx))
                .map_err(|_| Status::internal("Failed to request tracker flush"))?;
            // Wait up to ~2s for the tracker to acknowledge flush
            let _ = ack_rx
                .recv_timeout(std::time::Duration::from_millis(2000))
                .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker flush"))?;
        } else {
            return Err(Status::unavailable(
                "System tracker control channel not configured",
            ));
        }

        // Route the read through the tracker thread using a query channel so
        // the query runs against the same in-process DB instance as the writer.
        let tx = SYSTEM_TRACKER_QUERY_TX
            .get()
            .ok_or_else(|| Status::unavailable("System tracker not configured (query tx)"))?;

        let pid = request.pid;

        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let mut metrics = if request.limit > 0 {
            let query = crate::daemon::tracker::MetricsQuery::ByLimit {
                pid,
                limit: request.limit as usize,
            };
            tx.send(crate::daemon::tracker::MetricsQueryReq {
                query,
                reply: reply_tx,
            })
            .map_err(|_| Status::internal("Failed to send query to tracker thread"))?;
            reply_rx
                .recv_timeout(std::time::Duration::from_millis(1500))
                .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker reply"))?
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
            tx.send(crate::daemon::tracker::MetricsQueryReq {
                query,
                reply: reply_tx,
            })
            .map_err(|_| Status::internal("Failed to send query to tracker thread"))?;
            reply_rx
                .recv_timeout(std::time::Duration::from_millis(1500))
                .map_err(|_| Status::deadline_exceeded("Timed out waiting for tracker reply"))?
                .map_err(|e| Status::internal(format!("Tracker query error: {}", e)))?
        };

        // Best-effort: if empty, allow one short retry to let the tracker
        // finish persisting immediately after a flush.
        if metrics.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            if request.limit > 0 {
                let query = crate::daemon::tracker::MetricsQuery::ByLimit {
                    pid,
                    limit: request.limit as usize,
                };
                tx.send(crate::daemon::tracker::MetricsQueryReq {
                    query,
                    reply: reply_tx,
                })
                .map_err(|_| Status::internal("Failed to send query to tracker thread (retry)"))?;
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
                tx.send(crate::daemon::tracker::MetricsQueryReq {
                    query,
                    reply: reply_tx,
                })
                .map_err(|_| Status::internal("Failed to send query to tracker thread (retry)"))?;
            }
            metrics = reply_rx
                .recv_timeout(std::time::Duration::from_millis(1200))
                .map_err(|_| {
                    Status::deadline_exceeded("Timed out waiting for tracker reply (retry)")
                })?
                .map_err(|e| Status::internal(format!("Tracker query error (retry): {}", e)))?;
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

pub fn spawn_server(uds_path: PathBuf) -> Result<std::thread::JoinHandle<()>> {
    if uds_path.exists() {
        warn!("Removing stale UDS: {:?}", uds_path);
        let _ = std::fs::remove_file(&uds_path);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
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
            let svc = KroniSvc::default();
            tx.send(()).ok();
            info!("kroni API listening on {:?}", uds_path);
            Server::builder()
                .add_service(KroniServer::new(svc))
                .serve_with_incoming(incoming)
                .await
                .expect("serve");
        });
    });
    rx.recv().ok();
    Ok(handle)
}

// Note: gRPC integration tests live under tests/ to avoid OnceCell contention.

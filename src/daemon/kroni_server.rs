#![cfg(feature = "kroni-api")]

use crate::daemon::snapshot;
use crate::kroni_api::kroni::v1::kroni_server::{Kroni, KroniServer};
use crate::kroni_api::kroni::v1::{
    snapshot_reply::ActivityState as PbState, snapshot_reply::Cadence, snapshot_reply::Config,
    snapshot_reply::Counts, snapshot_reply::Focus, snapshot_reply::Replay,
    snapshot_reply::SnapshotApp as PbApp, snapshot_reply::SnapshotWindow as PbWin,
    snapshot_reply::Storage, snapshot_reply::Transition, SnapshotReply, SnapshotRequest,
    WatchRequest,
};
use anyhow::Result;
use futures_core::Stream;
use log::{info, warn};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;
use tonic::{transport::Server, Request, Response, Status};

#[derive(Clone, Default)]
struct KroniSvc {}

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
        let stream =
            IntervalStream::new(tokio::time::interval(std::time::Duration::from_millis(500)))
                .map(|_| Ok(to_pb(&snapshot::get_current())));
        Ok(Response::new(Box::pin(stream)))
    }
}

pub(crate) fn to_pb(s: &snapshot::Snapshot) -> SnapshotReply {
    let state = match s.activity_state {
        crate::daemon::records::ActivityState::Active => PbState::Active,
        crate::daemon::records::ActivityState::Passive => PbState::Passive,
        crate::daemon::records::ActivityState::Inactive => PbState::Inactive,
        crate::daemon::records::ActivityState::Locked => PbState::Locked,
    } as i32;
    let focus = s.focus.as_ref().map(|f| Focus {
        app: f.app_name.clone(),
        pid: f.pid,
        window_id: f.window_id.clone(),
        title: f.window_title.clone(),
        since_rfc3339: f.window_instance_start.to_rfc3339(),
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
        at_rfc3339: t.at.to_rfc3339(),
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
        last_flush_rfc3339: s
            .storage
            .last_flush_at
            .as_ref()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
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
    let replay = Some(Replay {
        mode: s.replay.mode.clone(),
        position: s.replay.position.unwrap_or(0),
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
                    first_seen_rfc3339: w.first_seen.to_rfc3339(),
                    last_seen_rfc3339: w.last_seen.to_rfc3339(),
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
        next_timeout_rfc3339: s
            .next_timeout
            .as_ref()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        storage,
        config,
        replay,
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

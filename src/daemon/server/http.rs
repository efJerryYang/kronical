use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::daemon::snapshot;
use crate::util::logging::{debug, info, warn};
use anyhow::{Context, Result};
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::{Json, Router, routing::get};
use futures_util::stream::Stream;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tower::Service;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
static HTTP_ACCEPT_BUDGET: AtomicUsize = AtomicUsize::new(usize::MAX);

#[cfg(test)]
pub(crate) fn set_http_accept_budget(budget: usize) {
    HTTP_ACCEPT_BUDGET.store(budget, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn reset_http_accept_budget() {
    HTTP_ACCEPT_BUDGET.store(usize::MAX, Ordering::SeqCst);
}

async fn snapshot_handler(
    State(snapshot_bus): State<Arc<snapshot::SnapshotBus>>,
) -> Json<snapshot::Snapshot> {
    let s = snapshot_bus.snapshot();
    Json((*s).clone())
}

async fn stream_handler(
    State(snapshot_bus): State<Arc<snapshot::SnapshotBus>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::WatchStream;
    let rx = snapshot_bus.watch_snapshot();
    let stream = WatchStream::new(rx).map(|snap| {
        let data = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().data(data))
    });
    Sse::new(stream)
}

pub fn spawn_http_server(
    uds_path: PathBuf,
    snapshot_bus: Arc<snapshot::SnapshotBus>,
    threads: ThreadRegistry,
) -> Result<ThreadHandle> {
    if uds_path.exists() {
        warn!("Removing stale HTTP admin UDS: {:?}", uds_path);
        let _ = std::fs::remove_file(&uds_path);
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    let bus_for_thread = Arc::clone(&snapshot_bus);
    let handle = threads
        .spawn("http-server", move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let app = Router::new()
                .route("/v1/snapshot", get(snapshot_handler))
                .route("/v1/stream", get(stream_handler))
                .with_state(bus_for_thread);
            let listener = UnixListener::bind(&uds_path).expect("bind unix listener");
            tx.send(()).ok();
            info!("HTTP admin listening on {:?}", uds_path);

            let make_service = app.into_make_service();

            loop {
                #[cfg(test)]
                {
                    if HTTP_ACCEPT_BUDGET.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                }
                let (stream, _) = listener.accept().await.expect("accept unix connection");
                #[cfg(test)]
                {
                    HTTP_ACCEPT_BUDGET.fetch_sub(1, Ordering::SeqCst);
                }
                let io = TokioIo::new(stream);
                let mut make_svc = make_service.clone();

                tokio::task::spawn(async move {
                    let service = make_svc.call(()).await.expect("create service");
                    let hyper_service = TowerToHyperService::new(service);
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(io, hyper_service)
                        .await
                    {
                        // Treat client-initiated disconnects (e.g., quitting the monitor)
                        // as a normal shutdown; hyper reports these as IncompleteMessage.
                        if err.is_incomplete_message() {
                            debug!("Client disconnected while streaming (incomplete message) — normal exit");
                        } else {
                            eprintln!("Error serving connection: {:?}", err);
                        }
                    }
                });
            }
        });
    })
    .context("spawn HTTP server thread")?;
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
                "HTTP server failed to signal readiness within 500ms{detail}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::events::WindowFocusInfo;
    use crate::daemon::records::ActivityState;
    use crate::daemon::runtime::ThreadRegistry;
    use crate::daemon::snapshot::{
        ConfigSummary, Counts as SnapshotCounts, SnapshotBus, StorageInfo,
    };
    use axum::Json;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_focus() -> WindowFocusInfo {
        WindowFocusInfo {
            pid: 9001,
            process_start_time: 777,
            app_name: Arc::new("Terminal".to_string()),
            window_title: Arc::new("build".to_string()),
            window_id: 41,
            window_instance_start: Utc.with_ymd_and_hms(2024, 5, 1, 7, 0, 0).unwrap(),
            window_position: None,
            window_size: Some((1440, 900)),
        }
    }

    fn publish_sample(bus: &SnapshotBus, title_suffix: &str) {
        let focus = sample_focus();
        let mut focus = focus;
        focus.window_title = Arc::new(format!("build-{title_suffix}"));
        bus.publish_basic(
            ActivityState::Active,
            Some(focus),
            None,
            SnapshotCounts {
                signals_seen: 1,
                hints_seen: 2,
                records_emitted: 3,
            },
            500,
            "tick".into(),
            None,
            StorageInfo {
                backlog_count: 0,
                last_flush_at: None,
            },
            ConfigSummary::default(),
            vec!["healthy".into()],
            Vec::new(),
        );
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

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_handler_returns_latest_state() {
        let bus = Arc::new(SnapshotBus::new());
        publish_sample(&bus, "initial");

        let Json(payload) = snapshot_handler(State(Arc::clone(&bus))).await;
        assert_eq!(payload.activity_state, ActivityState::Active);
        let focus = payload.focus.expect("focus exists");
        assert_eq!(*focus.window_title, "build-initial");
        assert_eq!(payload.counts.records_emitted, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_handler_emits_initial_and_follow_up_events() {
        let bus = Arc::new(SnapshotBus::new());
        publish_sample(&bus, "initial");

        let sse = stream_handler(State(Arc::clone(&bus))).await;
        let response = sse.into_response();
        let headers = response.headers();
        assert_eq!(
            headers.get("content-type").map(|v| v.to_str().unwrap()),
            Some("text/event-stream"),
        );
        assert_eq!(
            headers.get("cache-control").map(|v| v.to_str().unwrap()),
            Some("no-cache"),
        );

        publish_sample(&bus, "next");
        let current_focus = bus.snapshot().focus.clone().expect("bus focus");
        assert_eq!(*current_focus.window_title, "build-next");
    }

    #[test]
    #[ignore = "requires UDS permissions"]
    fn http_server_spawns_and_respects_accept_budget() {
        let uds_path = test_socket_path("http");
        // Create a stale file to ensure the spawner removes it without panic.
        std::fs::write(&uds_path, b"stale").expect("write stale");

        let bus = Arc::new(SnapshotBus::new());
        publish_sample(&bus, "spawn");

        super::set_http_accept_budget(0);
        let handle = match super::spawn_http_server(
            uds_path.clone(),
            Arc::clone(&bus),
            ThreadRegistry::new(),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Operation not permitted")
                        || msg.contains("failed to signal readiness"),
                    "unexpected spawn error: {}",
                    msg
                );
                super::reset_http_accept_budget();
                let _ = std::fs::remove_file(&uds_path);
                return;
            }
        };

        handle.join().expect("join http thread");
        let _ = std::fs::remove_file(&uds_path);
        super::reset_http_accept_budget();
    }
}

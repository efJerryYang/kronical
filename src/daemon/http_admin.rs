#![cfg(feature = "http-admin")]

use crate::daemon::snapshot;
use anyhow::Result;
use axum::response::sse::{Event, Sse};
use axum::{routing::get, Json, Router};
use futures_util::stream::Stream;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use log::{info, warn};
use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixListener;
use tower::Service;

async fn snapshot_handler() -> Json<snapshot::Snapshot> {
    let s = snapshot::get_current();
    Json((*s).clone())
}

async fn stream_handler() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use tokio_stream::wrappers::IntervalStream;
    use tokio_stream::StreamExt;
    let stream = IntervalStream::new(tokio::time::interval(Duration::from_millis(500))).map(|_| {
        let snap = snapshot::get_current();
        let data = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().data(data))
    });
    Sse::new(stream)
}

pub fn spawn_http_admin(uds_path: PathBuf) -> Result<std::thread::JoinHandle<()>> {
    if uds_path.exists() {
        warn!("Removing stale HTTP admin UDS: {:?}", uds_path);
        let _ = std::fs::remove_file(&uds_path);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let app = Router::new()
                .route("/v1/snapshot", get(snapshot_handler))
                .route("/v1/stream", get(stream_handler));
            tx.send(()).ok();
            let listener = UnixListener::bind(&uds_path).expect("bind unix listener");
            info!("HTTP admin listening on {:?}", uds_path);

            let make_service = app.into_make_service();

            loop {
                let (stream, _) = listener.accept().await.expect("accept unix connection");
                let io = TokioIo::new(stream);
                let mut make_svc = make_service.clone();

                tokio::task::spawn(async move {
                    let service = make_svc.call(()).await.expect("create service");
                    let hyper_service = TowerToHyperService::new(service);
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(io, hyper_service)
                        .await
                    {
                        eprintln!("Error serving connection: {:?}", err);
                    }
                });
            }
        });
    });
    rx.recv().ok();
    Ok(handle)
}

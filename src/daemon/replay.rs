use crate::daemon::event_deriver::LockDeriver;
use crate::daemon::records::{aggregate_activities_since, ActivityRecord, RecordProcessor};
use crate::daemon::socket_server::SocketServer;
use crate::storage::StorageBackend;
use anyhow::Result;
use chrono::{DateTime, Utc};
use log::info;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn run_replay(
    mut store: Box<dyn StorageBackend>,
    workspace_sock_dir: PathBuf,
    speed: f64,
    retention_minutes: u64,
    thresholds: (u64, u64),
) -> Result<()> {
    let socket_path = workspace_sock_dir.join("chronicle.replay.sock");
    let server = Arc::new(SocketServer::new(socket_path, 60, 3));
    server.start()?;

    let now = Utc::now();
    let since = now - chrono::Duration::minutes(retention_minutes as i64);
    // Fetch raw envelopes
    let mut envelopes = store.fetch_envelopes_between(since, now)?;
    envelopes.sort_by_key(|e| e.timestamp);

    let mut deriver = LockDeriver::new();
    let mut rp = RecordProcessor::with_thresholds(thresholds.0, thresholds.1);
    let mut recent_records: std::collections::VecDeque<ActivityRecord> =
        std::collections::VecDeque::new();
    let mut state_hist: std::collections::VecDeque<char> = std::collections::VecDeque::new();

    info!(
        "Starting replay: {} envelopes from {} to {} at {}x",
        envelopes.len(),
        since,
        now,
        speed
    );

    if envelopes.is_empty() {
        return Ok(());
    }

    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut last_push = std::time::Instant::now();
    for env in envelopes.into_iter() {
        if let Some(prev) = last_ts {
            let delta_ms = (env.timestamp - prev).num_milliseconds();
            if delta_ms > 0 {
                let scaled = (delta_ms as f64 / speed).max(0.0);
                if scaled >= 1.0 {
                    thread::sleep(Duration::from_millis(scaled as u64));
                }
            }
        }
        last_ts = Some(env.timestamp);

        let derived = deriver.derive(&vec![env]);
        let new_records = rp.process_envelopes(derived);
        for r in new_records {
            let ch = match r.state {
                crate::daemon::records::ActivityState::Active => 'A',
                crate::daemon::records::ActivityState::Passive => 'P',
                crate::daemon::records::ActivityState::Inactive => 'I',
                crate::daemon::records::ActivityState::Locked => 'L',
            };
            if state_hist.back().copied() != Some(ch) {
                state_hist.push_back(ch);
                if state_hist.len() > 10 {
                    state_hist.pop_front();
                }
                let hist_str: String = state_hist.iter().collect();
                server.update_state_history(hist_str);
            }
            recent_records.push_back(r);
        }

        if last_push.elapsed() >= Duration::from_millis(100) {
            let now_sim = last_ts.unwrap_or(now);
            let since_sim = now_sim - chrono::Duration::minutes(retention_minutes as i64);
            while let Some(front) = recent_records.front() {
                let end = front.end_time.unwrap_or(now_sim);
                if end < since_sim {
                    recent_records.pop_front();
                } else {
                    break;
                }
            }
            let recs: Vec<ActivityRecord> = recent_records.iter().cloned().collect();
            let agg = aggregate_activities_since(&recs, since_sim, now_sim, 60, 3, 30);
            server.update_aggregated_data(agg);
            last_push = std::time::Instant::now();
        }
    }

    if let Some(now_sim) = last_ts {
        let since_sim = now_sim - chrono::Duration::minutes(retention_minutes as i64);
        let recs: Vec<ActivityRecord> = recent_records.iter().cloned().collect();
        let agg = aggregate_activities_since(&recs, since_sim, now_sim, 60, 3, 30);
        server.update_aggregated_data(agg);
    }

    Ok(())
}

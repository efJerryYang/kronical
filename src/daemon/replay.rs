use crate::daemon::event_model::{EventKind, HintKind};
use crate::daemon::records::{ActivityRecord, RecordBuilder, aggregate_activities_since};
use crate::storage::StorageBackend;
use anyhow::Result;
use chrono::{DateTime, Utc};
use log::info;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

pub fn run_replay(
    mut store: Box<dyn StorageBackend>,
    _workspace_sock_dir: PathBuf,
    speed: f64,
    retention_minutes: u64,
    thresholds: (u64, u64),
) -> Result<()> {
    // legacy monitor socket removed for replay

    let now = Utc::now();
    let since = now - chrono::Duration::minutes(retention_minutes as i64);
    // Fetch raw envelopes
    let mut envelopes = store.fetch_envelopes_between(since, now)?;
    envelopes.sort_by_key(|e| e.timestamp);

    // Detect whether the stored envelopes already contain StateChanged hints
    let has_state_hints = envelopes
        .iter()
        .any(|e| matches!(e.kind, EventKind::Hint(HintKind::StateChanged)));
    let mut state_deriver = if has_state_hints {
        None
    } else {
        Some(crate::daemon::event_deriver::StateDeriver::new(
            since,
            thresholds.0 as i64,
            thresholds.1 as i64,
        ))
    };
    let mut builder = RecordBuilder::new(crate::daemon::records::ActivityState::Inactive);
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

        // Prefer stored hints; if state hints absent, derive from signals on the fly
        let mut completed: Vec<ActivityRecord> = Vec::new();
        match &env.kind {
            EventKind::Hint(_) => {
                if let Some(rec) = builder.on_hint(&env) {
                    completed.push(rec);
                }
            }
            EventKind::Signal(_) => {
                if let Some(sd) = state_deriver.as_mut().and_then(|d| d.on_signal(&env)) {
                    if let Some(rec) = builder.on_hint(&sd) {
                        completed.push(rec);
                    }
                }
            }
        }
        for r in completed.into_iter() {
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
                let _ = hist_str;
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
            let _agg = aggregate_activities_since(
                recent_records.make_contiguous(),
                since_sim,
                now_sim,
                60,
                3,
                30,
            );
            last_push = std::time::Instant::now();
        }
    }

    if let Some(now_sim) = last_ts {
        let since_sim = now_sim - chrono::Duration::minutes(retention_minutes as i64);
        let _agg = aggregate_activities_since(
            recent_records.make_contiguous(),
            since_sim,
            now_sim,
            60,
            3,
            30,
        );
    }

    Ok(())
}

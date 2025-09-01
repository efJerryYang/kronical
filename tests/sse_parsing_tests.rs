use chrono::{DateTime, Utc};
use kronical::daemon::snapshot::Snapshot;

fn assemble_sse_payload(json: &str) -> String {
    format!("data: {}\n\n", json)
}

#[test]
fn parses_single_sse_event_into_snapshot() {
    let snap = Snapshot {
        seq: 1,
        mono_ns: 0,
        activity_state: kronical::daemon::records::ActivityState::Inactive,
        focus: None,
        last_transition: None,
        counts: Default::default(),
        cadence_ms: 0,
        cadence_reason: String::new(),
        next_timeout: None,
        storage: Default::default(),
        config: Default::default(),
        replay: Default::default(),
        health: vec![],
        aggregated_apps: vec![],
    };
    let json = serde_json::to_string(&snap).unwrap();
    let sse = assemble_sse_payload(&json);

    // Minimal SSE parser similar to kronictl monitor loop
    let mut data_buf = String::new();
    for line in sse.lines() {
        if line.starts_with("data:") {
            let payload = line[5..].trim();
            data_buf.push_str(payload);
            data_buf.push('\n');
        } else if line.is_empty() { // event delimiter
             // complete event
        }
    }
    let parsed: Snapshot = serde_json::from_str(data_buf.trim_end()).unwrap();
    assert_eq!(parsed.seq, 1);
}

#[test]
fn parses_multiple_sse_events() {
    let mut payload = String::new();
    for i in 0..3u64 {
        let mut s = Snapshot::empty();
        s.seq = i;
        let json = serde_json::to_string(&s).unwrap();
        payload.push_str(&format!("data: {}\n\n", json));
    }

    let mut frames: Vec<Snapshot> = Vec::new();
    let mut data_buf = String::new();
    for line in payload.lines() {
        if line.starts_with("data:") {
            let payload = line[5..].trim();
            data_buf.push_str(payload);
            data_buf.push('\n');
        } else if line.is_empty() {
            if !data_buf.is_empty() {
                let snap: Snapshot = serde_json::from_str(data_buf.trim_end()).unwrap();
                frames.push(snap);
                data_buf.clear();
            }
        }
    }
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].seq, 0);
    assert_eq!(frames[2].seq, 2);
}

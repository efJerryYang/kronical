use kronical::daemon::records::ActivityState;
use kronical::daemon::snapshot::{
    ConfigSummary, Counts, Snapshot, SnapshotApp, SnapshotWindow, StorageInfo, Transition,
};

#[test]
fn maps_aggregated_apps_in_grpc_snapshot_reply() {
    let now = chrono::Utc::now();
    let s = Snapshot {
        seq: 42,
        mono_ns: 123,
        activity_state: ActivityState::Active,
        focus: None,
        last_transition: Some(Transition {
            from: ActivityState::Inactive,
            to: ActivityState::Active,
            at: now,
        }),
        counts: Counts {
            signals_seen: 1,
            hints_seen: 2,
            records_emitted: 3,
        },
        cadence_ms: 2000,
        cadence_reason: "Active".into(),
        next_timeout: Some(now + chrono::Duration::milliseconds(2000)),
        storage: StorageInfo {
            backlog_count: 7,
            last_flush_at: Some(now),
        },
        config: ConfigSummary {
            active_grace_secs: 5,
            idle_threshold_secs: 300,
            retention_minutes: 60,
            ephemeral_max_duration_secs: 60,
            ephemeral_min_distinct_ids: 3,
            ephemeral_app_max_duration_secs: 45,
            ephemeral_app_min_distinct_procs: 2,
        },

        health: vec!["ok".into()],
        aggregated_apps: vec![SnapshotApp {
            app_name: "vim".into(),
            pid: 1234,
            process_start_time: 0,
            windows: vec![SnapshotWindow {
                window_id: "w1".into(),
                window_title: "main.rs".into(),
                first_seen: now - chrono::Duration::minutes(5),
                last_seen: now,
                duration_seconds: 300,
                is_group: false,
            }],
            total_duration_secs: 300,
            total_duration_pretty: "5m".into(),
        }],
    };

    let reply = kronical::daemon::kroni_server::to_pb(&s);
    assert_eq!(reply.seq, 42);
    assert_eq!(reply.aggregated_apps.len(), 1);
    let a = &reply.aggregated_apps[0];
    assert_eq!(a.app_name, "vim");
    assert_eq!(a.pid, 1234);
    assert_eq!(a.total_duration_secs, 300);
    assert_eq!(a.windows.len(), 1);
    assert_eq!(a.windows[0].window_id, "w1");
    assert_eq!(a.windows[0].duration_seconds, 300);
}

# Demo: Snapshot Command Output

## The `snapshot` Command and socket API

The `kronictl snapshot` command retrieves the current snapshot of Kronical's activity tracking data. This snapshot includes information about the current state, focus, recent activity records, and aggregated application usage. `--pretty` flag formats the output for better readability for human, it is recommended to use the raw JSON output for programmatic consumption. Some `jq` query examples are listed below in section **`jq` query examples**.

Socket API is just producing the same JSON but no indentation. Current sockets live under `~/.kronical/` as `kronid.sock` (gRPC) and `kronid.http.sock` (HTTP/SSE).

```txt
❯ curl --unix-socket ~/.kronical/kronid.http.sock http://localhost/v1/snapshot
```

```txt
❯ grpcurl -plaintext -unix -import-path proto -proto proto/kroni.proto ~/.kronical/kronid.sock kroni.v1.Snapshot/Current
```

```txt
❯ kronictl snapshot --pretty
Kronical Snapshot
- seq: 1279
- mono_ns: 1767977833536112000
- run_id: MWACA8ZAQraJnaufEIgaRw
- state: Active
- focus: Code [71458] - Kronical Snapshot • Untitled-5 — kronical
- last_transition: Inactive -> Active at 2026-01-09 16:54:10.484314 UTC
- cadence: 2000ms (Active)
- next_timeout: 2026-01-09 16:57:15.536107 UTC
- counts: signals=4688 hints=18 records=17
- storage: backlog=2 last_flush=2026-01-09T16:57:05+00:00
- config: active=30s idle=300s retention=4320m eph_max=60s eph_min=3 eph_app_max=60s eph_app_min_procs=3
- apps (1):
  - app: Code [71458] total=1m30s (90s)
    windows (1):
      - title: Kronical Snapshot • Untitled-5 — kronical
        id: 35012 first_seen: 2026-01-09 16:54:27.059290 UTC last_seen: 2026-01-09 16:56:26.123105 UTC dur: 90s
- records (17):
  - id=0 state=Active start=2026-01-09T16:54:10.484392+00:00 end=2026-01-09T16:54:26.258302+00:00 dur=15s events=0 triggers=0
  - id=1 state=Active start=2026-01-09T16:54:26.258302+00:00 end=2026-01-09T16:54:27.059290+00:00 dur=0s events=0 triggers=0
    focus: Google Chrome [72295] - Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1) (wid=40521)
  - id=2 state=Active start=2026-01-09T16:54:27.059290+00:00 end=2026-01-09T16:55:24.221872+00:00 dur=57s events=0 triggers=0
    focus: Code [71458] - run_id.rs — kronical (wid=35012)
  - id=3 state=Active start=2026-01-09T16:55:24.221872+00:00 end=2026-01-09T16:55:29.540334+00:00 dur=5s events=0 triggers=0
    focus: Google Chrome [72295] - Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1) (wid=40521)
  - id=4 state=Active start=2026-01-09T16:55:29.540335+00:00 end=2026-01-09T16:55:35.300060+00:00 dur=5s events=0 triggers=0
    focus: Code [71458] - kroni — kronii (wid=35012)
  - id=5 state=Active start=2026-01-09T16:55:35.300060+00:00 end=2026-01-09T16:55:41.820688+00:00 dur=6s events=0 triggers=0
    focus: Google Chrome [72295] - Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1) (wid=40521)
  - id=6 state=Active start=2026-01-09T16:55:41.820688+00:00 end=2026-01-09T16:55:43.021494+00:00 dur=1s events=0 triggers=0
    focus: WeChat [2165] - WeChat (Chats) (wid=145)
  - id=7 state=Active start=2026-01-09T16:55:43.021494+00:00 end=2026-01-09T16:55:47.132865+00:00 dur=4s events=0 triggers=0
    focus: Google Chrome [72295] - Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1) (wid=40521)
  - id=8 state=Active start=2026-01-09T16:55:47.132865+00:00 end=2026-01-09T16:55:49.904610+00:00 dur=2s events=0 triggers=0
    focus: Code [71458] - run_id.rs — kronical (wid=35012)
  - id=9 state=Active start=2026-01-09T16:55:49.904611+00:00 end=2026-01-09T16:55:54.329639+00:00 dur=4s events=0 triggers=0
    focus: WeChat [2165] - WeChat (Chats) (wid=145)
  - id=10 state=Active start=2026-01-09T16:55:54.329639+00:00 end=2026-01-09T16:56:04.345447+00:00 dur=10s events=0 triggers=0
    focus: Code [71458] - run_id.rs — kronical (wid=35012)
  - id=11 state=Active start=2026-01-09T16:56:04.345448+00:00 end=2026-01-09T16:56:19.032086+00:00 dur=14s events=0 triggers=0
    focus: Code [71458] - Kronical Snapshot • Untitled-5 — kronical (wid=35012)
  - id=12 state=Active start=2026-01-09T16:56:19.032087+00:00 end=2026-01-09T16:56:20.766522+00:00 dur=1s events=0 triggers=0
    focus: WeChat [2165] - WeChat (Chats) (wid=145)
  - id=13 state=Active start=2026-01-09T16:56:20.766522+00:00 end=2026-01-09T16:56:21.088268+00:00 dur=0s events=0 triggers=0
    focus: Code [71458] - Kronical Snapshot • Untitled-5 — kronical (wid=35012)
  - id=14 state=Active start=2026-01-09T16:56:21.088268+00:00 end=2026-01-09T16:56:24.089578+00:00 dur=3s events=0 triggers=0
    focus: WeChat [2165] - WeChat (Chats) (wid=145)
  - id=15 state=Active start=2026-01-09T16:56:24.089578+00:00 end=2026-01-09T16:56:26.123105+00:00 dur=2s events=0 triggers=0
    focus: Code [71458] - Kronical Snapshot • Untitled-5 — kronical (wid=35012)
  - id=16 state=Active start=2026-01-09T16:56:26.123107+00:00 end=2026-01-09T16:56:26.792787+00:00 dur=0s events=0 triggers=0
    focus: Code [71458] - kroni — kronii (wid=35012)
```

```txt
❯ kronictl snapshot
{
  "seq": 1290,
  "monoNs": 1767977837074965000,
  "runId": "MWACA8ZAQraJnaufEIgaRw",
  "activityState": "Active",
  "focus": {
    "pid": 71458,
    "processStartTime": 0,
    "appName": "Code",
    "windowTitle": "Kronical Snapshot • Untitled-5 — kronical",
    "windowId": 35017,
    "windowInstanceStart": "2026-01-09T16:56:24.075029Z",
    "windowPosition": null,
    "windowSize": null
  },
  "lastTransition": {
    "from": "Inactive",
    "to": "Active",
    "at": "2026-01-09T16:54:10.484314Z",
    "by_signal": null,
    "runId": "MWACA8ZAQraJnaufEIgaRw"
  },
  "transitionsRecent": [],
  "counts": {
    "signals_seen": 4702,
    "hints_seen": 18,
    "records_emitted": 17
  },
  "cadenceMs": 2000,
  "cadenceReason": "Active",
  "nextTimeout": "2026-01-09T16:57:19.074955Z",
  "storage": {
    "backlogCount": 1,
    "lastFlushAt": "2026-01-09T16:57:16Z"
  },
  "config": {
    "activeGraceSecs": 30,
    "idleThresholdSecs": 300,
    "retentionMinutes": 4320,
    "ephemeralMaxDurationSecs": 60,
    "ephemeralMinDistinctIds": 3,
    "ephemeralAppMaxDurationSecs": 60,
    "ephemeralAppMinDistinctProcs": 3
  },
  "health": [],
  "aggregatedApps": [
    {
      "appName": "Code",
      "pid": 71458,
      "processStartTime": 1767592026,
      "windows": [
        {
          "windowId": "35012",
          "windowTitle": "Kronical Snapshot • Untitled-5 — kronical",
          "firstSeen": "2026-01-09T16:54:27.059290Z",
          "lastSeen": "2026-01-09T16:56:26.123105Z",
          "durationSeconds": 90,
          "is_group": false
        }
      ],
      "totalDurationSecs": 90,
      "totalDurationPretty": "1m30s"
    }
  ],
  "records": [
    {
      "recordId": 0,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:54:10.484392Z",
      "endTime": "2026-01-09T16:54:26.258302Z",
      "state": "Active",
      "focus": null,
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 1,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:54:26.258302Z",
      "endTime": "2026-01-09T16:54:27.059290Z",
      "state": "Active",
      "focus": {
        "pid": 72295,
        "processStartTime": 0,
        "appName": "Google Chrome",
        "windowTitle": "Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1)",
        "windowId": 40521,
        "windowInstanceStart": "2026-01-09T16:54:26.242122Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 2,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:54:27.059290Z",
      "endTime": "2026-01-09T16:55:24.221872Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "run_id.rs — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:54:27.053430Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 3,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:24.221872Z",
      "endTime": "2026-01-09T16:55:29.540334Z",
      "state": "Active",
      "focus": {
        "pid": 72295,
        "processStartTime": 0,
        "appName": "Google Chrome",
        "windowTitle": "Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1)",
        "windowId": 40521,
        "windowInstanceStart": "2026-01-09T16:55:24.196599Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 4,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:29.540335Z",
      "endTime": "2026-01-09T16:55:35.300060Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "kroni — kronii",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:55:29.500971Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 5,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:35.300060Z",
      "endTime": "2026-01-09T16:55:41.820688Z",
      "state": "Active",
      "focus": {
        "pid": 72295,
        "processStartTime": 0,
        "appName": "Google Chrome",
        "windowTitle": "Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1)",
        "windowId": 40521,
        "windowInstanceStart": "2026-01-09T16:55:35.297326Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 6,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:41.820688Z",
      "endTime": "2026-01-09T16:55:43.021494Z",
      "state": "Active",
      "focus": {
        "pid": 2165,
        "processStartTime": 0,
        "appName": "WeChat",
        "windowTitle": "WeChat (Chats)",
        "windowId": 145,
        "windowInstanceStart": "2026-01-09T16:55:41.815638Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 7,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:43.021494Z",
      "endTime": "2026-01-09T16:55:47.132865Z",
      "state": "Active",
      "focus": {
        "pid": 72295,
        "processStartTime": 0,
        "appName": "Google Chrome",
        "windowTitle": "Connecting agents to tools | Letta Docs - Google Chrome - Jerry (Person 1)",
        "windowId": 40521,
        "windowInstanceStart": "2026-01-09T16:55:42.944424Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 8,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:47.132865Z",
      "endTime": "2026-01-09T16:55:49.904610Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "run_id.rs — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:55:47.117590Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 9,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:49.904611Z",
      "endTime": "2026-01-09T16:55:54.329639Z",
      "state": "Active",
      "focus": {
        "pid": 2165,
        "processStartTime": 0,
        "appName": "WeChat",
        "windowTitle": "WeChat (Chats)",
        "windowId": 145,
        "windowInstanceStart": "2026-01-09T16:55:49.855320Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 10,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:55:54.329639Z",
      "endTime": "2026-01-09T16:56:04.345447Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "run_id.rs — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:55:54.309291Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 11,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:04.345448Z",
      "endTime": "2026-01-09T16:56:19.032086Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "Kronical Snapshot • Untitled-5 — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:55:54.309291Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 12,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:19.032087Z",
      "endTime": "2026-01-09T16:56:20.766522Z",
      "state": "Active",
      "focus": {
        "pid": 2165,
        "processStartTime": 0,
        "appName": "WeChat",
        "windowTitle": "WeChat (Chats)",
        "windowId": 145,
        "windowInstanceStart": "2026-01-09T16:56:19.003605Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 13,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:20.766522Z",
      "endTime": "2026-01-09T16:56:21.088268Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "Kronical Snapshot • Untitled-5 — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:56:20.755418Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 14,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:21.088268Z",
      "endTime": "2026-01-09T16:56:24.089578Z",
      "state": "Active",
      "focus": {
        "pid": 2165,
        "processStartTime": 0,
        "appName": "WeChat",
        "windowTitle": "WeChat (Chats)",
        "windowId": 145,
        "windowInstanceStart": "2026-01-09T16:56:21.083138Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 15,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:24.089578Z",
      "endTime": "2026-01-09T16:56:26.123105Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "Kronical Snapshot • Untitled-5 — kronical",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:56:24.075029Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    },
    {
      "recordId": 16,
      "runId": "MWACA8ZAQraJnaufEIgaRw",
      "startTime": "2026-01-09T16:56:26.123107Z",
      "endTime": "2026-01-09T16:56:26.792787Z",
      "state": "Active",
      "focus": {
        "pid": 71458,
        "processStartTime": 0,
        "appName": "Code",
        "windowTitle": "kroni — kronii",
        "windowId": 35012,
        "windowInstanceStart": "2026-01-09T16:56:24.075029Z",
        "windowPosition": null,
        "windowSize": null
      },
      "eventCount": 0,
      "triggeringEvents": []
    }
  ]
}
```

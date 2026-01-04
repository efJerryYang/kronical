# Demo Snapshot Output

You can get a more detailed version without the `--pretty` flag, and it will generate the full JSON output.

```txt
❯ kronictl snapshot --pretty
Kronical Snapshot
- seq: 8364
- mono_ns: 1767500086306331000
- run_id: PFgwShVsR5OM1HacwuD4jw
- state: Active
- focus: iTerm2 [2159] - jerry@Jerrys-MacBook-Pro:~
- last_transition: Inactive -> Active at 2026-01-04 03:58:00.152783 UTC
- cadence: 2000ms (Active)
- next_timeout: 2026-01-04 04:14:48.306309 UTC
- counts: signals=37918 hints=106 records=105
- storage: backlog=1 last_flush=2026-01-04T04:14:45+00:00
- config: active=30s idle=300s retention=4320m eph_max=60s eph_min=3 eph_app_max=60s eph_app_min_procs=3
- apps (4):
  - app: Code [46988] total=1m58s (118s)
    windows (2):
      - title: demo-monitor.png — kronical
        id: 23269 first_seen: 2026-01-04 03:13:14.725005 UTC last_seen: 2026-01-04 04:14:37.782609 UTC dur: 116s
      - title: README.md — kronical
        id: 30507 first_seen: 2026-01-04 04:14:06.172144 UTC last_seen: 2026-01-04 04:14:08.185614 UTC dur: 2s
  - app: iTerm2 [2159] total=1m29s (89s)
    windows (1):
      - title: kronictl monitor
        id: 22626 first_seen: 2026-01-04 03:58:30.600844 UTC last_seen: 2026-01-04 04:14:02.535290 UTC dur: 89s
  - app: Google Chrome [72295] total=13m39s (819s)
    windows (10):
      - title: jaywalnut310/vits: VITS: Conditional Variational Autoencoder with Adversarial Learning for End-to-End Text-to-Speech - Google Chrome - Jerry (Person 1)
        id: 15233 first_seen: 2026-01-04 03:58:48.133149 UTC last_seen: 2026-01-04 04:12:49.549096 UTC dur: 580s
      - title: efJerryYang (Jerry Yang) - Google Chrome - Jerry (Person 1)
        id: 30466 first_seen: 2026-01-04 04:12:07.070385 UTC last_seen: 2026-01-04 04:12:15.070916 UTC dur: 8s
      - title: Window Change hook obtained outdated information after App Change hook · Issue #9 · efJerryYang/winshift-rs - Google Chrome - Jerry (Person 1)
        id: 30463 first_seen: 2026-01-04 04:10:20.678931 UTC last_seen: 2026-01-04 04:12:03.067536 UTC dur: 56s
      - title: github-readme-stats - Overview – Vercel - Google Chrome - Jerry (Person 1)
        id: 30435 first_seen: 2026-01-04 04:07:44.094856 UTC last_seen: 2026-01-04 04:07:52.150189 UTC dur: 8s
      - title: New Fine-grained Personal Access Token - Google Chrome - Jerry (Person 1)
        id: 16670 first_seen: 2026-01-04 04:02:48.979819 UTC last_seen: 2026-01-04 04:04:45.494327 UTC dur: 31s
      - title: Personal Access Tokens (Classic) - Google Chrome - Jerry (Person 1)
        id: 30404 first_seen: 2026-01-04 04:04:15.376380 UTC last_seen: 2026-01-04 04:04:25.406277 UTC dur: 10s
      - title: efJerryYang/github-readme-stats: :zap: Dynamically generated stats for your github readmes - Google Chrome - Jerry (Person 1)
        id: 30401 first_seen: 2026-01-04 04:02:55.102751 UTC last_seen: 2026-01-04 04:04:13.374953 UTC dur: 70s
      - title: New Project - Google Chrome - Jerry (Person 1)
        id: 30393 first_seen: 2026-01-04 04:02:15.855799 UTC last_seen: 2026-01-04 04:02:31.885140 UTC dur: 15s
      - title: Login – Vercel - Google Chrome - Jerry (Person 1)
        id: 30386 first_seen: 2026-01-04 04:01:16.615059 UTC last_seen: 2026-01-04 04:01:18.602547 UTC dur: 1s
      - title: Fork anuraghazra/github-readme-stats - Google Chrome - Jerry (Person 1)
        id: 30380 first_seen: 2026-01-04 03:59:46.298152 UTC last_seen: 2026-01-04 04:00:56.564209 UTC dur: 40s
  - app: loginwindow [0] total=43m49s (2629s)
    windows (1):
      - title: Login
        id: 4294967295 first_seen: 2026-01-04 03:14:10.271488 UTC last_seen: 2026-01-04 03:58:00.184898 UTC dur: 2629s
- records (105):
  - id=1 state=Active start=2026-01-04T03:12:55.196359+00:00 end=2026-01-04T03:13:14.725005+00:00 dur=19s events=0 triggers=0
  - id=2 state=Active start=2026-01-04T03:13:14.725005+00:00 end=2026-01-04T03:13:45.897383+00:00 dur=31s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=3 state=Passive start=2026-01-04T03:13:45.897383+00:00 end=2026-01-04T03:13:52.385676+00:00 dur=6s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=4 state=Active start=2026-01-04T03:13:52.385676+00:00 end=2026-01-04T03:14:10.271445+00:00 dur=17s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=5 state=Locked start=2026-01-04T03:14:10.271445+00:00 end=2026-01-04T03:14:10.271488+00:00 dur=0s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=6 state=Locked start=2026-01-04T03:14:10.271488+00:00 end=2026-01-04T03:58:00.184898+00:00 dur=2629s events=0 triggers=0
    focus: loginwindow [0] - Login (wid=4294967295)
  - id=7 state=Inactive start=2026-01-04T03:58:00.184898+00:00 end=2026-01-04T03:58:00.184949+00:00 dur=0s events=0 triggers=0
    focus: loginwindow [0] - Login (wid=4294967295)
  - id=8 state=Active start=2026-01-04T03:58:00.184949+00:00 end=2026-01-04T03:58:00.185161+00:00 dur=0s events=0 triggers=0
    focus: loginwindow [0] - Login (wid=4294967295)
  - id=9 state=Active start=2026-01-04T03:58:00.185161+00:00 end=2026-01-04T03:58:30.600844+00:00 dur=30s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=10 state=Active start=2026-01-04T03:58:30.600844+00:00 end=2026-01-04T03:58:48.133149+00:00 dur=17s events=0 triggers=0
    focus: iTerm2 [2159] - jerry@Jerrys-MacBook-Pro:~ (wid=22626)
  - id=11 state=Active start=2026-01-04T03:58:48.133149+00:00 end=2026-01-04T03:59:02.138409+00:00 dur=14s events=0 triggers=0
    focus: Google Chrome [72295] - Noitool - Google Chrome - Jerry (Person 1) (wid=15233)
  - id=12 state=Active start=2026-01-04T03:59:02.138412+00:00 end=2026-01-04T03:59:16.202089+00:00 dur=14s events=0 triggers=0
    focus: Google Chrome [72295] - Editing efJerryYang/README.md at main · efJerryYang/efJerryYang - Google Chrome - Jerry (Person 1) (wid=15233)
  - id=13 state=Active start=2026-01-04T03:59:16.202093+00:00 end=2026-01-04T03:59:20.217939+00:00 dur=4s events=0 triggers=0
    focus: Google Chrome [72295] - github-readme-stats - Google Search - Google Chrome - Jerry (Person 1) (wid=15233)
  - id=14 state=Active start=2026-01-04T03:59:20.217943+00:00 end=2026-01-04T03:59:24.254963+00:00 dur=4s events=0 triggers=0
    focus: Google Chrome [72295] - anuraghazra/github-readme-stats: :zap: Dynamically generated stats for your github readmes - Google Chrome - Jerry (Person 1) (wid=15233)
  - id=15 state=Active start=2026-01-04T03:59:24.254969+00:00 end=2026-01-04T03:59:40.289177+00:00 dur=16s events=0 triggers=0
    focus: Google Chrome [72295] - 503: SERVICE_UNAVAILABLE - Google Chrome - Jerry (Person 1) (wid=15233)
  ...
  - id=100 state=Active start=2026-01-04T04:13:35.716584+00:00 end=2026-01-04T04:13:41.757519+00:00 dur=6s events=0 triggers=0
    focus: iTerm2 [2159] - jerry@Jerrys-MacBook-Pro:~ (wid=22626)
  - id=101 state=Active start=2026-01-04T04:13:41.757524+00:00 end=2026-01-04T04:14:02.535290+00:00 dur=20s events=0 triggers=0
    focus: iTerm2 [2159] - kronictl monitor (wid=22626)
  - id=102 state=Active start=2026-01-04T04:14:02.535290+00:00 end=2026-01-04T04:14:06.172144+00:00 dur=3s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=23269)
  - id=103 state=Active start=2026-01-04T04:14:06.172144+00:00 end=2026-01-04T04:14:08.185614+00:00 dur=2s events=0 triggers=0
    focus: Code [46988] - README.md — kronical (wid=30507)
  - id=104 state=Active start=2026-01-04T04:14:08.185614+00:00 end=2026-01-04T04:14:16.231559+00:00 dur=8s events=0 triggers=0
    focus: Code [46988] - image.png — kronical (wid=23269)
  - id=105 state=Active start=2026-01-04T04:14:16.231563+00:00 end=2026-01-04T04:14:37.782609+00:00 dur=21s events=0 triggers=0
    focus: Code [46988] - demo-monitor.png — kronical (wid=23269)
```

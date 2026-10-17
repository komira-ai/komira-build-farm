# Spike evidence (raw output; throwaway, never merged)

Pinned Envoy 1.39.3 (the CI pin, sha256 f61653fb...c200), box: 28-thread i7-14700K, 61 GB, ext4 on NVMe, load average 13 to 22 from other sessions during every run. Timestamps are seconds since the scenario started.

## spike-envoy silent

```
[t=  0.000s] scenario: == silent ==
[t=  0.073s] A-defaults: envoy started (admin 31000)
[t=  0.084s] B-timeout0-hcm-idle5s: envoy started (admin 31003)
[t=  0.084s] C-box-routeidle0-hcm-idle5s: envoy started (admin 31006)
[t=  0.084s] D-progress-2s-hcm-idle5s: envoy started (admin 31009)
[t=  0.085s] F-max_stream_duration-6s-control: envoy started (admin 31012)
[t= 15.112s] B-timeout0-hcm-idle5s: stream ENDED at 5.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 16.108s] F-max_stream_duration-6s-control: stream ENDED at 6.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 24.112s] D-progress-2s-hcm-idle5s: stream still OPEN after 14s (frames after first: 6); stub cancelled=0
[t= 24.112s] D-progress-2s-hcm-idle5s: then the stub ended it: clean end (OK trailers)
[t= 25.112s] A-defaults: stream ENDED at 15.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 35.109s] C-box-routeidle0-hcm-idle5s: stream still OPEN after 25s (frames after first: 0); stub cancelled=0
[t= 35.110s] C-box-routeidle0-hcm-idle5s: then the stub ended it: clean end (OK trailers)
```

## spike-envoy healthflip

```
[t=  0.000s] scenario: == healthflip ==
[t=  0.000s] healthflip: envoy started (admin 31000)
[t=  0.047s] flags: [(31002, []), (31003, [])]
[t=  0.047s] raw: {port:31002, health_status:{"eds_health_status":"HEALTHY"}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  0.047s] raw-healthy: {port:31002, health_status:{"eds_health_status":"HEALTHY"}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  0.048s] stream: silent stream is on stub a
[t=  3.049s] flip: stub a now answers NOT_SERVING to the health check
[t=  4.036s] flags: [(31002, ["failed_active_health_check"]), (31003, [])] (detected 0.99s after the flip)
[t=  4.036s] raw-failed: {port:31002, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t= 29.039s] stream: 25s after the check failed: OPEN open frames=0; stub a open=1 cancelled=0
[t= 38.061s] load: 1603 unary calls, 0 errors []
[t= 38.061s] load: calls answered by the failed host a: 20 after the flip, last at +0.98s (detected at +0.99s); by the other: 1321
[t= 38.061s] load: after recovery, calls to a again: 119
[t= 38.061s] stream: stub ended the stream: ended clean end (OK trailers); cancelled=0
```

## spike-envoy breakers

```
[t=  0.000s] scenario: == breakers ==
[t=  0.000s] breakers-default: envoy started (admin 31000)
[t=  0.089s] breakers-default: 1100 silent streams requested: 1024 open, errors {"Unavailable: upstream connect error or disconnect/reset before headers. reset reason: overflow": 76}
[t=  0.090s] breakers-default: stats cluster.kbf_reapi.circuit_breakers.default.cx_open: 0; cluster.kbf_reapi.circuit_breakers.default.rq_open: 1; cluster.kbf_reapi.upstream_cx_active: 4; cluster.kbf_reapi.upstream_cx_overflow: 0; cluster.kbf_reapi.upstream_rq_503: 76; cluster.kbf_reapi.upstream_rq_active: 1024; cluster.kbf_reap
[t=  0.090s] breakers-default: clusters kbf_reapi::default_priority::max_connections::1024; kbf_reapi::default_priority::max_requests::1024; kbf_reapi::high_priority::max_connections::1024; kbf_reapi::high_priority::max_requests::1024
[t=  0.095s] breakers-box-5000: envoy started (admin 31004)
[t=  0.194s] breakers-box-5000: 1100 silent streams requested: 1100 open, errors {}
[t=  0.194s] breakers-box-5000: stats cluster.kbf_reapi.circuit_breakers.default.cx_open: 0; cluster.kbf_reapi.circuit_breakers.default.rq_open: 0; cluster.kbf_reapi.upstream_cx_active: 4; cluster.kbf_reapi.upstream_cx_overflow: 0; cluster.kbf_reapi.upstream_rq_active: 1100; cluster.kbf_reapi.upstream_rq_pending_overflow: 0; 
[t=  0.194s] breakers-box-5000: clusters kbf_reapi::default_priority::max_connections::5000; kbf_reapi::default_priority::max_requests::5000; kbf_reapi::high_priority::max_connections::1024; kbf_reapi::high_priority::max_requests::1024
```

## spike-envoy tcp

```
[t=  0.000s] scenario: == tcp ==
[t=  0.000s] tcp: envoy started (admin 31000)
[t=  3.055s] tcp: the TCP backend stops accepting (connect checks fail) at +3.0s
[t= 20.055s] tcp: session keep: None still open after 20s
[t= 20.055s] tcp: session close: Some(4.980607544) read -> Ok(0)
[t= 20.055s] tcp: session idle5: Some(5.000780825) read -> Ok(0)
[t= 20.055s] tcp: session defaultidle: None still open after 20s
[t= 20.055s] tcp: hosts: {port:31007, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
```

## spike-envoy hotrestart

```
[t=  0.000s] scenario: == hotrestart ==
[t=  0.000s] hr-epoch0: envoy started (admin 31000)
[t=  4.055s] hr-epoch1: envoy started (admin 31000)
[t=  4.055s] hotrestart: epoch 1 started at t=4.05s (parent shutdown 12s, drain 3s)
[t=  6.060s] admin: +2s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t=  8.063s] admin: +4s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 10.067s] admin: +6s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 12.071s] admin: +8s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 14.075s] admin: +10s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 16.079s] admin: +12s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 18.083s] admin: +14s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit Some((16.094339683, "exit status: 0")); epoch1 exit None
[t= 20.086s] admin: +16s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit Some((16.094339683, "exit status: 0")); epoch1 exit None
[t= 28.089s] hotrestart: held silent stream: ended at t=16.09s = restart +12.04s (Some(16.037048987)s after open): error Unknown: h2 protocol error: error reading a body from connection, frames=0
[t= 28.089s] hotrestart: quiet tcp_proxy session: read returned at t=16.09s = restart +12.04s: Ok(0)
[t= 28.089s] hotrestart: epoch0 exit Some((16.094339683, "exit status: 0")) (restart at t=4.05); epoch1 exit None
[t= 28.089s] hotrestart: old channel: 120 unary calls, 1 errors [("+1.2", "Cancelled: operation was canceled")]
[t= 28.089s] hotrestart: fresh channel per call: 120 unary calls, 0 errors []
```

## spike-envoy epochs

```
[t=  0.000s] scenario: == epochs ==
[t=  0.000s] epochs-0: envoy started (admin 31000)
[t=  2.055s] epochs: after epoch 0 start (+2.1s): process alive [(0, true)]
[t=  2.057s] epochs:   stream opened under epoch 0 (age 0.0s): OPEN open
[t=  2.057s] epochs-1: envoy started (admin 31000)
[t=  4.060s] epochs: after epoch 1 start (+4.1s): process alive [(0, true), (1, true)]
[t=  4.062s] epochs:   stream opened under epoch 0 (age 2.0s): OPEN open
[t=  4.064s] epochs:   stream opened under epoch 1 (age 0.0s): OPEN open
[t=  4.065s] epochs-2: envoy started (admin 31000)
[t=  6.068s] epochs: after epoch 2 start (+6.1s): process alive [(0, false), (1, true), (2, true)]
[t=  6.068s] epochs:   stream opened under epoch 0 (age 4.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  6.070s] epochs:   stream opened under epoch 1 (age 2.0s): OPEN open
[t=  6.072s] epochs:   stream opened under epoch 2 (age 0.0s): OPEN open
[t=  6.073s] epochs-3: envoy started (admin 31000)
[t=  8.075s] epochs: after epoch 3 start (+8.1s): process alive [(0, false), (1, false), (2, true), (3, true)]
[t=  8.076s] epochs:   stream opened under epoch 0 (age 6.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  8.076s] epochs:   stream opened under epoch 1 (age 4.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  8.078s] epochs:   stream opened under epoch 2 (age 2.0s): OPEN open
[t=  8.080s] epochs:   stream opened under epoch 3 (age 0.0s): OPEN open
[t= 53.081s] epochs: epoch 0 exit: Some((4.10596798, "exit status: 0"))
[t= 53.081s] epochs: epoch 1 exit: Some((6.112601612, "exit status: 0"))
[t= 53.081s] epochs: epoch 2 exit: Some((46.123185012, "exit status: 0"))
[t= 53.081s] epochs: epoch 3 exit: None
[t= 53.081s] epochs: final: stream of epoch 0 (age 51.0s): ENDED clean end (OK trailers)
[t= 53.081s] epochs: final: stream of epoch 1 (age 49.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t= 53.081s] epochs: final: stream of epoch 2 (age 47.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t= 53.083s] epochs: final: stream of epoch 3 (age 45.0s): OPEN open
```

## spike-envoy newhost

```
[t=  0.000s] scenario: == newhost ==
[t=  0.003s] newhost-eds: envoy started (admin 31000)
[t=  4.186s] eds: host a flags: []
[t=  4.187s] eds: adding b (serving) to the file now
[t=  4.191s] eds: +0.004s host 31003: text health_flags="/failed_active_hc"  json port:31003, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
[t=  4.891s] eds: +0.705s host 31003: text health_flags="/failed_active_hc"  json port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  4.895s] eds: +0.708s host 31003: text health_flags="healthy"  json port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  4.895s] eds: b checks answered: 1
[t=  4.895s] newhost-static: envoy started (admin 31005)
[t=  4.947s] static: first flags with b not running: [(31007, []), (31008, ["failed_active_health_check"])]
[t=  7.948s] static: 3s later: [(31007, []), (31008, ["failed_active_health_check"])]
[t=  7.949s] static: starting b now
[t=  7.949s] static: +0.000s host 31008: text health_flags="/failed_active_hc"  json port:31008, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
[t=  8.932s] static: +0.983s host 31008: text health_flags="healthy"  json port:31008, health_status:{"eds_health_status":"HEALTHY"}}
```

## spike-switch meta

```
meta size=100000 cmds: built in 14.1s, applied index 100017, dir 179 MiB; second open while held: raft dir <work>/upd1 in 0.03ms; drop of the writer 44.75ms
meta size=100000: open_dir (lock + replay of 100017 entries + elect) n=9 p50=2197.2ms p99=3201.6ms max=3201.6ms
meta size=100000: replay rate ~45521 entries/s
meta size=100000: open without the lock over a copy 2524.4ms (applied 100027)
meta size=400000 cmds: built in 57.0s, applied index 400059, dir 726 MiB; second open while held: raft dir <work>/upd1 in 0.08ms; drop of the writer 254.64ms
meta size=400000: open_dir (lock + replay of 400059 entries + elect) n=9 p50=9996.1ms p99=10809.7ms max=10809.7ms
meta size=400000: replay rate ~40021 entries/s
meta size=400000: open without the lock over a copy 9737.2ms (applied 400069)
```

## spike-switch ticks

```
meta[ticks] size=100000 cmds: built in 5.3s, applied index 100008, dir 3 MiB; second open while held: raft dir <work>/upd1 in 0.04ms; drop of the writer 1.17ms
meta[ticks] size=100000: open_dir (lock + replay of 100008 entries + elect) n=7 p50=44.2ms p99=50.4ms max=50.4ms
meta[ticks] size=100000: replay rate ~2263894 entries/s
meta[ticks] size=100000: open without the lock over a copy 78.8ms (applied 100016)
meta[ticks] size=1000000 cmds: built in 55.0s, applied index 1000057, dir 33 MiB; second open while held: raft dir <work>/upd1 in 0.09ms; drop of the writer 20.54ms
meta[ticks] size=1000000: open_dir (lock + replay of 1000057 entries + elect) n=7 p50=332.1ms p99=387.4ms max=387.4ms
meta[ticks] size=1000000: replay rate ~3011755 entries/s
meta[ticks] size=1000000: open without the lock over a copy 339.9ms (applied 1000065)
```

## spike-switch state

```
state: built 300 done + 8000 unfinished (4 leased) in 157.9s; file 2 MiB (+wal 3 MiB); first open of a new file 1704.5ms
state: Store::demote, idle  n=6 p50=0.2ms p99=0.2ms max=0.2ms
state: Store::demote, under submit load n=5 p50=4.6ms p99=7.4ms max=7.4ms
state: successor SqliteStore::open after the demote n=11 p50=6.4ms p99=8.2ms max=8.2ms
state: scheduler recovery (Farm::with_state: epoch + load + recover) n=11 p50=13.2ms p99=14.7ms max=14.7ms
state: lease_epoch across the repeated handovers (Keep): [117457005402739672]
```

## Read from code, not run (each says where)

- `RaftMetaLog::query` reads only `Shared::state` (an `Arc<RwLock<MetaState>>`); `closed` guards `commit_batch` only; the directory lock is a separate `Option<File>` field. So a demote that stops the tick task, sets `closed` and drops the lock file leaves `query`, `leadership` and `applied_index` working. There is no `demote` on arch-v2, so this is by construction, not a run.
- `FsStore::open` empties `tmp/` and each open numbers its temporary files from 0 (`Inner::next`). A second store opened while the first has a PUT in flight would delete that PUT's temporary file (the link then fails) or collide on `tmp/0`. The split lock as designed is NOT safe for `file://` without a per-open `tmp/` subdirectory and a sweep that skips live owners.
- Daemon redial (`kbf-daemon/src/connect.rs`): wait after a welcomed session = `--reconnect-ms` (1000) jittered down by up to half; after the n-th failed round `--reconnect-ms * 2^(n-1)`, capped at 30 s, jittered the same way. A 100000-trial model of that schedule gives the time of the first attempt after the endpoint has been unreachable for D seconds: D=2s p99 3.8s; D=5s p99 12.3s; D=10s p99 24.5s; D=20s p99 47.1s with 4.3% past the 40 s fence; D=31s 89% past it.

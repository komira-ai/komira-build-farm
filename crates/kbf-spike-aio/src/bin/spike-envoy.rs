//! THROWAWAY SPIKE: the Envoy questions of docs/design/aio-handover.md section 12.
//!
//! `spike-envoy <scenario> --envoy <binary> --dir <scratch dir>`; scenarios: `silent`,
//! `healthflip`, `breakers`, `hotrestart`, `epochs`, `tcp`. Prints evidence lines, asserts
//! nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::grpc::health::v1::{HealthCheckRequest, HealthListRequest};
use kbf_spike_aio::envoy::{Cfg, Envoy, Flags, host_flags, host_raw, http_get, write_eds};
use kbf_spike_aio::stub::{Silent, Stub};
use kbf_spike_aio::{ev, now};
use tokio::io::AsyncReadExt;
use tokio::time::{Instant, sleep, timeout};
use tonic::Streaming;
use tonic::transport::{Channel, Endpoint};

static PORT: AtomicU16 = AtomicU16::new(31000);

fn ports(n: u16) -> u16 {
    PORT.fetch_add(n, Ordering::SeqCst)
}

struct Env {
    envoy: PathBuf,
    dir: PathBuf,
}

impl Env {
    async fn start(&self, name: &str, cfg: &Cfg, flags: &Flags) -> Envoy {
        let conf = self.dir.join(format!("{name}.json"));
        cfg.write(&conf);
        let log = self.dir.join(format!("{name}.log"));
        let e = Envoy::start(&self.envoy, &conf, &log, cfg.admin, flags).await;
        ev!(name, "envoy started (admin {})", cfg.admin);
        if !e.wait_ready(Duration::from_secs(10)).await {
            ev!(name, "admin /ready not 200 after 10s");
        }
        e
    }
}

async fn channel(port: u16) -> Channel {
    Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
        .expect("uri")
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("connect")
}

/// Which stub answers a `List` through `port`, on a fresh call over `ch`.
async fn who(ch: &Channel) -> Result<String, String> {
    let r = timeout(
        Duration::from_secs(3),
        HealthClient::new(ch.clone()).list(HealthListRequest {}),
    )
    .await
    .map_err(|_| "client timeout 3s".to_owned())?
    .map_err(|s| format!("{:?}: {}", s.code(), s.message()))?;
    Ok(r.into_inner().statuses.into_keys().next().unwrap_or_default())
}

/// Waits until a `List` through `ch` succeeds.
async fn wait_serving(ch: &Channel, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if who(ch).await.is_ok() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn open_watch(ch: &Channel) -> Result<Streaming<kbf_proto::grpc::health::v1::HealthCheckResponse>, String> {
    let mut s = HealthClient::new(ch.clone())
        .watch(HealthCheckRequest::default())
        .await
        .map_err(|s| format!("{:?}: {}", s.code(), s.message()))?
        .into_inner();
    match timeout(Duration::from_secs(10), s.message()).await {
        Ok(Ok(Some(_))) => Ok(s),
        Ok(Ok(None)) => Err("ended before the first frame".to_owned()),
        Ok(Err(e)) => Err(format!("{:?}: {}", e.code(), e.message())),
        Err(_) => Err("no first frame in 10s".to_owned()),
    }
}

/// How a held stream ended: (seconds after `from`, description), or `None` if still open.
async fn hold(
    s: &mut Streaming<kbf_proto::grpc::health::v1::HealthCheckResponse>,
    from: Instant,
    limit: Duration,
) -> (Option<f64>, String, u64) {
    let end = from + limit;
    let mut frames = 0;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match timeout(left, s.message()).await {
            Err(_) => return (None, "open".to_owned(), frames),
            Ok(Ok(Some(_))) => frames += 1,
            Ok(Ok(None)) => {
                return (Some(from.elapsed().as_secs_f64()), "clean end (OK trailers)".to_owned(), frames);
            }
            Ok(Err(e)) => {
                return (
                    Some(from.elapsed().as_secs_f64()),
                    format!("error {:?}: {}", e.code(), e.message()),
                    frames,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// E1: silent streams, timeouts

async fn silent_case(
    env: &Env,
    label: &'static str,
    progress_ms: u64,
    hold_s: u64,
    tweak: impl FnOnce(&mut Cfg),
) {
    let p = ports(3);
    let stub = Stub::start("a", p + 2);
    stub.progress_ms.store(progress_ms, Ordering::SeqCst);
    let mut cfg = Cfg::new(p, p + 1, vec![p + 2]);
    tweak(&mut cfg);
    let mut e = env.start(label, &cfg, &Flags::default()).await;
    e.wait_ready(Duration::from_secs(10)).await;
    let ch = channel(p + 1).await;
    if !wait_serving(&ch, Duration::from_secs(10)).await {
        ev!(label, "never became healthy");
        return;
    }
    let t0 = Instant::now();
    let mut s = match open_watch(&ch).await {
        Ok(s) => s,
        Err(e) => return ev!(label, "open failed: {e}"),
    };
    let (at, how, frames) = hold(&mut s, t0, Duration::from_secs(hold_s)).await;
    match at {
        Some(t) => ev!(label, "stream ENDED at {t:.2}s after open: {how} (frames after first: {frames})"),
        None => {
            ev!(label, "stream still OPEN after {hold_s}s (frames after first: {frames}); stub cancelled={}", stub.cancelled.load(Ordering::SeqCst));
            stub.end_watches();
            let (_, how, _) = hold(&mut s, Instant::now(), Duration::from_secs(3)).await;
            ev!(label, "then the stub ended it: {how}");
        }
    }
    e.kill();
}

async fn silent(env: &Env) {
    let a = silent_case(env, "A-defaults", 0, 22, |c| {
        c.route_timeout = None;
        c.route_idle = None;
    });
    let b = silent_case(env, "B-timeout0-hcm-idle5s", 0, 12, |c| {
        c.route_idle = None;
        c.hcm_idle = Some("5s");
    });
    let c = silent_case(env, "C-box-routeidle0-hcm-idle5s", 0, 25, |c| {
        c.hcm_idle = Some("5s");
    });
    let d = silent_case(env, "D-progress-2s-hcm-idle5s", 2000, 14, |c| {
        c.route_idle = None;
        c.hcm_idle = Some("5s");
    });
    let f = silent_case(env, "F-max_stream_duration-6s-control", 0, 12, |c| {
        c.max_stream_duration = Some("6s");
    });
    tokio::join!(a, b, c, d, f);
}

// ---------------------------------------------------------------------------------------
// E2/E3: health flip, no 503 window, /clusters flags

async fn healthflip(env: &Env) {
    let p = ports(4);
    let (a, b) = (Stub::start("a", p + 2), Stub::start("b", p + 3));
    let cfg = Cfg::new(p, p + 1, vec![p + 2, p + 3]);
    let mut e = env.start("healthflip", &cfg, &Flags::default()).await;
    // The admin interface answers early; poll the first host flags at 20 ms to catch
    // `pending_active_hc`.
    let mut last = String::new();
    let mut first_json = true;
    let t0 = Instant::now();
    loop {
        let flags = host_flags(p, "kbf_reapi").await;
        let text = format!("{flags:?}");
        if text != last && !flags.is_empty() {
            ev!("flags", "{text}");
            if first_json {
                ev!("raw", "{}", host_raw(p, "kbf_reapi").await);
                first_json = false;
            }
            last = text;
        }
        if !flags.is_empty() && flags.iter().all(|(_, f)| f.is_empty()) {
            break;
        }
        if t0.elapsed() > Duration::from_secs(15) {
            ev!("flags", "hosts not healthy after 15s");
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    ev!("raw-healthy", "{}", host_raw(p, "kbf_reapi").await);
    let ch = channel(p + 1).await;
    wait_serving(&ch, Duration::from_secs(5)).await;

    // The open silent stream, on a channel of its own.
    let sch = channel(p + 1).await;
    let mut s = open_watch(&sch).await.expect("watch");
    let (x, y) = if a.open.load(Ordering::SeqCst) == 1 { (&a, &b) } else { (&b, &a) };
    ev!("stream", "silent stream is on stub {}", x.id);

    // Load: a new unary call every 20 ms for the whole window.
    let log: Arc<Mutex<Vec<(f64, Result<String, String>)>>> = Arc::default();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let load = {
        let (log, running, ch) = (Arc::clone(&log), Arc::clone(&running), ch.clone());
        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let (log, ch) = (Arc::clone(&log), ch.clone());
                tokio::spawn(async move {
                    let at = now();
                    let r = who(&ch).await;
                    log.lock().expect("lock").push((at, r));
                });
                sleep(Duration::from_millis(20)).await;
            }
        })
    };
    sleep(Duration::from_secs(3)).await;

    let t_flip = now();
    x.serving.store(false, Ordering::SeqCst);
    ev!("flip", "stub {} now answers NOT_SERVING to the health check", x.id);
    let mut t_detect = None;
    for _ in 0..200 {
        let flags = host_flags(p, "kbf_reapi").await;
        let xp = x.port as u64;
        if flags.iter().any(|(port, f)| *port == xp && f.iter().any(|f| f == "failed_active_health_check")) {
            t_detect = Some(now());
            ev!("flags", "{flags:?} (detected {:.2}s after the flip)", now() - t_flip);
            ev!("raw-failed", "{}", host_raw(p, "kbf_reapi").await);
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    sleep(Duration::from_secs(25)).await;
    let (at, how, frames) = hold(&mut s, Instant::now(), Duration::from_millis(1)).await;
    ev!(
        "stream",
        "25s after the check failed: {} {how} frames={frames}; stub {} open={} cancelled={}",
        at.map_or("OPEN", |_| "ENDED"),
        x.id,
        x.open.load(Ordering::SeqCst),
        x.cancelled.load(Ordering::SeqCst)
    );
    // Recover the host: new streams may come back.
    x.serving.store(true, Ordering::SeqCst);
    sleep(Duration::from_secs(5)).await;
    running.store(false, Ordering::SeqCst);
    let _ = load.await;
    sleep(Duration::from_secs(4)).await;
    let rows = log.lock().expect("lock").clone();
    let errors: Vec<_> = rows.iter().filter(|(_, r)| r.is_err()).collect();
    let total = rows.len();
    let x_after: Vec<f64> = rows
        .iter()
        .filter(|(t, r)| *t > t_flip && r.as_deref() == Ok(x.id.as_str()))
        .map(|(t, _)| *t)
        .collect();
    let y_after = rows.iter().filter(|(t, r)| *t > t_flip && r.as_deref() == Ok(y.id.as_str())).count();
    ev!("load", "{total} unary calls, {} errors {:?}", errors.len(), errors.iter().take(3).collect::<Vec<_>>());
    let in_window: Vec<f64> = x_after.iter().copied().filter(|t| *t < t_flip + 25.0).collect();
    ev!(
        "load",
        "calls answered by the failed host {}: {} after the flip, last at +{:.2}s (detected at +{:.2}s); by the other: {y_after}",
        x.id,
        in_window.len(),
        in_window.iter().copied().fold(0.0, f64::max) - t_flip,
        t_detect.map_or(f64::NAN, |d| d - t_flip)
    );
    ev!("load", "after recovery, calls to {} again: {}", x.id, x_after.iter().filter(|t| **t > t_flip + 25.0).count());
    x.end_watches();
    let (at, how, _) = hold(&mut s, Instant::now(), Duration::from_secs(3)).await;
    ev!("stream", "stub ended the stream: {} {how}; cancelled={}", at.map_or("still open", |_| "ended"), x.cancelled.load(Ordering::SeqCst));
    e.kill();
}

// ---------------------------------------------------------------------------------------
// E4: circuit breakers

async fn breakers(env: &Env) {
    for (label, br) in [("default", None), ("box-5000", Some(5000u32))] {
        let p = ports(4);
        let (_a, _b) = (Stub::start("a", p + 2), Stub::start("b", p + 3));
        let mut cfg = Cfg::new(p, p + 1, vec![p + 2, p + 3]);
        cfg.breakers = br;
        let name = format!("breakers-{label}");
        let mut e = env.start(&name, &cfg, &Flags::default()).await;
        let ch0 = channel(p + 1).await;
        wait_serving(&ch0, Duration::from_secs(10)).await;
        let mut chans = Vec::new();
        for _ in 0..11 {
            chans.push(channel(p + 1).await);
        }
        let mut tasks = Vec::new();
        for i in 0..1100 {
            let ch = chans[i % 11].clone();
            tasks.push(tokio::spawn(async move { open_watch(&ch).await }));
        }
        let mut open = Vec::new();
        let mut errs: std::collections::BTreeMap<String, u32> = Default::default();
        for t in tasks {
            match t.await.expect("join") {
                Ok(s) => open.push(s),
                Err(e) => *errs.entry(e).or_default() += 1,
            }
        }
        ev!(&name, "1100 silent streams requested: {} open, errors {errs:?}", open.len());
        let stats = http_get(p, "/stats?filter=cluster.kbf_reapi.(upstream_rq_pending_overflow|upstream_rq_active|upstream_cx_overflow|upstream_rq_503|circuit_breakers.default.(cx_open|rq_open)|upstream_cx_active)$")
            .await
            .map(|(_, b)| b.replace('\n', "; "))
            .unwrap_or_default();
        ev!(&name, "stats {stats}");
        let cb = http_get(p, "/clusters").await.map(|(_, b)| {
            b.lines().filter(|l| l.contains("circuit_breakers") || l.contains("max_requests") || l.contains("max_connections")).collect::<Vec<_>>().join("; ")
        });
        ev!(&name, "clusters {}", cb.unwrap_or_default());
        drop(open);
        e.kill();
    }
}

// ---------------------------------------------------------------------------------------
// E5: hot restart

async fn hotrestart(env: &Env) {
    let p = ports(8);
    let (a, b) = (Stub::start("a", p + 2), Stub::start("b", p + 3));
    let _ = (&a, &b);
    let mut cfg = Cfg::new(p, p + 1, vec![p + 2, p + 3]);
    cfg.tcp = vec![("worker", p + 4, Some("0s"), false)];
    cfg.tcp_backends = vec![p + 5];
    let _silent = Silent::start(p + 5);
    let base = 700 + (p % 100) as u32;
    let f0 = Flags { base_id: Some(base), epoch: 0, parent_shutdown_s: Some(12), drain_s: Some(3) };
    let mut e0 = env.start("hr-epoch0", &cfg, &f0).await;
    e0.wait_ready(Duration::from_secs(10)).await;
    let ch = channel(p + 1).await;
    wait_serving(&ch, Duration::from_secs(10)).await;
    let t_open = Instant::now();
    let mut s = open_watch(&ch).await.expect("watch");
    // A quiet TCP session through tcp_proxy.
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", p + 4)).await.expect("tcp");
    let tcp_task = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        let r = tcp.read(&mut buf).await;
        (now(), format!("{r:?}"))
    });
    // Unary calls every 200 ms on the same (old) channel, and on a fresh channel each time.
    let old: Arc<Mutex<Vec<(f64, Result<String, String>)>>> = Arc::default();
    let fresh: Arc<Mutex<Vec<(f64, Result<String, String>)>>> = Arc::default();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let (old, fresh, running, ch) = (old.clone(), fresh.clone(), running.clone(), ch.clone());
        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let (old, ch) = (old.clone(), ch.clone());
                tokio::spawn(async move {
                    let r = who(&ch).await;
                    old.lock().expect("lock").push((now(), r));
                });
                let fresh = fresh.clone();
                tokio::spawn(async move {
                    let r = match timeout(Duration::from_secs(3), Endpoint::from_shared(format!("http://127.0.0.1:{}", p + 1)).expect("uri").connect()).await {
                        Ok(Ok(c)) => who(&c).await,
                        Ok(Err(e)) => Err(format!("connect: {e}")),
                        Err(_) => Err("connect timeout".to_owned()),
                    };
                    fresh.lock().expect("lock").push((now(), r));
                });
                sleep(Duration::from_millis(200)).await;
            }
        });
    }
    sleep(Duration::from_secs(4)).await;
    let t_restart = now();
    let f1 = Flags { epoch: 1, ..f0.clone() };
    let mut e1 = env.start("hr-epoch1", &cfg, &f1).await;
    ev!("hotrestart", "epoch 1 started at t={t_restart:.2}s (parent shutdown 12s, drain 3s)");
    // Poll admin and the held stream.
    let stream_end = tokio::spawn(async move {
        let (at, how, frames) = hold(&mut s, t_open, Duration::from_secs(40)).await;
        (now(), at, how, frames)
    });
    for i in 0..8 {
        sleep(Duration::from_secs(2)).await;
        let info = http_get(p, "/server_info").await.map(|(_, b)| {
            let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
            format!("state={} epoch={}", v["state"], v["command_line_options"]["restart_epoch"])
        });
        ev!("admin", "+{}s /server_info {:?}; epoch0 exit {:?}; epoch1 exit {:?}", (i + 1) * 2, info, e0.exit(), e1.exit());
    }
    sleep(Duration::from_secs(4)).await;
    running.store(false, Ordering::SeqCst);
    sleep(Duration::from_secs(4)).await;
    let (t_end, at, how, frames) = stream_end.await.expect("join");
    ev!("hotrestart", "held silent stream: ended at t={t_end:.2}s = restart +{:.2}s ({at:?}s after open): {how}, frames={frames}", t_end - t_restart);
    let (t_tcp, r) = tcp_task.await.expect("join");
    ev!("hotrestart", "quiet tcp_proxy session: read returned at t={t_tcp:.2}s = restart +{:.2}s: {r}", t_tcp - t_restart);
    ev!("hotrestart", "epoch0 exit {:?} (restart at t={t_restart:.2}); epoch1 exit {:?}", e0.exit(), e1.exit());
    for (name, rows) in [("old channel", &old), ("fresh channel per call", &fresh)] {
        let rows = rows.lock().expect("lock").clone();
        let errs: Vec<_> = rows.iter().filter(|(_, r)| r.is_err()).map(|(t, r)| (format!("+{:.1}", t - t_restart), r.clone().unwrap_err())).collect();
        ev!("hotrestart", "{name}: {} unary calls, {} errors {:?}", rows.len(), errs.len(), errs.iter().take(6).collect::<Vec<_>>());
    }
    e0.kill();
    e1.kill();
}

// ---------------------------------------------------------------------------------------
// E5b: epochs 0..3 with live parents

async fn epochs(env: &Env) {
    let p = ports(8);
    let (_a, _b) = (Stub::start("a", p + 2), Stub::start("b", p + 3));
    let cfg = Cfg::new(p, p + 1, vec![p + 2, p + 3]);
    let base = 800 + (p % 100) as u32;
    let mk = |epoch| Flags { base_id: Some(base), epoch, parent_shutdown_s: Some(40), drain_s: Some(5) };
    let mut envoys: Vec<Envoy> = Vec::new();
    let mut streams: Vec<(u32, Streaming<_>, Instant)> = Vec::new();
    let t0 = now();
    for epoch in 0..4u32 {
        let e = env.start(&format!("epochs-{epoch}"), &cfg, &mk(epoch)).await;
        e.wait_ready(Duration::from_secs(8)).await;
        envoys.push(e);
        sleep(Duration::from_secs(2)).await;
        // Open a silent stream on the listener (the newest epoch serves it, if it started).
        let ch = channel(p + 1).await;
        wait_serving(&ch, Duration::from_secs(8)).await;
        match open_watch(&ch).await {
            Ok(s) => streams.push((epoch, s, Instant::now())),
            Err(err) => ev!("epochs", "epoch {epoch}: could not open a stream: {err}"),
        }
        let alive: Vec<_> = envoys.iter().enumerate().map(|(i, e)| (i, e.exit().is_none())).collect();
        ev!("epochs", "after epoch {epoch} start (+{:.1}s): process alive {alive:?}", now() - t0);
        // Which streams ended so far?
        for (ep, s, t) in streams.iter_mut() {
            let (at, how, _) = hold(s, Instant::now(), Duration::from_millis(1)).await;
            ev!("epochs", "  stream opened under epoch {ep} (age {:.1}s): {} {how}", t.elapsed().as_secs_f64(), at.map_or("OPEN", |_| "ENDED"));
        }
    }
    sleep(Duration::from_secs(45)).await;
    for (i, e) in envoys.iter().enumerate() {
        ev!("epochs", "epoch {i} exit: {:?}", e.exit());
    }
    for (ep, s, t) in streams.iter_mut() {
        let (at, how, _) = hold(s, Instant::now(), Duration::from_millis(1)).await;
        ev!("epochs", "final: stream of epoch {ep} (age {:.1}s): {} {how}", t.elapsed().as_secs_f64(), at.map_or("OPEN", |_| "ENDED"));
    }
    for e in envoys.iter_mut() {
        e.kill();
    }
}

// ---------------------------------------------------------------------------------------
// tcp_proxy: idle timeout and close_connections_on_host_health_failure

async fn tcp(env: &Env) {
    let p = ports(10);
    let _a = Stub::start("a", p + 2);
    let silent = Silent::start(p + 7);
    let mut cfg = Cfg::new(p, p + 1, vec![p + 2]);
    cfg.tcp_backends = vec![p + 7];
    cfg.tcp = vec![
        ("keep", p + 3, Some("0s"), false),
        ("close", p + 4, Some("0s"), true),
        ("idle5", p + 5, Some("5s"), false),
        ("defaultidle", p + 6, None, false),
    ];
    let mut e = env.start("tcp", &cfg, &Flags::default()).await;
    e.wait_ready(Duration::from_secs(10)).await;
    let ch = channel(p + 1).await;
    wait_serving(&ch, Duration::from_secs(10)).await;
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for (name, port) in [("keep", p + 3), ("close", p + 4), ("idle5", p + 5), ("defaultidle", p + 6)] {
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("tcp");
        tasks.push(tokio::spawn(async move {
            let mut buf = [0u8; 8];
            match timeout(Duration::from_secs(20), c.read(&mut buf)).await {
                Err(_) => (name, None, "still open after 20s".to_owned()),
                Ok(r) => (name, Some(t0.elapsed().as_secs_f64()), format!("read -> {r:?}")),
            }
        }));
    }
    sleep(Duration::from_secs(3)).await;
    silent.stop_listening();
    ev!("tcp", "the TCP backend stops accepting (connect checks fail) at +{:.1}s", t0.elapsed().as_secs_f64());
    for t in tasks {
        let (name, at, how) = t.await.expect("join");
        ev!("tcp", "session {name}: {at:?} {how}");
    }
    ev!("tcp", "hosts: {}", host_raw(p, "keep").await);
    e.kill();
}

// ---------------------------------------------------------------------------------------
// a host added later: the flags it shows from its first moment

/// Polls `/clusters` every 5 ms until `port` shows no flag, printing every change.
async fn watch_host(label: &str, admin: u16, port: u16, limit: Duration) {
    let t0 = Instant::now();
    let mut last = String::from("<unset>");
    while t0.elapsed() < limit {
        // The text and JSON forms of the same moment, back to back.
        let text = http_get(admin, "/clusters").await.map(|(_, b)| {
            b.lines()
                .filter(|l| l.contains(&format!(":{port}::health_flags")))
                .map(|l| l.rsplit("::").next().unwrap_or("").to_owned())
                .collect::<Vec<_>>()
                .join(",")
        });
        let raw = host_raw(admin, "kbf_reapi").await;
        let mine: String = raw.split("} {").find(|h| h.contains(&format!("port:{port}"))).unwrap_or("<host absent>").to_owned();
        let now_text = format!("text health_flags={:?}  json {mine}", text.unwrap_or_default());
        if now_text != last {
            ev!(label, "+{:.3}s host {port}: {now_text}", t0.elapsed().as_secs_f64());
            last = now_text;
        }
        if last.contains("health_flags=\"healthy\"") {
            return;
        }
        sleep(Duration::from_millis(2)).await;
    }
    ev!(label, "host {port} not clean after {limit:?}");
}

async fn newhost(env: &Env) {
    // (1) EDS from a file: a host added to a running Envoy.
    let p = ports(5);
    let a = Stub::start("a", p + 2);
    let b = Stub::start("b", p + 3);
    // The first check of `b` takes 700 ms, so its pending state lasts long enough to read.
    b.check_delay_ms.store(700, Ordering::SeqCst);
    let eds = env.dir.join("newhost-eds.json");
    write_eds(&eds, "kbf_reapi", &[p + 2], 1);
    let mut cfg = Cfg::new(p, p + 1, vec![]);
    cfg.eds = Some(eds.clone());
    let mut e = env.start("newhost-eds", &cfg, &Flags::default()).await;
    e.wait_ready(Duration::from_secs(10)).await;
    sleep(Duration::from_secs(3)).await;
    ev!("eds", "host a flags: {:?}", host_flags(p, "kbf_reapi").await);
    ev!("eds", "adding b (serving) to the file now");
    write_eds(&eds, "kbf_reapi", &[p + 2, p + 3], 2);
    watch_host("eds", p, p + 3, Duration::from_secs(15)).await;
    ev!("eds", "b checks answered: {}", b.checks.load(Ordering::SeqCst));
    e.kill();
    drop(a);
    // (2) static cluster, host listed but not running yet, started later.
    let p = ports(5);
    let a = Stub::start("a", p + 2);
    let cfg = Cfg::new(p, p + 1, vec![p + 2, p + 3]);
    let mut e = env.start("newhost-static", &cfg, &Flags::default()).await;
    e.wait_ready(Duration::from_secs(10)).await;
    ev!("static", "first flags with b not running: {:?}", host_flags(p, "kbf_reapi").await);
    sleep(Duration::from_secs(3)).await;
    ev!("static", "3s later: {:?}", host_flags(p, "kbf_reapi").await);
    ev!("static", "starting b now");
    let b = Stub::start("b", p + 3);
    watch_host("static", p, p + 3, Duration::from_secs(20)).await;
    e.kill();
    drop((a, b));
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| panic!("{flag} <value>"))
    };
    let env = Env { envoy: PathBuf::from(get("--envoy")), dir: PathBuf::from(get("--dir")) };
    assert!(Path::new(&env.envoy).exists());
    let _ = now();
    for scenario in args.get(1).expect("scenario").split(',') {
        ev!("scenario", "== {scenario} ==");
        match scenario {
            "silent" => silent(&env).await,
            "healthflip" => healthflip(&env).await,
            "breakers" => breakers(&env).await,
            "hotrestart" => hotrestart(&env).await,
            "epochs" => epochs(&env).await,
            "tcp" => tcp(&env).await,
            "newhost" => newhost(&env).await,
            other => panic!("unknown scenario {other}"),
        }
    }
}

//! The webhook notifier against a fake HTTP receiver on loopback: delivery in order,
//! retry with backoff, the outbox file across a restart, and a caller that never waits.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_alert::{
    Alert, Backoff, Event, OUTBOX_FILE, Outbox, Severity, Transition, UnixMillis, Webhook,
    WebhookConfig, WebhookError,
};
use serde_json::Value;

/// How the fake receiver answers the request with this index (0-based, in arrival
/// order).
#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    /// `302 Found` to this path on the same receiver.
    Redirect(&'static str),
    /// Read the request and never answer.
    Hang,
}

/// One request the receiver read.
#[derive(Debug, Clone)]
struct Request {
    headers: Vec<String>,
    body: Value,
    at: Instant,
}

/// An HTTP/1.1 receiver on loopback that records every request and answers by script.
struct Receiver {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Receiver {
    fn start(script: impl Fn(usize) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}/hook", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                let index = {
                    let mut seen = seen.lock().expect("lock");
                    seen.push(request);
                    seen.len() - 1
                };
                match script(index) {
                    Reply::Status(code) => answer(stream, code, ""),
                    Reply::Redirect(path) => {
                        answer(stream, 302, &format!("location: {path}\r\n"));
                    }
                    Reply::Hang => held.push(stream),
                }
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }

    /// Waits up to 20 s for `n` requests.
    fn wait_for(&self, n: usize) -> Vec<Request> {
        wait_until(|| self.requests().len() >= n);
        self.requests()
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        headers.push(line);
    }
    let length: usize = headers
        .iter()
        .find_map(|h| {
            let (name, value) = h.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    // No body (a followed redirect sends none) is recorded as null.
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).ok()?
    };
    Some(Request {
        headers,
        body,
        at: Instant::now(),
    })
}

fn answer(mut stream: TcpStream, code: u16, extra_headers: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {code} X\r\n{extra_headers}content-length: 0\r\nconnection: close\r\n\r\n"
    );
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A fresh directory under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-alert")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn config(url: &str) -> WebhookConfig {
    WebhookConfig {
        url: url.to_owned(),
        bearer_token: None,
        timeout: Duration::from_secs(5),
        backoff: Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(40),
        },
    }
}

fn event(subject: &str) -> Event {
    Event {
        transition: Transition::Raised,
        alert: Alert::new(
            "disconnected",
            subject,
            Severity::Critical,
            "the daemon is gone",
            "systemctl restart kbf-daemon",
        )
        .expect("valid"),
        first_seen: UnixMillis(1_000),
        last_seen: UnixMillis(2_000),
        resolved_at: None,
    }
}

fn subjects(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| r.body["subject"].as_str().expect("subject").to_owned())
        .collect()
}

fn ids(requests: &[Request]) -> Vec<u64> {
    requests
        .iter()
        .map(|r| r.body["id"].as_u64().expect("id"))
        .collect()
}

fn outbox_on_disk(dir: &Path) -> Outbox {
    Outbox::decode(&std::fs::read(dir.join(OUTBOX_FILE)).expect("outbox file")).expect("decodes")
}

/// Catches: a failed attempt that drops the event (no retry), a retry with no wait
/// (no backoff), a later event sent before an earlier one is delivered, and a body or
/// header the receiver cannot use. The receiver fails the first three attempts: event
/// "a" arrives four times with the same id, then "b" once.
#[test]
fn a_failing_receiver_gets_every_event_in_order_after_backoff() {
    let receiver = Receiver::start(|i| Reply::Status(if i < 3 { 500 } else { 200 }));
    let dir = scratch("retry");
    let mut config = config(&receiver.url);
    config.bearer_token = Some("t0ken".into());
    config.backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
    };
    let webhook = Webhook::start(&dir, config).expect("starts");
    webhook.notify(event("a"));
    webhook.notify(event("b"));
    let requests = receiver.wait_for(5);
    wait_until(|| webhook.status().delivered == 2);

    assert_eq!(subjects(&requests), ["a", "a", "a", "a", "b"]);
    assert_eq!(ids(&requests), [1, 1, 1, 1, 2]);
    // 50 + 100 + 200 ms between the four attempts at "a".
    let spread = requests[3].at - requests[0].at;
    assert!(spread >= Duration::from_millis(350), "{spread:?}");

    let first = &requests[0];
    assert_eq!(first.headers[0], "POST /hook HTTP/1.1");
    let has = |h: &str| first.headers.iter().any(|l| l.eq_ignore_ascii_case(h));
    assert!(has("authorization: Bearer t0ken"), "{:?}", first.headers);
    assert!(has("content-type: application/json"), "{:?}", first.headers);
    assert_eq!(first.body["transition"], "raised");
    assert_eq!(first.body["fix"], "systemctl restart kbf-daemon");
    assert_eq!(first.body["first_seen"], 1_000);
    assert_eq!(first.body["resolved_at"], Value::Null);

    let status = webhook.status();
    assert_eq!(status.pending, 0);
    assert_eq!(status.failures_in_a_row, 0);
    assert_eq!(
        status.last_error.as_deref(),
        Some("webhook answered 500 Internal Server Error")
    );
    webhook.stop();
    assert!(outbox_on_disk(&dir).is_empty());
}

/// Catches: an outbox kept only in memory (lost on restart), a load that ignores the
/// file, and ids reused after a restart. The first notifier's receiver is down; the
/// second notifier, on the same state directory, delivers both events, then a new one
/// with the next id.
#[test]
fn undelivered_events_survive_a_restart() {
    let dir = scratch("restart");
    // A port that refuses connections: bound, then closed.
    let down = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}/hook", listener.local_addr().expect("addr"))
    };
    let first = Webhook::start(&dir, config(&down)).expect("starts");
    first.notify(event("a"));
    first.notify(event("b"));
    wait_until(|| first.status().failures_in_a_row >= 2);
    let status = first.status();
    assert_eq!(status.pending, 2);
    assert!(status.last_error.is_some());
    first.stop();
    let on_disk = outbox_on_disk(&dir);
    assert_eq!(on_disk.len(), 2);
    assert!(on_disk.head().expect("a").attempts >= 2);

    let receiver = Receiver::start(|_| Reply::Status(204));
    let second = Webhook::start(&dir, config(&receiver.url)).expect("restarts");
    let requests = receiver.wait_for(2);
    assert_eq!(subjects(&requests), ["a", "b"]);
    assert_eq!(ids(&requests), [1, 2]);
    second.notify(event("c"));
    let requests = receiver.wait_for(3);
    assert_eq!(ids(&requests), [1, 2, 3]);
    wait_until(|| second.status().delivered == 3);
    second.stop();
    assert!(outbox_on_disk(&dir).is_empty());
}

/// Catches: a notifier that delivers on the caller's thread, so a receiver that hangs
/// stalls whoever raised the alert. 200 events are handed over while the receiver
/// holds the first request unanswered: every call returns at once, and once that
/// attempt times out all 201 events are in the outbox file.
#[test]
fn notify_never_waits_for_the_receiver() {
    let receiver = Receiver::start(|_| Reply::Hang);
    let dir = scratch("hang");
    let mut config = config(&receiver.url);
    config.timeout = Duration::from_secs(1);
    let webhook = Webhook::start(&dir, config).expect("starts");
    webhook.notify(event("first"));
    receiver.wait_for(1);
    let start = Instant::now();
    for n in 0..200 {
        webhook.notify(event(&format!("n{n}")));
    }
    let took = start.elapsed();
    assert!(took < Duration::from_millis(500), "notify waited {took:?}");
    wait_until(|| webhook.status().pending == 201);
    assert_eq!(outbox_on_disk(&dir).len(), 201);
    assert_eq!(webhook.status().delivered, 0);
    drop(webhook);
}

/// Catches: a notifier that keeps going without a working state directory (an alert
/// would be lost on restart without a word), and an outbox write failure while
/// running that stops delivery instead of being reported.
#[test]
fn the_state_directory_must_take_the_outbox() {
    let receiver = Receiver::start(|_| Reply::Status(200));
    let missing = scratch("missing").join("absent");
    let err = Webhook::start(&missing, config(&receiver.url)).expect_err("no directory");
    assert!(matches!(err, WebhookError::Io { .. }), "{err}");
    assert!(err.to_string().contains(OUTBOX_FILE), "{err}");

    let dir = scratch("vanishes");
    let webhook = Webhook::start(&dir, config(&receiver.url)).expect("starts");
    std::fs::remove_dir_all(&dir).expect("remove the state directory");
    webhook.notify(event("a"));
    receiver.wait_for(1);
    wait_until(|| webhook.status().delivered == 1);
    let status = webhook.status();
    let error = status.last_error.expect("the write failure is reported");
    assert!(error.contains(OUTBOX_FILE), "{error}");
    webhook.stop();
}

/// Catches: a corrupt outbox read as empty (undelivered alerts dropped), a path that
/// cannot take the file (a directory there) accepted, and a URL the client cannot use
/// accepted at start instead of failing every attempt.
#[test]
fn start_refuses_a_bad_outbox_or_url() {
    let receiver = Receiver::start(|_| Reply::Status(200));
    let dir = scratch("corrupt");
    std::fs::write(dir.join(OUTBOX_FILE), b"{\"version\":1").expect("write");
    let err = Webhook::start(&dir, config(&receiver.url)).expect_err("corrupt");
    assert!(matches!(err, WebhookError::Outbox { .. }), "{err}");
    assert!(err.to_string().contains("does not parse"), "{err}");

    let dir = scratch("unreadable");
    std::fs::create_dir(dir.join(OUTBOX_FILE)).expect("a directory where the file goes");
    let err = Webhook::start(&dir, config(&receiver.url)).expect_err("unreadable");
    assert!(matches!(err, WebhookError::Io { .. }), "{err}");

    let dir = scratch("url");
    let err = Webhook::start(&dir, config("not a url")).expect_err("parse");
    assert!(
        err.to_string().starts_with("webhook url \"not a url\""),
        "{err}"
    );
    let err = Webhook::start(&dir, config("https://example.invalid/hook")).expect_err("tls");
    assert!(
        err.to_string().contains("scheme https is not supported"),
        "{err}"
    );
}

/// Catches: a client that follows a redirect. reqwest's default policy turns a 302
/// to a POST into a GET with no body, and its 2xx would count as delivered: the event
/// would leave the outbox and no receiver would ever get it. The receiver answers the
/// first POST with a 302 to another path, then 200: the 302 must be a failed attempt,
/// and the event must arrive a second time by POST to the configured path, with its
/// body.
#[test]
fn a_redirect_is_a_failed_attempt_not_a_delivery() {
    let receiver = Receiver::start(|i| {
        if i == 0 {
            Reply::Redirect("/elsewhere")
        } else {
            Reply::Status(200)
        }
    });
    let dir = scratch("redirect");
    let webhook = Webhook::start(&dir, config(&receiver.url)).expect("starts");
    webhook.notify(event("a"));
    wait_until(|| webhook.status().delivered == 1);
    let requests = receiver.requests();
    let lines: Vec<&str> = requests.iter().map(|r| r.headers[0].as_str()).collect();
    assert_eq!(lines, ["POST /hook HTTP/1.1", "POST /hook HTTP/1.1"]);
    assert_eq!(ids(&requests), [1, 1]);
    let status = webhook.status();
    assert_eq!(status.pending, 0);
    assert_eq!(
        status.last_error.as_deref(),
        Some("webhook answered 302 Found")
    );
    webhook.stop();
}

/// Catches: an attempt made whenever an event arrives, ignoring the backoff. In an
/// outage alerts keep coming, so that would hammer a failing receiver at the rate
/// alerts are raised. The receiver fails the first attempt; the backoff is 1 s; 10
/// events are handed over during it. The second attempt comes no earlier than the
/// backoff allows, and then all 11 are delivered in order.
#[test]
fn events_during_a_backoff_wait_for_it() {
    let receiver = Receiver::start(|i| Reply::Status(if i == 0 { 503 } else { 200 }));
    let dir = scratch("backoff-events");
    let mut config = config(&receiver.url);
    config.backoff = Backoff {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(1),
    };
    let webhook = Webhook::start(&dir, config).expect("starts");
    webhook.notify(event("first"));
    receiver.wait_for(1);
    wait_until(|| webhook.status().failures_in_a_row == 1);
    for n in 0..10 {
        webhook.notify(event(&format!("n{n}")));
        std::thread::sleep(Duration::from_millis(20));
    }
    let requests = receiver.wait_for(12);
    wait_until(|| webhook.status().delivered == 11);
    let gap = requests[1].at - requests[0].at;
    assert!(gap >= Duration::from_millis(950), "retried after {gap:?}");
    assert_eq!(
        ids(&requests),
        [1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        "in order, one attempt each after the retry"
    );
    webhook.stop();
}

/// Catches: an outbox file that exists but cannot be read taken as an empty outbox.
/// `start` would then write an empty outbox over it and the undelivered alert would be
/// gone without a word. The file holds one event and has mode 000: `start` must fail
/// with an I/O error and leave the file as it was. Skipped where mode 000 does not
/// stop a read (running as root).
#[cfg(unix)]
#[test]
fn start_refuses_an_outbox_it_cannot_read_and_keeps_it() {
    use std::os::unix::fs::PermissionsExt as _;

    let receiver = Receiver::start(|_| Reply::Status(200));
    let dir = scratch("mode-000");
    let path = dir.join(OUTBOX_FILE);
    let mut outbox = Outbox::new();
    outbox.push(event("a"));
    let bytes = outbox.encode();
    std::fs::write(&path, &bytes).expect("write");
    let mode = |m: u32| {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).expect("chmod");
    };
    mode(0o000);
    if std::fs::read(&path).is_ok() {
        mode(0o644);
        eprintln!("skipped: mode 000 does not stop this process reading the file");
        return;
    }
    let result = Webhook::start(&dir, config(&receiver.url));
    mode(0o644);
    let err = result.expect_err("an unreadable outbox");
    assert!(matches!(err, WebhookError::Io { .. }), "{err}");
    assert!(err.to_string().contains(OUTBOX_FILE), "{err}");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        bytes,
        "the file is untouched"
    );
    assert!(receiver.requests().is_empty());
}

/// Catches: a backoff whose wait cannot be added to the clock accepted at start. A
/// failure whose wait reached it would then panic the thread on the addition, and
/// every later event would be dropped with nothing in the status.
#[test]
fn start_refuses_a_backoff_too_long_for_the_clock() {
    let dir = scratch("backoff-max");
    let mut config = config("http://127.0.0.1:9/hook");
    config.backoff.max = Duration::MAX;
    let err = Webhook::start(&dir, config).expect_err("backoff");
    assert!(matches!(err, WebhookError::Backoff(_)), "{err}");
    assert!(!dir.join(OUTBOX_FILE).exists(), "refused before any write");
}

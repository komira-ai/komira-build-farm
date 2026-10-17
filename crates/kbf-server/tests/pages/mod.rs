//! A server with the operator API for the build-page tests, the records fed to its
//! recorder directly, and builders for the events.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_auth::Policy;
use kbf_front::{Cache, MemoryMetaLog, Recorder, VerifiedBlob};
use kbf_invocation::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Listing, Metadata,
    Outcome, Output, Owner, RecordKey, Usage, WallTime,
};
use kbf_objstore::MemoryStore;
use kbf_sched::RequeueReason;
use kbf_server::{Api, Listeners, StateStore, bind_server_with_state};
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, WorkerId};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Every caller accepted and allowed on every instance, pages and blobs.
pub const OPEN: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#;

/// The pages without the blob route.
pub const PAGES_ONLY: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"allow": {}}}}"#;

/// A server with the operator API in this process, over a cache the test holds.
pub struct Server {
    pub api: SocketAddr,
    pub cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    pub recorder: Recorder,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

/// One answer: its status code, head and body (as JSON, `null` if it is not).
#[derive(Debug)]
pub struct Answer {
    pub code: u16,
    pub head: String,
    pub body: Vec<u8>,
    pub json: Value,
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

impl Server {
    /// A server running `policy`, its records within `memory` bytes.
    pub fn start(policy: Policy, memory: usize) -> Self {
        Self::start_over(policy, memory, None)
    }

    /// [`Server::start`], keeping the scheduler's state in `state` and writing the build
    /// history it names, if any.
    pub fn start_over(policy: Policy, memory: usize, state: Option<StateStore>) -> Self {
        let cache = Arc::new(Cache::memory());
        let listeners = Listeners {
            reapi: loopback(),
            worker: loopback(),
            reapi_tls: None,
            worker_tls: None,
            heartbeat_interval: Duration::from_millis(200),
            hello_wait: Duration::from_secs(5),
            tick: Duration::from_millis(50),
            unservable_wait: kbf_sched::UNSERVABLE_WAIT,
            memory_ladder: kbf_server::MemoryLadder::default(),
            finished_retention: kbf_sched::FINISHED_RETENTION,
            shutdown_timeout: Duration::from_secs(10),
            streams: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: memory,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            stream_limits: kbf_server::StreamLimits::DEFAULT,
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = stopped.await;
        };
        let api = Api {
            listen: loopback(),
            token: None,
            local_cache: None,
        };
        let bound = bind_server_with_state(
            Arc::clone(&cache),
            listeners,
            Some(api),
            policy,
            state,
            shutdown,
        )
        .expect("bind");
        let api = bound.api.expect("an API address");
        let recorder = bound.recorder.clone();
        let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
        Self {
            api,
            cache,
            recorder,
            stop,
            serving,
        }
    }

    /// A server running the policy file `json`, with the default memory bound.
    pub fn with(json: &str) -> Self {
        let policy = Policy::from_json(json).expect("a policy");
        Self::start(policy, kbf_front::DEFAULT_INVOCATION_MEMORY)
    }

    /// Shuts the server down and waits until it has stopped.
    pub async fn stop(self) {
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }

    /// Applies `events` to the records, in order, before it returns.
    pub async fn feed(&self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            self.recorder.emit(event);
        }
        self.recorder.caught_up().await;
    }

    /// Stores `bytes` in the cache; their digest.
    pub async fn store(&self, bytes: &[u8]) -> Digest {
        let blob = VerifiedBlob::hashed(Bytes::copy_from_slice(bytes));
        let digest = blob.digest();
        self.cache.store_blobs(vec![blob]).await.expect("store");
        digest
    }

    /// One `GET` of `path` (raw: sent as given).
    pub async fn get(&self, path: &str) -> Answer {
        let mut stream = TcpStream::connect(self.api).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read");
        let end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head");
        let head = String::from_utf8(response[..end].to_vec()).expect("an ASCII head");
        let body = response[end + 4..].to_vec();
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("a status code");
        let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Answer {
            code,
            head,
            body,
            json,
        }
    }

    /// `GET path`, which must answer `200`; its JSON.
    pub async fn ok(&self, path: &str) -> Value {
        let answer = self.get(path).await;
        assert_eq!(answer.code, 200, "{path}: {} {}", answer.head, answer.json);
        answer.json
    }
}

/// The base time of the tests: 2026-10-11T09:00:00Z.
pub const BASE_SECS: u64 = 1_791_709_200;

/// `micros` microseconds after [`BASE_SECS`].
pub fn at(micros: u64) -> WallTime {
    WallTime::from_unix_micros(BASE_SECS * 1_000_000 + micros)
}

/// A digest that names no stored blob: its hash is `n`'s bytes, its size `n`.
pub fn digest(n: u64) -> Digest {
    let mut hash = [0xa5; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// A digest as the API writes it.
pub fn text(d: &Digest) -> String {
    format!("{}-{}", d.hash_hex(), d.size_bytes)
}

/// A call of invocation `id` by the caller `owner` on `instance` at `when`.
pub fn call(id: &str, owner: &str, instance: &str, when: WallTime, kind: CallKind) -> Event {
    Event::Call(Box::new(Call {
        id: InvocationId::new(id),
        minted: false,
        owner: Owner(owner.to_owned()),
        instance: instance.to_owned(),
        at: when,
        metadata: Metadata {
            tool: Some("buck2".to_owned()),
            tool_version: Some("0.1.0".to_owned()),
            ..Metadata::default()
        },
        kind,
    }))
}

/// A result's listing: `outputs` files named `out/<i>.o`, and the logs given.
pub fn listing(
    result: Digest,
    exit_code: i32,
    outputs: &[Digest],
    logs: (Option<Digest>, Option<Digest>),
) -> Listing {
    Listing {
        result: Some(result),
        exit_code: Some(exit_code),
        outputs: outputs
            .iter()
            .enumerate()
            .map(|(i, d)| Output {
                path: format!("out/{i}.o"),
                digest: *d,
                executable: false,
                directory: false,
            })
            .collect(),
        stdout: logs.0,
        stderr: logs.1,
        usage: None,
    }
}

/// A GetActionResult hit of `action` on `main` by invocation `id`.
pub fn hit(id: &str, when: WallTime, action: Digest, listing: Listing) -> Event {
    let hit = Some(listing);
    call(
        id,
        "",
        "main",
        when,
        CallKind::GetActionResult { action, hit },
    )
}

/// An Execute of `action` on `main` by invocation `id`, answered from the cache.
pub fn cached(id: &str, when: WallTime, action: Digest, listing: Listing) -> Event {
    let answer = ExecuteAnswer::Cached {
        name: format!("operations/cached/{}", text(&action)),
        result: Box::new(listing),
    };
    call(id, "", "main", when, CallKind::Execute { action, answer })
}

/// An Execute of `action` on `instance` by invocation `id` that queued operation `op`.
pub fn queued(id: &str, instance: &str, when: WallTime, action: Digest, op: u64) -> Event {
    let answer = ExecuteAnswer::Queued {
        name: format!("operations/1-{op}"),
        operation: OperationId(op),
    };
    call(id, "", instance, when, CallKind::Execute { action, answer })
}

pub fn lease(n: u64) -> LeaseId {
    LeaseId::new(1, n)
}

/// Operation `op`'s lease `n` granted on `worker`.
pub fn granted(op: u64, n: u64, worker: &str, when: WallTime) -> Event {
    Event::Granted {
        operation: OperationId(op),
        lease: lease(n),
        worker: WorkerId::new(worker),
        at: when,
    }
}

pub fn started(op: u64, n: u64, when: WallTime) -> Event {
    Event::Started {
        operation: OperationId(op),
        lease: lease(n),
        at: when,
    }
}

pub fn requeued(op: u64, n: u64, reason: RequeueReason, when: WallTime) -> Event {
    Event::Requeued {
        operation: OperationId(op),
        lease: lease(n),
        reason,
        at: when,
    }
}

/// Operation `op` ended by lease `n`, accepted, its callers answered `outcome`.
pub fn finished(
    op: u64,
    n: u64,
    outcome: Outcome,
    result: Option<Listing>,
    when: WallTime,
) -> Event {
    Event::Finished(Finished {
        operation: OperationId(op),
        lease: Some((lease(n), LeaseEnd::Accepted)),
        outcome,
        result,
        at: when,
    })
}

/// Usage a result carries.
pub const fn usage(cpu: u64, memory: u64) -> Usage {
    Usage {
        cpu_user_micros: cpu,
        cpu_system_micros: 1,
        peak_memory_bytes: memory,
        wall_micros: 2 * cpu,
    }
}

pub fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

/// Operation `op` ended with no result: by lease `n` and how it ended, or by no
/// lease (a refusal), its callers answered `outcome`.
pub fn ended(
    op: u64,
    lease_end: Option<(u64, LeaseEnd)>,
    outcome: Outcome,
    when: WallTime,
) -> Event {
    Event::Finished(Finished {
        operation: OperationId(op),
        lease: lease_end.map(|(n, end)| (lease(n), end)),
        outcome,
        result: None,
        at: when,
    })
}

/// [`queued`], with `target` as the call's `target_id`.
pub fn queued_target(id: &str, target: &str, when: WallTime, action: Digest, op: u64) -> Event {
    let Event::Call(mut call) = queued(id, "main", when, action, op) else {
        unreachable!("queued is a call")
    };
    call.metadata.target = Some(kbf_invocation::Text::sent(target));
    Event::Call(call)
}

/// 2030-01-01T00:00:00Z: a base for events after any server's start.
pub const LATER_SECS: u64 = 1_893_456_000;

/// `micros` microseconds after [`LATER_SECS`].
pub fn later(micros: u64) -> WallTime {
    WallTime::from_unix_micros(LATER_SECS * 1_000_000 + micros)
}

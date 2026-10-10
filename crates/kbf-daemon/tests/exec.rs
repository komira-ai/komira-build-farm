//! A real server and a real daemon in one process, end to end: a REAPI client uploads
//! an action and calls Execute; the server places it on the daemon over mutual TLS; the
//! daemon fetches the action and its inputs from the server's CAS, runs it with the
//! test-only local runtime, uploads the outputs, and reports the result with its
//! resource usage; the client reads the result and the outputs back. A runtime that
//! fails runs as a driver's memory kill would shows the server reading the daemon's
//! `Result.memory_kill` (failure classes, 6.1).

#![cfg(target_os = "linux")]

mod support;

use std::future::pending;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::{Cas, CasClient, digest_of};
use kbf_daemon::usage::usage_of;
use kbf_daemon::{Daemon, DaemonConfig, LocalRuntime, NodeReport, Runtime, RuntimeError, Work};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::{KeyPrefix, MemoryStore, ObjectKey, ObjectStore, PageSize};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{ActionResult, Digest, ExecuteRequest, ExecuteResponse};
use kbf_server::{Listeners, WorkerTls, bind_server};
use kbf_types::LeaseId;
use prost::Message;
use support::memory::Spec;
use support::{PROMPT, pki, scratch};
use tokio::time::timeout;
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// A server and a daemon on loopback ports, and a REAPI channel to the server.
struct Farm {
    cache: Arc<MemoryCache>,
    reapi: Channel,
    cas: CasClient,
}

impl Farm {
    async fn start(name: &str) -> Self {
        Self::start_with(name, |runtime| runtime).await
    }

    /// A farm whose daemon runs leases through `wrap` of the local runtime.
    async fn start_with<R: Runtime>(
        name: &str,
        wrap: impl FnOnce(LocalRuntime<CasClient>) -> R,
    ) -> Self {
        let pki = pki(name);
        let cache = Arc::new(Cache::memory());
        let listeners = Listeners {
            reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
            worker: SocketAddr::from(([127, 0, 0, 1], 0)),
            reapi_tls: None,
            worker_tls: Some(WorkerTls {
                server: pki.server_tls(),
                deny_list: None,
            }),
            heartbeat_interval: Duration::from_millis(100),
            hello_wait: Duration::from_secs(2),
            tick: Duration::from_millis(50),
            unservable_wait: Duration::from_secs(300),
            finished_retention: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(10),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        };
        let bound = bind_server(Arc::clone(&cache), listeners, pending()).expect("bind");
        let (reapi_addr, worker_addr) = (bound.reapi, bound.worker);
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        let reapi = Endpoint::from_shared(format!("http://{reapi_addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");

        // The daemon reads and writes blobs over its own connection to the front.
        let runtime = Arc::new(wrap(LocalRuntime::new(
            Arc::new(CasClient::new(reapi.clone())),
            scratch(name),
        )));
        let report = NodeReport::new([
            ("arch", "x86_64"),
            ("cpus", "4"),
            ("drivers", "local"),
            ("mem_gib", "8"),
            ("os", "linux"),
        ]);
        let mut config = DaemonConfig::new(
            format!("https://127.0.0.1:{}", worker_addr.port()),
            pki.client,
            "node-1".to_owned(),
        );
        config.reconnect_after = Duration::from_millis(100);
        let daemon = Daemon::new(config, runtime, report).expect("daemon config");
        tokio::spawn(daemon.run(pending()));
        Self {
            cache,
            cas: CasClient::new(reapi.clone()),
            reapi,
        }
    }

    /// Uploads every blob of `spec` through the REAPI CAS; returns the action digest.
    async fn upload(&self, spec: &Spec) -> Digest {
        let mut blobs = Vec::new();
        let action = spec.store_with(&mut |bytes| {
            let digest = digest_of(&bytes);
            blobs.push(bytes);
            digest
        });
        for bytes in blobs {
            self.cas.put(bytes).await.expect("upload");
        }
        action
    }

    /// Executes `action` and returns the ExecuteResponse of the done operation.
    async fn execute(&self, action: &Digest) -> ExecuteResponse {
        let mut ops = ExecutionClient::new(self.reapi.clone())
            .execute(ExecuteRequest {
                action_digest: Some(action.clone()),
                skip_cache_lookup: true,
                ..ExecuteRequest::default()
            })
            .await
            .expect("Execute")
            .into_inner();
        let done: Operation = timeout(PROMPT, async {
            loop {
                let op = ops.message().await.expect("stream").expect("an update");
                if op.done {
                    return op;
                }
            }
        })
        .await
        .expect("the operation finishes in time");
        let Some(operation::Result::Response(any)) = done.result else {
            panic!("no response: {done:?}");
        };
        ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
    }

    async fn text(&self, digest: Option<&Digest>) -> String {
        let bytes = self
            .cas
            .get(digest.expect("digest"))
            .await
            .expect("readable");
        String::from_utf8(bytes).expect("UTF-8")
    }

    /// Every object in the store.
    async fn objects(&self) -> Vec<ObjectKey> {
        let page = self
            .cache
            .objects()
            .list(&KeyPrefix::default(), None, PageSize::MAX)
            .await
            .expect("list");
        page.objects.into_iter().map(|o| o.key).collect()
    }
}

/// Catches a daemon that does not execute what the server starts: the action must
/// really run (its output is computed from an input file), its outputs, stdout and
/// stderr must be readable from the server's CAS, the result must reach the client
/// and the action cache, and it must carry the resource usage the runtime measured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_action_runs_on_the_daemon_and_its_outputs_are_in_the_cas() {
    let farm = Farm::start("exec-run").await;
    let spec = Spec::sh(
        "mkdir -p out; tr a-z A-Z < in.txt > out/greeting; echo ran; echo warned >&2; \
         i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done",
    )
    .outputs(&["out/greeting"])
    .inputs(&[("in.txt", b"hello from the cas\n", false)]);
    let action = farm.upload(&spec).await;

    let response = farm.execute(&action).await;
    assert_eq!(response.status.map(|s| s.code), Some(Code::Ok as i32));
    let result = response.result.expect("a result");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(result.output_files[0].path, "out/greeting");
    assert_eq!(
        farm.text(result.output_files[0].digest.as_ref()).await,
        "HELLO FROM THE CAS\n"
    );
    assert_eq!(farm.text(result.stdout_digest.as_ref()).await, "ran\n");
    assert_eq!(farm.text(result.stderr_digest.as_ref()).await, "warned\n");

    let usage = usage_of(&result).expect("resource usage reported");
    assert!(usage.cpu_user_micros > 0, "{usage:?}");
    assert!(usage.peak_memory_bytes > 0, "{usage:?}");
    assert!(usage.wall_micros >= usage.cpu_user_micros, "{usage:?}");

    // The server names the node and when the action was queued; the daemon's own
    // times stay, in order after it (issue #166).
    let metadata = result.execution_metadata.as_ref().expect("metadata");
    assert_eq!(metadata.worker, "node-1");
    let time = |t: Option<&prost_types::Timestamp>| {
        std::time::SystemTime::try_from(*t.expect("a timestamp")).expect("a time")
    };
    let queued = time(metadata.queued_timestamp.as_ref());
    let worker_start = time(metadata.worker_start_timestamp.as_ref());
    let worker_completed = time(metadata.worker_completed_timestamp.as_ref());
    assert!(
        queued <= worker_start && worker_start <= worker_completed,
        "{metadata:?}"
    );

    let cached = kbf_proto::reapi::action_cache_client::ActionCacheClient::new(farm.reapi.clone())
        .get_action_result(kbf_proto::reapi::GetActionResultRequest {
            action_digest: Some(action),
            ..Default::default()
        })
        .await
        .expect("an action-cache hit")
        .into_inner();
    assert_eq!(cached, result);
}

/// Catches a daemon that hangs, reports success, or stops serving when an input it
/// was started with cannot be read from the CAS (the front checked it was present,
/// then its object was lost): the operation must end with an error and no action-cache
/// entry, and the next action must still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_input_lost_from_the_cas_fails_the_action_cleanly() {
    let farm = Farm::start("exec-lost").await;
    let spec = Spec::sh("cat data").inputs(&[("data", b"soon gone", false)]);
    let before = farm.objects().await;
    farm.cas
        .put(b"soon gone".to_vec())
        .await
        .expect("upload the input alone");
    let lost: Vec<ObjectKey> = farm
        .objects()
        .await
        .into_iter()
        .filter(|k| !before.contains(k))
        .collect();
    assert_eq!(lost.len(), 1, "one object holds the input");
    let action = farm.upload(&spec).await;
    farm.cache.objects().delete(&lost[0]).await.expect("delete");

    let response = farm.execute(&action).await;
    assert_eq!(response.result, None);
    let status = response.status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");
    let miss = kbf_proto::reapi::action_cache_client::ActionCacheClient::new(farm.reapi.clone())
        .get_action_result(kbf_proto::reapi::GetActionResultRequest {
            action_digest: Some(action),
            ..Default::default()
        })
        .await
        .expect_err("no action-cache entry");
    assert_eq!(miss.code(), Code::NotFound);

    let after = farm.upload(&Spec::sh("echo still here")).await;
    let response = farm.execute(&after).await;
    let result = response.result.expect("the daemon still runs actions");
    assert_eq!(
        farm.text(result.stdout_digest.as_ref()).await,
        "still here\n"
    );
}

const GIB: u64 = 1 << 30;

/// The local runtime, except that a lease booked less memory than `needs` ends as a
/// driver's own-limit memory kill does ([`RuntimeError::OutOfMemory`]: the native
/// driver's memory watch, or the container driver at the lease's cap). Records each
/// lease's memory booking in `booked`, in the order the leases started.
struct NeedsMemory {
    inner: LocalRuntime<CasClient>,
    needs: u64,
    booked: Arc<Mutex<Vec<u64>>>,
}

impl Runtime for NeedsMemory {
    fn driver(&self) -> &'static str {
        self.inner.driver()
    }

    fn serves(&self, kind: &str) -> bool {
        self.inner.serves(kind)
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let limit = work.resources.memory_bytes;
        self.booked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(limit);
        if limit < self.needs {
            return Err(RuntimeError::OutOfMemory {
                used: self.needs,
                limit,
            });
        }
        self.inner.run(work).await
    }

    async fn kill(&self, lease_id: LeaseId) {
        self.inner.kill(lease_id).await;
    }
}

/// A farm whose one node (8 GiB) kills every lease booked less than `needs`, and the
/// bookings its leases ran with, in GiB.
async fn needing(name: &str, needs: u64) -> (Farm, impl Fn() -> Vec<u64>) {
    let booked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&booked);
    let farm = Farm::start_with(name, move |inner| NeedsMemory {
        inner,
        needs,
        booked,
    })
    .await;
    let gib = move || {
        let booked = seen.lock().unwrap_or_else(PoisonError::into_inner);
        booked.iter().map(|bytes| bytes / GIB).collect()
    };
    (farm, gib)
}

/// Catches, with the real daemon and server between them: a daemon that sends a
/// driver's own-limit kill without `MEMORY_KILL_OWN_LIMIT`, or a server that does not
/// read it with the status the daemon pairs it with (either way the client is answered
/// at once, after one run); a rerun that does not double (1, 2, 4 GiB) or whose doubled
/// booking does not reach the daemon's `Start`; and a later Execute of the action that
/// does not start at the booking that ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drivers_own_limit_kill_reruns_the_action_with_its_booking_doubled() {
    let (farm, booked) = needing("exec-oom", 4 * GIB).await;
    let action = farm.upload(&Spec::sh("echo fits")).await;

    let response = farm.execute(&action).await;
    assert_eq!(response.status.map(|s| s.code), Some(Code::Ok as i32));
    let result = response.result.expect("a result");
    assert_eq!(farm.text(result.stdout_digest.as_ref()).await, "fits\n");
    assert_eq!(booked(), [1, 2, 4], "the bookings of the runs");

    let again = farm.execute(&action).await;
    assert_eq!(again.status.map(|s| s.code), Some(Code::Ok as i32));
    assert_eq!(booked(), [1, 2, 4, 4], "starts at the raised booking");
}

/// Catches a driver's own-limit kill at the largest node run again forever, or answered
/// as anything but the action needing more memory than any node offers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drivers_own_limit_kill_at_the_largest_node_is_answered() {
    let (farm, booked) = needing("exec-oom-cap", 16 * GIB).await;
    let action = farm.upload(&Spec::sh("echo never")).await;

    let response = farm.execute(&action).await;
    assert_eq!(response.result, None);
    let status = response.status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");
    assert_eq!(booked(), [1, 2, 4, 8], "doubled up to the node's 8 GiB");
}

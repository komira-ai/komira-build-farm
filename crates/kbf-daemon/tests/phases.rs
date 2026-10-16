//! Where a lease's time went (`kbf_daemon::phases`): a real server and a real daemon
//! in one process run an action through the test-only local runtime over the transfer
//! engine. The daemon's `LeaseEnded` event, the same fields as its `lease phases:` log
//! line, must account for the lease's time, and the engine's `transfers:` line for the
//! same `lease=` must account for what the lease moved, as the server counted it.

#![cfg(target_os = "linux")]

mod support;

use std::collections::BTreeMap;
use std::future::pending;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::{Cas, digest_of};
use kbf_daemon::phases::LeasePhases;
use kbf_daemon::{Daemon, DaemonConfig, Event, LocalRuntime, NodeReport};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{Digest, ExecuteRequest, ExecuteResponse};
use kbf_server::{Listeners, WorkerTls, bind_server};
use kbf_transfer::{TransferConfig, Transfers};
use kbf_types::LeaseId;
use prost::Message;
use support::memory::Spec;
use support::{PROMPT, pki, scratch};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// How long the action sleeps: the run phase must hold it.
const SLEEP: Duration = Duration::from_millis(600);

/// How far the phases may fall short of the lease's time: what the local runtime does
/// outside its three stamped phases (a scratch directory made and removed).
const SLACK_MS: u64 = 500;

/// A server and a daemon on loopback ports, a REAPI channel to the server, and the
/// daemon's events.
struct Farm {
    cache: Arc<MemoryCache>,
    reapi: Channel,
    cas: Transfers,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Farm {
    async fn start(name: &str) -> Self {
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
            memory_ladder: kbf_server::MemoryLadder::default(),
            finished_retention: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(10),
            timing: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            streams: kbf_server::StreamLimits::DEFAULT,
        };
        let bound = bind_server(Arc::clone(&cache), listeners, pending()).expect("bind");
        let (reapi_addr, worker_addr) = (bound.reapi, bound.worker);
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        let reapi = Endpoint::from_shared(format!("http://{reapi_addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let runtime = Arc::new(LocalRuntime::new(
            Arc::new(Transfers::over(reapi.clone(), TransferConfig::default())),
            scratch(name),
        ));
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
        let (tx, events) = mpsc::unbounded_channel();
        let daemon = Daemon::new(config, runtime, report)
            .expect("daemon config")
            .with_events(tx);
        tokio::spawn(daemon.run(pending()));
        Self {
            cache,
            cas: Transfers::over(reapi.clone(), TransferConfig::default()),
            reapi,
            events,
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

    /// The next `LeaseEnded` event.
    async fn ended(&mut self) -> LeasePhases {
        timeout(PROMPT, async {
            loop {
                match self.events.recv().await.expect("the daemon runs") {
                    Event::LeaseEnded(phases) => return phases,
                    _ => continue,
                }
            }
        })
        .await
        .expect("a lease ended in time")
    }
}

/// Everything this test binary logs, from its one global subscriber.
fn log() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&log);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || Captured(Arc::clone(&writer)))
            .finish();
        // The daemon's and the engine's tasks run on every worker thread, so the
        // subscriber is the process's, not one thread's.
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber");
        log
    })
}

/// Bytes written to [`log`].
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut log = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        log.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The tests of this binary one at a time: each server numbers its leases from the
/// same start, so two at once would log `transfers:` lines with the same `lease=`.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The fields of the engine's `transfers:` line for `lease`, logged after `from`
/// bytes of [`log`]; waits for it, as the engine logs it when the lease's last handle
/// goes.
async fn transfers(lease: LeaseId, from: usize) -> BTreeMap<String, u64> {
    let want = format!("lease={lease}");
    timeout(PROMPT, async {
        loop {
            let text = {
                let log = log().lock().unwrap_or_else(PoisonError::into_inner);
                String::from_utf8_lossy(&log[from..]).into_owned()
            };
            let line = text.lines().find(|l| {
                l.contains(" INFO ")
                    && l.contains("transfers: ")
                    && l.split(' ').any(|field| field == want)
            });
            if let Some(line) = line {
                let (_, fields) = line.split_once("transfers: ").expect("the line");
                return fields
                    .split(' ')
                    .filter_map(|field| field.split_once('='))
                    .filter_map(|(k, v)| Some((k.to_owned(), v.parse().ok()?)))
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the engine logged the lease's transfers in time")
}

/// The length of [`log`] now.
fn logged() -> usize {
    log().lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Catches a lease-end report that leaves out a phase, takes a phase from the wrong
/// stamps (the run phase must hold the action's sleep, and the three phases must
/// account for the lease's time within [`SLACK_MS`]). Also the log line's fields: each
/// named once, in order. And the engine's `transfers:` line for the same lease must
/// count its CAS calls, not another lease's or none: the blobs and bytes it wrote must
/// be the ByteStream Writes the server received while it ran (none of the test's own
/// uploads before it), the lease's 8 uploads must be 7 written and the empty stderr
/// skipped (the server always holds the empty blob), and the reads must be the Action,
/// the Command, the input root and its one file. Mutants seen red: the run phase read
/// from the upload stamps; `written_blobs` never counted (0 against the server's 7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_accounts_for_its_time_and_its_transfers() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let from = logged();
    let mut farm = Farm::start("phases-run").await;
    let sleep = format!("{}", SLEEP.as_secs_f64());
    let spec = Spec::act(&[
        "mkdir",
        "out",
        ";",
        "upper",
        "out/a",
        "in.txt",
        ";",
        "mkdir",
        "out/sub",
        ";",
        "append",
        "out/sub/b",
        "below",
        ";",
        "append",
        "top",
        "top level",
        ";",
        "echo",
        "ran",
        ";",
        "sleep",
        &sleep,
    ])
    .outputs(&["out", "top"])
    .inputs(&[("in.txt", b"an input file\n", false)]);
    let action = farm.upload(&spec).await;
    let writes = farm.cache.metrics().write.total.get();
    let written = farm.cache.metrics().write.bytes.get();

    let response = farm.execute(&action).await;
    let result = response.result.expect("a result");
    assert_eq!(result.exit_code, 0);
    let phases = farm.ended().await;

    assert!(phases.ok);
    assert_eq!(phases.driver, "local");
    let inputs = phases.inputs_ms.expect("inputs_ms");
    let run = phases.run_ms.expect("run_ms");
    let outputs = phases.outputs_ms.expect("outputs_ms");
    let sleep_ms = u64::try_from(SLEEP.as_millis()).expect("small");
    assert!(run >= sleep_ms, "run_ms {run} under the action's sleep");
    assert!(inputs < sleep_ms && outputs < sleep_ms, "{phases}");
    let sum = inputs + run + outputs;
    // Each phase is rounded down to the millisecond.
    assert!(
        sum <= phases.lease_ms + 3,
        "{sum} ms of phases in a lease of {phases}"
    );
    assert!(
        phases.lease_ms - sum.min(phases.lease_ms) <= SLACK_MS,
        "the phases leave out more than {SLACK_MS} ms: {phases}"
    );
    assert_eq!((phases.output_files, phases.output_dirs), (1, 1));

    let t = transfers(phases.lease, from).await;
    let metrics = farm.cache.metrics();
    assert_eq!(
        (t["written_blobs"], t["written_bytes"]),
        (
            metrics.write.total.get() - writes,
            metrics.write.bytes.get() - written
        ),
        "{t:?}"
    );
    // out/a, out/sub/b, top, the Directories of out/sub and out, the Tree and stdout
    // written; the empty stderr skipped, as one the server holds.
    assert_eq!((t["written_blobs"], t["outputs_present"]), (7, 1), "{t:?}");
    assert_eq!(t["write_calls"], t["written_blobs"], "{t:?}");
    assert_eq!((t["read_calls"], t["merged_reads"]), (4, 0), "{t:?}");
    let input = b"an input file\n".len() as u64;
    assert!(t["read_bytes"] > input, "{t:?}");

    let line = phases.to_string();
    let keys: Vec<&str> = line
        .split(' ')
        .map(|field| field.split_once('=').expect("key=value").0)
        .collect();
    assert_eq!(
        keys,
        [
            "lease",
            "driver",
            "ok",
            "lease_ms",
            "inputs_ms",
            "run_ms",
            "outputs_ms",
            "output_files",
            "output_dirs",
            // Appended after the existing keys; `-` here, the local runtime having no
            // lease cgroup.
            "cpu_usage_us",
            "cpu_user_us",
            "cpu_system_us",
            "cpu_periods",
            "cpu_throttled",
            "throttled_us",
            "mem_peak",
            "parallelism",
            "numa_bytes",
            "numa_spread",
        ]
    );
    assert!(line.ends_with(" cpu_usage_us=- cpu_user_us=- cpu_system_us=- cpu_periods=- cpu_throttled=- throttled_us=- mem_peak=- parallelism=- numa_bytes=- numa_spread=-"), "{line}");
    assert!(line.contains(&format!(" run_ms={run} ")), "{line}");
}

/// Catches a lease that produced no result reported with phase times it does not
/// have (the line must say `-`), or not reported at all: a run that fails still
/// ends its lease, and the engine still logs what it read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_lease_ends_without_phase_times() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let from = logged();
    let mut farm = Farm::start("phases-failed").await;
    // A Command without arguments: the local runtime refuses it once it has read the
    // Action and the Command.
    let action = farm.upload(&Spec::argv(Vec::<&str>::new())).await;

    let _ = farm.execute(&action).await;
    let phases = farm.ended().await;
    assert!(!phases.ok, "{phases}");
    assert_eq!(
        (phases.inputs_ms, phases.run_ms, phases.outputs_ms),
        (None, None, None)
    );
    assert_eq!((phases.output_files, phases.output_dirs), (0, 0));
    let t = transfers(phases.lease, from).await;
    assert_eq!(
        (t["read_calls"], t["written_blobs"]),
        (2, 0),
        "the Action and the Command: {t:?}"
    );
    let line = phases.to_string();
    assert!(
        line.contains(" ok=false ") && line.contains(" inputs_ms=- run_ms=- outputs_ms=- "),
        "{line}"
    );
}

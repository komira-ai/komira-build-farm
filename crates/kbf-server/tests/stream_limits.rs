//! The HTTP/2 stream limit of each listener, and the flow-control windows that follow it.
//!
//! buck2 holds one Execute stream open per outstanding action, idle almost the whole
//! time, so the REAPI listener must not hide its backlog behind a small per-connection
//! cap (`--reapi-max-streams`, 10000 by default). A daemon holds one session stream and
//! a few short transfer streams, so the worker listener keeps 200
//! (`--worker-max-streams`). The connection window must hold every stream's window at
//! once, so a higher limit shrinks the stream window rather than leaving the
//! connection window short.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use kbf_front::{DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS, STREAM_WINDOW_BYTES};
use kbf_proto::reapi::ExecuteRequest;
use kbf_server::StreamLimits;
use support::{Blob, Cell, Job, PROMPT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The test is only a test if the actions pass the worker's limit and fit the REAPI's.
const _: () =
    assert!(ACTIONS > DEFAULT_WORKER_STREAMS as usize && ACTIONS < DEFAULT_REAPI_STREAMS as usize);
/// buck2's outstanding actions, in the test below.
const ACTIONS: usize = 1000;

/// What a server says about its connection in its first frames: the SETTINGS values
/// the test reads, and the connection window its WINDOW_UPDATE on stream 0 makes.
#[derive(Debug, PartialEq, Eq)]
struct Advertised {
    max_streams: Option<u32>,
    stream_window: Option<u32>,
    connection_window: u64,
}

/// The HTTP/2 client connection preface, then an empty SETTINGS frame.
const CLIENT_HELLO: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00";
const SETTINGS: u8 = 4;
const WINDOW_UPDATE: u8 = 8;
const MAX_CONCURRENT_STREAMS: u16 = 3;
const INITIAL_WINDOW_SIZE: u16 = 4;
/// A connection's window before any WINDOW_UPDATE (RFC 9113, section 6.9.2).
const DEFAULT_WINDOW: u64 = 65_535;

/// Connects to `addr`, says hello as an HTTP/2 client and reads the server's frames
/// until it has sent its SETTINGS and a connection-level WINDOW_UPDATE (the server
/// sends both unprompted, in its first flight). Waits for those frames, never for a
/// time: [`PROMPT`] only bounds a server that never sends them.
async fn advertised(addr: SocketAddr) -> Advertised {
    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    tcp.write_all(CLIENT_HELLO).await.expect("hello");
    let mut seen = Advertised {
        max_streams: None,
        stream_window: None,
        connection_window: DEFAULT_WINDOW,
    };
    let (mut settings, mut update) = (false, false);
    let read = async {
        while !(settings && update) {
            let mut head = [0u8; 9];
            tcp.read_exact(&mut head).await.expect("a frame header");
            let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
            let (kind, flags) = (head[3], head[4]);
            let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
            let mut payload = vec![0u8; len];
            tcp.read_exact(&mut payload).await.expect("a frame payload");
            match kind {
                SETTINGS if flags & 1 == 0 => {
                    settings = true;
                    for pair in payload.chunks_exact(6) {
                        let id = u16::from_be_bytes([pair[0], pair[1]]);
                        let value = u32::from_be_bytes([pair[2], pair[3], pair[4], pair[5]]);
                        match id {
                            MAX_CONCURRENT_STREAMS => seen.max_streams = Some(value),
                            INITIAL_WINDOW_SIZE => seen.stream_window = Some(value),
                            _ => {}
                        }
                    }
                }
                WINDOW_UPDATE if stream == 0 => {
                    update = true;
                    let by = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    seen.connection_window = DEFAULT_WINDOW + u64::from(by & 0x7fff_ffff);
                }
                _ => {}
            }
        }
    };
    let finished = tokio::time::timeout(PROMPT, read).await;
    assert!(
        finished.is_ok(),
        "{addr} sent no SETTINGS and connection WINDOW_UPDATE: {seen:?}"
    );
    seen
}

/// Waits until `read` returns `want`, up to `within`, failing with what it last read.
async fn settles(what: &str, within: Duration, read: impl Fn() -> u64, want: u64) {
    let deadline = tokio::time::Instant::now() + within;
    while read() != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} is {} after {within:?}, wanted {want}",
            read()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Catches: a REAPI listener that caps one connection's streams at the worker's 200
/// (the state before `--reapi-max-streams`): buck2's 1000 outstanding actions, each an
/// idle Execute stream on one connection, are then held in the client, and the
/// scheduler sees 200. Every action here is distinct, so none joins another's
/// operation, and no worker is connected, so every one waits queued. The client is one
/// tonic channel, which is one HTTP/2 connection, and its HTTP/2 client holds a request
/// back while the server's limit is reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_idle_executes_on_one_connection_are_all_queued() {
    let cell = Cell::start().await;
    let m = cell.cache.metrics();
    let jobs: Vec<Job> = (0..ACTIONS)
        .map(|i| Job::new(&format!("action-{i}"), &[]))
        .collect();
    for chunk in jobs.chunks(50) {
        let blobs: Vec<&Blob> = chunk.iter().flat_map(Job::blobs).collect();
        cell.upload(&blobs).await;
    }
    for job in &jobs {
        let mut exec = cell.exec();
        let request = ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(job.action.proto.clone()),
            ..Default::default()
        };
        // The stream is held open, idle, as buck2 holds it, until the test ends.
        tokio::spawn(async move {
            let stream = exec.execute(request).await.expect("Execute");
            let _held = stream.into_inner();
            std::future::pending::<()>().await;
        });
    }
    settles(
        "kbf_sched_queued",
        Duration::from_secs(30),
        || m.sched_queued.get(),
        ACTIONS as u64,
    )
    .await;
}

/// Catches: the REAPI listener left at the worker's limit, or the worker listener
/// raised with it (the first Execute a daemon-side limit hides is a farm that looks
/// idle; the second is a daemon connection that may hold 10000 streams); a flag that
/// reaches the wrong listener, shown by limits that differ (7 and 3). Read off the
/// server's own SETTINGS frame, as a client sees it.
#[tokio::test]
async fn each_listener_advertises_its_own_stream_limit() {
    let cell = Cell::start().await;
    let reapi = advertised(cell.reapi_addr).await;
    let worker = advertised(cell.worker_addr).await;
    assert_eq!(reapi.max_streams, Some(10_000), "REAPI: {reapi:?}");
    assert_eq!(worker.max_streams, Some(200), "worker: {worker:?}");

    let cell = Cell::start_with_streams(StreamLimits {
        reapi: 7,
        worker: 3,
    })
    .await;
    assert_eq!(advertised(cell.reapi_addr).await.max_streams, Some(7));
    assert_eq!(advertised(cell.worker_addr).await.max_streams, Some(3));
}

/// Catches: a connection window smaller than the streams its listener allows times
/// each stream's window (a fixed 200 MiB beside 10000 streams: the unread data of the
/// Writes waiting for admission can take all of it, and the admitted ones starve;
/// `write_admission.rs` runs that), at any limit, on either listener; a stream window
/// under HTTP/2's default 64 KiB; a worker listener whose 200 streams do not keep 1 MiB
/// each. Read off the server's own frames, as a client sees them.
#[tokio::test]
async fn the_connection_window_holds_every_streams_window_whatever_the_limit() {
    for limits in [
        StreamLimits::DEFAULT,
        StreamLimits {
            reapi: 32_767,
            worker: 1,
        },
        StreamLimits {
            reapi: 1,
            worker: 32_767,
        },
        StreamLimits {
            reapi: 2048,
            worker: 2047,
        },
    ] {
        let cell = Cell::start_with_streams(limits).await;
        for (name, addr, streams) in [
            ("REAPI", cell.reapi_addr, limits.reapi),
            ("worker", cell.worker_addr, limits.worker),
        ] {
            let got = advertised(addr).await;
            let window = u64::from(got.stream_window.expect("a stream window"));
            assert_eq!(got.max_streams, Some(streams), "{name} with {limits:?}");
            assert!(
                got.connection_window >= u64::from(streams) * window,
                "{name} listener with {limits:?}: {got:?}"
            );
            assert!(window >= 65_536, "{name} listener with {limits:?}: {got:?}");
        }
    }
    let worker = advertised(Cell::start().await.worker_addr).await;
    assert_eq!(worker.stream_window, Some(STREAM_WINDOW_BYTES));
    assert_eq!(
        worker.connection_window,
        200 * u64::from(STREAM_WINDOW_BYTES)
    );
}

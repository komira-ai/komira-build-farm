//! The proxy never holds a blob, judged by allocation: the most bytes this process had
//! live at once while a 32 MiB blob crossed it, the client, the proxy and the fake node
//! all in the one process. A test binary of its own, with one test, so nothing else
//! allocates beside it; the allocator counts and does nothing else.

#[path = "proxy/harness.rs"]
mod harness;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use futures::stream;
use harness::{CHUNK, Fake, blob, front, node, owner, pattern, upload};
use kbf_auth::Authorizers;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use tonic::Request;

/// Bytes live now and the most there have been since the last reset.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

struct Counting;

// SAFETY: every call goes to `System` with the caller's arguments unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract, which is `System.alloc`'s.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and the caller upholds `realloc`'s contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            grew(new_size);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` and says the most bytes that were live at once during it beyond those live
/// when it began.
async fn peak_during<T>(f: impl std::future::Future<Output = T>) -> usize {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    f.await;
    PEAK.load(Relaxed).saturating_sub(base)
}

const MIB: usize = 1 << 20;
const BLOB: u64 = 32 * MIB as u64;

async fn read(client: &mut ByteStreamClient<tonic::transport::Channel>) {
    let request = ReadRequest {
        resource_name: blob("main", '0', BLOB),
        ..Default::default()
    };
    let mut s = client.read(request).await.expect("read").into_inner();
    while let Some(m) = s.message().await.expect("message") {
        assert_eq!(m.data.len(), CHUNK);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await; // a slow client
    }
}

async fn write(
    client: &mut ByteStreamClient<tonic::transport::Channel>,
    name: String,
    chunks: usize,
) {
    let sent = stream::iter((0..chunks).map(move |i| WriteRequest {
        resource_name: if i == 0 { name.clone() } else { String::new() },
        write_offset: (i * CHUNK) as i64,
        finish_write: i == chunks - 1,
        data: vec![pattern(i); CHUNK],
    }));
    client.write(Request::new(sent)).await.expect("write");
}

/// Catches: a proxy that holds a blob, or sizes a buffer by the size a name declares.
/// The whole path (both ends and the proxy between) peaks far under the blob while it
/// crosses in either direction; a proxy that collects (the control) peaks over it; and a
/// Write that declares 16 GiB while sending 3 MiB allocates nothing near it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_crosses_the_proxy_without_being_held() {
    let fake = Fake::new(true);
    let f = front(
        vec![owner("a", node(&fake).await)],
        Authorizers::allow_all(),
        None,
    )
    .await;
    let mut client = ByteStreamClient::new(f.channel.clone());
    // The first call sets up the connections and pools.
    write(&mut client, upload("main", '0', 2 * CHUNK as u64), 2).await;

    let read_peak = peak_during(read(&mut client)).await;
    let name = upload("main", '0', BLOB);
    let write_peak = peak_during(write(&mut client, name, 32)).await;
    let huge = upload("main", '0', 16 << 30);
    let huge_peak = peak_during(write(&mut client, huge, 3)).await;
    eprintln!("peaks: read {read_peak}, write {write_peak}, huge {huge_peak}");
    let limit = BLOB as usize / 2;
    assert!(read_peak < limit, "a read peaked at {read_peak}");
    assert!(write_peak < limit, "a write peaked at {write_peak}");
    assert!(
        huge_peak < limit,
        "a write that declares 16 GiB peaked at {huge_peak}"
    );

    // The control: the same node behind a proxy that holds what crosses it.
    let control =
        ByteStreamClient::new(harness::serve(harness::collecting(node(&fake).await)).await);
    let collected = peak_during(read(&mut { control.clone() })).await;
    assert!(
        collected > BLOB as usize,
        "a collecting proxy peaked at only {collected}"
    );
}

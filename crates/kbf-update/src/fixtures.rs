//! Values and fakes the unit tests share: an in-memory release service, a free-space
//! source a test sets, and scratch directories.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream;
use kbf_threads::disk::{FreeSpace, Space};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Notify, Semaphore};

use crate::source::{Chunks, FetchError, Source};
use crate::types::{Component, Offer};

pub(crate) const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
pub(crate) const PLATFORM: &str = "linux-x86_64";

pub(crate) fn component(name: &str) -> Component {
    Component {
        name: name.to_owned(),
        sha256: "ab".repeat(32),
        size: 1000,
        attestation: b"{\"bundle\":1}\n".to_vec(),
    }
}

pub(crate) fn offer() -> Offer {
    Offer {
        generation: 1,
        commit: COMMIT.to_owned(),
        platform: PLATFORM.to_owned(),
        components: vec![component("kbf")],
    }
}

/// The 40-digit commit of generation `generation`: the number in hex, zero padded.
pub(crate) fn commit(generation: u64) -> String {
    format!("{generation:040x}")
}

/// An empty directory for one test, under the test binary's own directory.
pub(crate) fn scratch(name: &str) -> PathBuf {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-update-unit")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            CALLS.fetch_add(1, Ordering::Relaxed)
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// `len` bytes that depend on `seed`.
pub(crate) fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).expect("below 251").wrapping_add(seed))
        .collect()
}

/// The offer of `generation` for the blob `data`, as the frontend would make it.
pub(crate) fn offer_of(generation: u64, data: &[u8]) -> Offer {
    Offer {
        generation,
        commit: commit(generation),
        platform: PLATFORM.to_owned(),
        components: vec![Component {
            name: "kbf".to_owned(),
            sha256: hex::encode(Sha256::digest(data)),
            size: data.len() as u64,
            attestation: b"{\"bundle\":1}\n".to_vec(),
        }],
    }
}

/// What a test makes the next stream do.
#[derive(Default)]
struct Script {
    /// Calls to `fetch` that fail with a transport error before they return a stream.
    refuse_calls: usize,
    /// Streams that fail with a transport error after their first chunk.
    break_streams: usize,
}

/// A release service in memory.
pub(crate) struct FakeSource {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    script: Mutex<Script>,
    /// Calls to `fetch`.
    pub fetches: AtomicUsize,
    /// Chunks handed out, over all streams.
    pub chunks: Arc<AtomicUsize>,
    chunk_size: usize,
    /// When set, a stream waits for a permit after its first chunk.
    stall: Mutex<Option<Arc<Semaphore>>>,
    /// Rung when a stream has handed out its first chunk.
    pub started: Arc<Notify>,
    /// Run when `fetch` is called, before it answers.
    on_fetch: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Run when a stream has handed out its last chunk.
    on_end: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl FakeSource {
    pub(crate) fn new(chunk_size: usize) -> Arc<Self> {
        Arc::new(Self {
            blobs: Mutex::default(),
            script: Mutex::default(),
            fetches: AtomicUsize::new(0),
            chunks: Arc::new(AtomicUsize::new(0)),
            chunk_size,
            stall: Mutex::new(None),
            started: Arc::new(Notify::new()),
            on_fetch: Mutex::new(None),
            on_end: Mutex::new(None),
        })
    }

    /// Serves `data` under its own digest.
    pub(crate) fn serve(&self, data: &[u8]) -> String {
        let digest = hex::encode(Sha256::digest(data));
        self.serve_as(&digest, data);
        digest
    }

    /// Serves `data` under `digest`, which it need not match.
    pub(crate) fn serve_as(&self, digest: &str, data: &[u8]) {
        self.blobs
            .lock()
            .expect("lock")
            .insert(digest.to_owned(), data.to_vec());
    }

    pub(crate) fn refuse_calls(&self, n: usize) {
        self.script.lock().expect("lock").refuse_calls = n;
    }

    pub(crate) fn break_streams(&self, n: usize) {
        self.script.lock().expect("lock").break_streams = n;
    }

    /// Makes streams wait after their first chunk until the returned semaphore has a
    /// permit.
    pub(crate) fn stall(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.stall.lock().expect("lock") = Some(Arc::clone(&gate));
        gate
    }

    pub(crate) fn unstall(&self) {
        self.stall.lock().expect("lock").take();
    }

    pub(crate) fn on_fetch(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_fetch.lock().expect("lock") = Some(Arc::new(hook));
    }

    pub(crate) fn on_end(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_end.lock().expect("lock") = Some(Arc::new(hook));
    }
}

impl Source for FakeSource {
    fn fetch(&self, sha256: &str) -> BoxFuture<'_, Result<Chunks, FetchError>> {
        let sha256 = sha256.to_owned();
        Box::pin(async move {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let hook = self.on_fetch.lock().expect("lock").clone();
            if let Some(hook) = hook {
                hook();
            }
            let (refuse, breaks) = {
                let mut script = self.script.lock().expect("lock");
                let refuse = script.refuse_calls > 0;
                script.refuse_calls = script.refuse_calls.saturating_sub(1);
                let breaks = !refuse && script.break_streams > 0;
                if breaks {
                    script.break_streams -= 1;
                }
                (refuse, breaks)
            };
            if refuse {
                return Err(FetchError::Transport("connection reset".to_owned()));
            }
            let data = self
                .blobs
                .lock()
                .expect("lock")
                .get(&sha256)
                .cloned()
                .ok_or_else(|| FetchError::Missing(sha256.clone()))?;
            let pieces: Vec<Vec<u8>> = data.chunks(self.chunk_size).map(<[u8]>::to_vec).collect();
            let counter = Arc::clone(&self.chunks);
            let stall = self.stall.lock().expect("lock").clone();
            let started = Arc::clone(&self.started);
            let end = self.on_end.lock().expect("lock").clone();
            let total = pieces.len();
            let stream = stream::unfold(0_usize, move |index| {
                let pieces = pieces.clone();
                let counter = Arc::clone(&counter);
                let stall = stall.clone();
                let started = Arc::clone(&started);
                let end = end.clone();
                async move {
                    if index == total {
                        if let Some(end) = end {
                            end();
                        }
                        return None;
                    }
                    if index >= 1 {
                        if breaks {
                            return Some((
                                Err(FetchError::Transport("stream broke".to_owned())),
                                total,
                            ));
                        }
                        if let Some(stall) = &stall {
                            stall.acquire().await.expect("open").forget();
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    if index == 0 {
                        started.notify_one();
                    }
                    Some((Ok(pieces[index].clone()), index + 1))
                }
            });
            Ok(Box::pin(stream) as Chunks)
        })
    }
}

/// Free space a test sets.
pub(crate) struct FakeSpace(pub Mutex<Result<u64, io::ErrorKind>>);

impl FakeSpace {
    pub(crate) fn plenty() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Ok(u64::MAX))))
    }
}

impl FreeSpace for FakeSpace {
    fn space(&self, _path: &Path) -> io::Result<Space> {
        match *self.0.lock().expect("lock") {
            Ok(available) => Ok(Space {
                total: u64::MAX,
                available,
            }),
            Err(kind) => Err(io::Error::from(kind)),
        }
    }
}

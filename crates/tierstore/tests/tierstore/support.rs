//! Fixtures shared by the tests.

use std::sync::Arc;

use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, FaultyBackend, MemoryBackend, Tiered};

/// A hot and a cold in-memory backend, each behind a fault injector. The memory backends
/// are kept so a test can read or change a tier without going through the store.
pub struct Rig {
    pub hot_mem: Arc<MemoryBackend>,
    pub cold_mem: Arc<MemoryBackend>,
    pub hot: Arc<FaultyBackend>,
    pub cold: Arc<FaultyBackend>,
}

impl Rig {
    pub fn new() -> Self {
        let hot_mem = Arc::new(MemoryBackend::new());
        let cold_mem = Arc::new(MemoryBackend::new());
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        Self {
            hot_mem,
            cold_mem,
            hot,
            cold,
        }
    }

    /// A store over the rig's backends that keeps the hot tier at `budget` bytes.
    pub async fn open(&self, budget: u64) -> Tiered {
        let opened = Tiered::open(self.hot.clone(), self.cold.clone(), budget).await;
        opened.unwrap()
    }
}

/// The body of an object with this text.
pub fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// The whole object under `key`.
pub async fn read(backend: &dyn Backend, key: &str) -> Bytes {
    backend.get(key, None).await.unwrap()
}

/// The keys a backend lists, in order.
pub async fn keys(backend: &dyn Backend) -> Vec<String> {
    let listed = backend.list("").await.unwrap();
    listed.into_iter().map(|object| object.key).collect()
}

/// Reads ranges of the ten-byte object `0123456789` under `key` and checks every one.
pub async fn check_ranges(backend: &dyn Backend, key: &str) {
    let cases = [
        (0, 1, "0"),
        (9, 1, "9"),
        (4, 3, "456"),
        (8, 100, "89"),
        (3, 0, ""),
        (10, 0, ""),
        (10, 5, ""),
    ];
    for (offset, len, want) in cases {
        let range = Some(ByteRange::new(offset, len));
        let got = backend.get(key, range).await.unwrap();
        assert_eq!(got, body(want), "range {offset}+{len}");
    }
    let whole = backend.get(key, None).await.unwrap();
    assert_eq!(whole, body("0123456789"));
    let past = backend.get(key, Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))));
}

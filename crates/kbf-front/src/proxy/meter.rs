//! What the pass-through counts: wire bytes per call and direction, and the bytes the
//! proxy itself holds between taking a frame and being asked for the next.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// A ByteStream call the raw path passes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `ByteStream.Read`.
    Read,
    /// `ByteStream.Write`.
    Write,
    /// `ByteStream.QueryWriteStatus`.
    QueryWriteStatus,
}

impl Method {
    /// The call a gRPC path names, if it is one the raw path serves.
    pub(super) fn of_path(path: &str) -> Option<Self> {
        match path {
            "/google.bytestream.ByteStream/Read" => Some(Self::Read),
            "/google.bytestream.ByteStream/Write" => Some(Self::Write),
            "/google.bytestream.ByteStream/QueryWriteStatus" => Some(Self::QueryWriteStatus),
            _ => None,
        }
    }

    /// The gRPC path, as authorization logs it.
    pub(super) const fn path(self) -> &'static str {
        match self {
            Self::Read => "/google.bytestream.ByteStream/Read",
            Self::Write => "/google.bytestream.ByteStream/Write",
            Self::QueryWriteStatus => "/google.bytestream.ByteStream/QueryWriteStatus",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Which way bytes pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the client, on to the storage node.
    ToStorage,
    /// From the storage node, back to the client.
    FromStorage,
}

/// Counters of the raw path. Bytes are wire bytes: each gRPC message counts its
/// protobuf length plus the 5-byte message prefix.
#[derive(Debug, Default)]
pub struct Meter {
    bytes: [[AtomicU64; 2]; 3],
    held: AtomicU64,
    held_peak: AtomicU64,
}

impl Meter {
    /// The bytes of `method` that passed in `direction`.
    #[must_use]
    pub fn bytes(&self, method: Method, direction: Direction) -> u64 {
        self.bytes[method.index()][direction as usize].load(Relaxed)
    }

    /// The most bytes the proxy held at once: frames pulled from one side and not yet
    /// asked past by the other. A proxy that buffers a blob shows it here.
    #[must_use]
    pub fn held_peak(&self) -> u64 {
        self.held_peak.load(Relaxed)
    }

    /// The bytes the proxy holds now: none when no call is in flight.
    #[must_use]
    pub fn held(&self) -> u64 {
        self.held.load(Relaxed)
    }

    pub(super) fn add(&self, method: Method, direction: Direction, n: u64) {
        self.bytes[method.index()][direction as usize].fetch_add(n, Relaxed);
    }

    pub(super) fn hold(&self, n: u64) {
        let now = self.held.fetch_add(n, Relaxed) + n;
        self.held_peak.fetch_max(now, Relaxed);
    }

    pub(super) fn release(&self, n: u64) {
        self.held.fetch_sub(n, Relaxed);
    }
}

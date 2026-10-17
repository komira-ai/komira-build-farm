//! Run ids: minted from memory when a `Result` commits, time-ordered, and able to find
//! their own chunk with no per-run index row.
//!
//! A [`RunId`] is 12 bytes, shown as 24 hex digits: 6 bytes of the commit time in
//! milliseconds, 3 bytes of [`StreamId`] and 3 bytes of row sequence in that stream. A
//! stream is one (invocation, writer, UTC day). Stream ids come from blocks of
//! [`BLOCK_SIZE`] the history writer reserves ahead of need from the `ui/streams`
//! counter; when no block is at hand the [`Minter`] takes a fallback id from an
//! in-process counter in a range no block uses (top bit set, then the writer's 7-bit
//! frontend index, then 16 bits), so minting never waits on the state store. Fallback
//! ids are not durable and are reused after a restart; the `ui/stream` row key carries
//! the stream's open time, so a reused id still resolves to its own chunk.
//!
//! The minter owns that open time: it is the time of the stream's first id, and every
//! later id of the stream carries a time no earlier than the one before it (a commit
//! time read before the lock is raised to it), so time order follows sequence order
//! and no run of a stream is earlier than the stream's row.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::keys::{Day, InvKey};

/// Stream ids per reserved block.
pub const BLOCK_SIZE: u32 = 4096;

/// The first stream id of the fallback range; no block reaches it.
const FALLBACK_BASE: u32 = 0x80_0000;

/// Blocks below the fallback range. The counter's first value is 1, so block 0 (and
/// stream id 0) is never handed out.
const BLOCKS: u64 = (FALLBACK_BASE / BLOCK_SIZE) as u64;

/// The largest row sequence a stream holds (3 bytes); the next run opens a new stream.
const MAX_SEQ: u32 = 0xFF_FFFF;

/// The largest commit time a run id carries (6 bytes of milliseconds, year 10889).
const MAX_TIME_MS: u64 = 0xFFFF_FFFF_FFFF;

/// Open streams the minter holds at most (one per invocation and day, about 100 bytes
/// each). Past it the first by key is forgotten, which costs only a new stream id for
/// that invocation's next run.
const OPEN_STREAMS: usize = 65_536;

/// A stream of run ids: 24 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub u32);

impl StreamId {
    /// Whether the id is from the fallback range (not durable, reused after a restart).
    #[must_use]
    pub fn is_fallback(self) -> bool {
        self.0 & FALLBACK_BASE != 0
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06x}", self.0)
    }
}

/// The id of one run record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId {
    time_ms: u64,
    stream: StreamId,
    seq: u32,
}

impl RunId {
    /// A run id from its parts, each cut to its width (6, 3 and 3 bytes).
    #[must_use]
    pub fn new(time_ms: u64, stream: StreamId, seq: u32) -> RunId {
        RunId {
            time_ms: time_ms.min(MAX_TIME_MS),
            stream: StreamId(stream.0 & 0xFF_FFFF),
            seq: seq & MAX_SEQ,
        }
    }

    /// The commit time, in milliseconds after the Unix epoch.
    #[must_use]
    pub fn time_ms(self) -> u64 {
        self.time_ms
    }

    /// The UTC day of the commit, which is the stream's day.
    #[must_use]
    pub fn day(self) -> Day {
        Day::from_ms(self.time_ms)
    }

    /// The stream.
    #[must_use]
    pub fn stream(self) -> StreamId {
        self.stream
    }

    /// The row sequence in the stream.
    #[must_use]
    pub fn seq(self) -> u32 {
        self.seq
    }

    /// The 12 bytes, big-endian, so byte order is time order.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 12] {
        let mut b = [0; 12];
        b[..6].copy_from_slice(&self.time_ms.to_be_bytes()[2..]);
        b[6..9].copy_from_slice(&self.stream.0.to_be_bytes()[1..]);
        b[9..].copy_from_slice(&self.seq.to_be_bytes()[1..]);
        b
    }

    /// The id of 12 bytes.
    #[must_use]
    pub fn from_bytes(b: [u8; 12]) -> RunId {
        let mut t = [0; 8];
        t[2..].copy_from_slice(&b[..6]);
        let mut s = [0; 4];
        s[1..].copy_from_slice(&b[6..9]);
        let mut q = [0; 4];
        q[1..].copy_from_slice(&b[9..]);
        RunId::new(
            u64::from_be_bytes(t),
            StreamId(u32::from_be_bytes(s)),
            u32::from_be_bytes(q),
        )
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.to_bytes()))
    }
}

/// Text that is not a run id.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a run id (24 hex digits)")]
pub struct ParseRunIdError(String);

impl FromStr for RunId {
    type Err = ParseRunIdError;

    fn from_str(s: &str) -> Result<RunId, ParseRunIdError> {
        let mut b = [0; 12];
        hex::decode_to_slice(s, &mut b).map_err(|_| ParseRunIdError(s.to_owned()))?;
        Ok(RunId::from_bytes(b))
    }
}

/// A freshly minted run id, whether it opened its stream, and when the stream opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Minted {
    /// The id.
    pub id: RunId,
    /// The id is its stream's first (sequence 0).
    pub opened_stream: bool,
    /// The time of the stream's first id, milliseconds: the `ui/stream` row's key. The
    /// history event of the run carries it ([`Event::Run`](crate::Event::Run)).
    pub stream_opened_ms: u64,
}

/// An open stream of one (invocation, day).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenStream {
    stream: StreamId,
    /// The next sequence.
    next: u32,
    /// The time of its first id.
    opened_ms: u64,
    /// The time of its newest id.
    last_ms: u64,
}

/// Mints run ids from memory. Shared between the request path, which mints, and the
/// history writer, which refills its stream blocks.
#[derive(Debug)]
pub struct Minter {
    state: Mutex<MintState>,
}

#[derive(Debug)]
struct MintState {
    /// The writer's frontend index (7 bits) in the fallback range.
    frontend: u32,
    /// Unused ids of the current block, `[next, end)`.
    current: Option<(u32, u32)>,
    /// A block reserved ahead of need.
    spare: Option<u32>,
    /// The next fallback id's low 16 bits.
    fallback: u16,
    /// The open stream of each (invocation, day).
    open: BTreeMap<(InvKey, Day), OpenStream>,
    /// Entries of `open` at most.
    open_limit: usize,
}

impl Minter {
    /// A minter for the writer whose frontend index is `frontend` (0 on the single
    /// frontend; only its low 7 bits are used). It holds no block: until the writer
    /// reserves one, ids come from the fallback range.
    #[must_use]
    pub fn new(frontend: u8) -> Minter {
        Minter {
            state: Mutex::new(MintState {
                frontend: u32::from(frontend & 0x7F),
                current: None,
                spare: None,
                fallback: 0,
                open: BTreeMap::new(),
                open_limit: OPEN_STREAMS,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, MintState> {
        // The state is a few integers updated in place; a panic elsewhere leaves it whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of a run of invocation `inv` whose `Result` committed at `commit_ms`.
    /// Never waits: a stream id comes from the current block, the spare block, or the
    /// fallback range. The id's time is `commit_ms`, raised to the stream's newest id's
    /// time when the caller's clock read lost a race with another mint.
    pub fn mint(&self, inv: &InvKey, commit_ms: u64) -> Minted {
        let mut st = self.state();
        let day = Day::from_ms(commit_ms);
        let slot = (inv.clone(), day);
        let open = match st.open.get(&slot) {
            // The raised time is the newest id's, of the same day.
            Some(&o) if o.next <= MAX_SEQ => OpenStream {
                last_ms: o.last_ms.max(commit_ms),
                ..o
            },
            _ => OpenStream {
                stream: st.next_stream(),
                next: 0,
                opened_ms: commit_ms,
                last_ms: commit_ms,
            },
        };
        if st.open.len() >= st.open_limit && !st.open.contains_key(&slot) {
            st.open.pop_first();
        }
        st.open.insert(
            slot,
            OpenStream {
                next: open.next + 1,
                ..open
            },
        );
        Minted {
            id: RunId::new(open.last_ms, open.stream, open.next),
            opened_stream: open.next == 0,
            stream_opened_ms: open.opened_ms,
        }
    }

    /// Forgets the open stream of every invocation whose stream day is before `day`, so
    /// the map holds today's invocations only.
    pub fn forget_before(&self, day: Day) {
        self.state().open.retain(|(_, d), _| *d >= day);
    }

    /// Forgets `inv`'s open streams: its next run opens a new stream.
    pub fn close(&self, inv: &InvKey) {
        self.state().open.retain(|(i, _), _| i != inv);
    }

    /// Whether the writer should reserve a block: there is no spare and the current
    /// block is gone or at least half used.
    #[must_use]
    pub fn needs_block(&self) -> bool {
        let st = self.state();
        st.spare.is_none()
            && st
                .current
                .is_none_or(|(next, end)| end - next <= BLOCK_SIZE / 2)
    }

    /// Takes the `ui/streams` counter value `value` as a reserved block. A value past
    /// the last block (the counter has run through every id below the fallback range)
    /// is refused, and minting stays on the fallback range.
    pub fn add_block(&self, value: u64) -> bool {
        if value == 0 || value >= BLOCKS {
            return false;
        }
        let mut st = self.state();
        // `value < BLOCKS`, so the block's first id is below the fallback range.
        let start = value as u32 * BLOCK_SIZE;
        if st.current.is_none() {
            st.current = Some((start, start + BLOCK_SIZE));
        } else {
            st.spare = Some(start);
        }
        true
    }
}

impl MintState {
    fn next_stream(&mut self) -> StreamId {
        if let Some((next, end)) = self.current {
            let rest = next + 1;
            self.current = if rest < end {
                Some((rest, end))
            } else {
                self.spare.take().map(|s| (s, s + BLOCK_SIZE))
            };
            return StreamId(next);
        }
        let low = self.fallback;
        self.fallback = low.wrapping_add(1);
        StreamId(FALLBACK_BASE | (self.frontend << 16) | u32::from(low))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(n: u8) -> InvKey {
        InvKey::from_record_id(&format!("build-{n}"))
    }

    const T: u64 = 1_792_108_800_000; // 2026-10-16T00:00:00Z

    #[test]
    fn run_ids_order_by_time_then_stream_then_seq() {
        // Catches: a little-endian or reordered byte layout, under which a later run
        // could sort before an earlier one and pages by id would be out of time order.
        let a = RunId::new(T, StreamId(0xFF_FFFF), 0xFF_FFFF);
        let b = RunId::new(T + 1, StreamId(1), 0);
        assert!(a.to_bytes() < b.to_bytes());
        assert!(a.to_string() < b.to_string());
        assert!(a < b);
        let c = RunId::new(T, StreamId(2), 1);
        let d = RunId::new(T, StreamId(2), 2);
        assert!(c.to_string() < d.to_string());
    }

    #[test]
    fn run_ids_round_trip_their_text_and_bytes() {
        // Catches: a part that does not survive encoding, or text of the wrong width.
        let id = RunId::new(T + 5, StreamId(0x80_1234), 77);
        let text = id.to_string();
        assert_eq!(text.len(), 24);
        assert_eq!(text.parse::<RunId>(), Ok(id));
        assert_eq!(RunId::from_bytes(id.to_bytes()), id);
        assert_eq!(
            (id.time_ms(), id.stream(), id.seq()),
            (T + 5, StreamId(0x80_1234), 77)
        );
        assert_eq!(id.day().to_string(), "20261016");
        for bad in ["", "zz", &"0".repeat(23), &"0".repeat(26)] {
            let e = bad.parse::<RunId>().expect_err(bad);
            assert!(e.to_string().contains("not a run id"));
        }
        // Parts are cut to their widths.
        let wide = RunId::new(u64::MAX, StreamId(u32::MAX), u32::MAX);
        assert_eq!(wide.to_string(), "f".repeat(24));
    }

    #[test]
    fn without_a_block_ids_come_from_the_fallback_range_and_repeat_after_restart() {
        // Catches: a minter that waits for (or invents) a block, and a fallback range
        // that overlaps the block range or drops the frontend index.
        let m = Minter::new(0x85); // only the low 7 bits count
        let a = m.mint(&inv(1), T);
        assert!(a.opened_stream);
        assert!(a.id.stream().is_fallback());
        assert_eq!(a.id.stream(), StreamId(0x80_0000 | (0x05 << 16)));
        let b = m.mint(&inv(1), T + 1);
        assert_eq!(
            (b.id.stream(), b.id.seq(), b.opened_stream),
            (a.id.stream(), 1, false)
        );
        assert_ne!(m.mint(&inv(2), T).id.stream(), a.id.stream());
        // A restart: a new minter, no block yet, hands out the same stream id again.
        let restarted = Minter::new(0x85);
        assert_eq!(restarted.mint(&inv(9), T + 10).id.stream(), a.id.stream());
    }

    #[test]
    fn blocks_are_used_in_order_and_refilled_at_half() {
        // Catches: a refill asked too late (minting falls to the fallback range while a
        // block could have been ready), a spare block skipped, or block 0 handed out.
        let m = Minter::new(0);
        assert!(m.needs_block());
        assert!(!m.add_block(0));
        assert!(m.add_block(1));
        let first = m.mint(&inv(0), T).id.stream();
        assert_eq!(first, StreamId(BLOCK_SIZE));
        assert!(!first.is_fallback());
        for n in 1..BLOCK_SIZE / 2 - 1 {
            m.mint(&InvKey::from_record_id(&n.to_string()), T);
            assert!(!m.needs_block(), "asked at {n}");
        }
        m.mint(&inv(1), T);
        assert!(m.needs_block());
        assert!(m.add_block(5));
        assert!(!m.needs_block());
        for n in 0..BLOCK_SIZE / 2 {
            m.mint(&InvKey::from_record_id(&format!("rest-{n}")), T);
        }
        // The current block is spent: the next stream is the spare's first id.
        assert_eq!(m.mint(&inv(2), T).id.stream(), StreamId(5 * BLOCK_SIZE));
        // Past the last block below the fallback range, a value is refused.
        assert!(!m.add_block(BLOCKS));
        assert!(m.add_block(BLOCKS - 1));
    }

    #[test]
    fn a_spent_block_with_no_spare_falls_back() {
        // Catches: a minter that reuses a spent block's last id when no spare is ready.
        let m = Minter::new(1);
        assert!(m.add_block(2));
        for n in 0..BLOCK_SIZE {
            m.mint(&InvKey::from_record_id(&n.to_string()), T);
        }
        assert!(m.mint(&inv(3), T).id.stream().is_fallback());
    }

    #[test]
    fn a_new_day_or_a_full_stream_or_a_close_opens_a_new_stream() {
        // Catches: a stream that spans two days (its row would be under one day only,
        // so the other day's runs would not resolve), a sequence that wraps into an
        // earlier run's id, or a closed invocation that keeps its stream.
        let m = Minter::new(0);
        let a = m.mint(&inv(1), T - 1).id;
        let b = m.mint(&inv(1), T).id;
        assert_ne!(a.stream(), b.stream());
        assert_eq!(b.seq(), 0);
        m.close(&inv(1));
        let c = m.mint(&inv(1), T + 1);
        assert!(c.opened_stream);
        assert_ne!(c.id.stream(), b.stream());
        {
            let mut st = m.state();
            let slot = (inv(1), Day::from_ms(T));
            let open = st.open.get_mut(&slot).unwrap();
            open.next = MAX_SEQ;
        }
        let last = m.mint(&inv(1), T + 2).id;
        assert_eq!((last.stream(), last.seq()), (c.id.stream(), MAX_SEQ));
        let next = m.mint(&inv(1), T + 3);
        assert!(next.opened_stream);
        assert_ne!(next.id.stream(), c.id.stream());
        // Forgetting the days before today drops yesterday's slots only.
        m.mint(&inv(2), T - 1);
        assert_eq!(m.state().open.len(), 2);
        m.forget_before(Day::from_ms(T));
        let st = m.state();
        assert!(st.open.keys().all(|(_, d)| *d == Day::from_ms(T)));
        assert_eq!(st.open.len(), 1);
    }

    #[test]
    fn fallback_ids_wrap_within_their_range() {
        // Catches: a fallback counter that carries into the frontend index bits.
        let m = Minter::new(0);
        m.state().fallback = u16::MAX;
        assert_eq!(m.mint(&inv(1), T).id.stream(), StreamId(0x80_FFFF));
        assert_eq!(m.mint(&inv(2), T).id.stream(), StreamId(0x80_0000));
    }

    #[test]
    fn a_stream_keeps_its_open_time_and_its_ids_never_go_back_in_time() {
        // Catches: a run id whose time is earlier than its stream's previous id (a
        // commit time read before the lock, minted after a later one), which would sort
        // seq 1 before seq 0 and fall before the stream's row; and an open time that
        // is not the stream's first id's.
        let m = Minter::new(0);
        let a = m.mint(&inv(1), T + 5);
        assert_eq!((a.id.time_ms(), a.stream_opened_ms), (T + 5, T + 5));
        let b = m.mint(&inv(1), T);
        assert_eq!(
            (b.id.seq(), b.id.time_ms()),
            (1, T + 5),
            "raised to the newest"
        );
        assert!(a.id < b.id);
        let c = m.mint(&inv(1), T + 9);
        assert_eq!((c.id.time_ms(), c.stream_opened_ms), (T + 9, T + 5));
        // A new stream opens at its own first id's time.
        let d = m.mint(&inv(2), T + 1);
        assert_eq!((d.id.time_ms(), d.stream_opened_ms), (T + 1, T + 1));
    }

    #[test]
    fn open_streams_are_bounded_and_a_forgotten_one_reopens() {
        // Catches: one open stream kept per client invocation id until the day ends,
        // so a client sending fresh ids grows the minter without bound.
        let m = Minter::new(0);
        m.state().open_limit = 2;
        m.mint(&inv(1), T);
        m.mint(&inv(2), T);
        // A stream already held takes no room.
        assert_eq!(m.mint(&inv(2), T).id.seq(), 1);
        assert_eq!(m.state().open.len(), 2);
        m.mint(&inv(3), T);
        let held: Vec<InvKey> = m.state().open.keys().map(|(i, _)| i.clone()).collect();
        assert_eq!(held.len(), 2);
        assert!(held.contains(&inv(3)));
        let gone: Vec<InvKey> = [inv(1), inv(2)]
            .into_iter()
            .filter(|i| !held.contains(i))
            .collect();
        assert_eq!(gone.len(), 1);
        assert!(
            m.mint(&gone[0], T).opened_stream,
            "the forgotten one opens anew"
        );
    }

    #[test]
    fn a_poisoned_minter_still_mints() {
        // Catches: a panic on another thread holding the lock turning every later
        // `Result` commit into a panic.
        let m = std::sync::Arc::new(Minter::new(0));
        let held = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = held.state();
            panic!("poison");
        })
        .join();
        assert!(m.mint(&inv(1), T).opened_stream);
    }
}

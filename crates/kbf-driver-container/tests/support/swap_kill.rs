//! What a swap-watch kill (`SwapKill`) promises, checked the same way against the stand-in
//! `crun` and against a real kernel: the outcome and the watch's own log line.

use std::cell::RefCell;
use std::fmt::Debug;
use std::io::Write;

use kbf_daemon::RuntimeError;
use tracing_subscriber::fmt::TestWriter;

thread_local! {
    /// Where this thread's log lines go while a [`Logs`] lives on it.
    static CAPTURE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// This thread's capture if it has one, else the test's output.
struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let captured = CAPTURE.with_borrow_mut(|c| c.as_mut().map(|c| c.extend_from_slice(buf)));
        match captured {
            Some(()) => Ok(buf.len()),
            None => TestWriter::new().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Shows the driver's logs in a failing test's output, and lets [`Logs`] read them.
/// One global subscriber with a per-thread sink, not a subscriber per test: with a
/// per-thread default subscriber (`tracing`'s `set_default`), the fake kill test saw
/// its kill but none of the kill's log line whenever it ran beside the other tests.
pub fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(|| Sink)
        .try_init();
}

/// The driver's log lines logged on this thread while this lives. A `#[tokio::test]`
/// runs every task on its one thread, so this holds what the test's runs logged. Those
/// lines go nowhere else meanwhile: put [`Logs::text`] in a failure message.
pub struct Logs(());

impl Logs {
    pub fn capture() -> Self {
        trace();
        CAPTURE.set(Some(Vec::new()));
        Self(())
    }

    pub fn text(&self) -> String {
        CAPTURE
            .with_borrow(|c| String::from_utf8_lossy(c.as_deref().unwrap_or_default()).into_owned())
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        CAPTURE.set(None);
    }
}

/// What the watch's kill line says: the lease's own swap, all of its swap, and the
/// threshold it passed, in that order.
fn kill_line(line: &str) -> Option<(u64, u64, u64)> {
    let rest = line.split_once("still pushing into swap: ")?.1;
    let (own, rest) = rest.split_once(" of its ")?;
    let (swap, rest) = rest.split_once(" bytes in swap")?;
    let threshold = rest.split_once("(more than ")?.1.split_once(')')?.0;
    Some((
        own.parse().ok()?,
        swap.parse().ok()?,
        threshold.parse().ok()?,
    ))
}

/// Asserts that `outcome` is the swap watch's kill of a lease capped at `cap` whose
/// watch kills past `threshold` bytes of the lease's own swap, with `logs` what the
/// driver logged meanwhile and `context` more for the failure message:
///
/// - `OutOfMemory` with `limit` the cap and `used` (RAM plus swap at the sample the
///   watch killed at) past the cap;
/// - exactly one kill line, saying the lease's own swap was past `threshold`, the
///   threshold it was given, and own swap within its swap, and its swap within `used`.
///
/// Not `used` past the cap by the threshold: by the sample the watch kills at, reclaim
/// at the cap may have left the lease's RAM below the cap, so RAM plus swap can be
/// short of cap + threshold while the lease's own swap is past the threshold (crun-arm
/// once killed at 478 MB of RAM under a 562 MB cap plus 146 MB of its own swap).
pub fn assert_swap_kill<T: Debug>(
    outcome: &Result<T, RuntimeError>,
    logs: &str,
    cap: u64,
    threshold: u64,
    context: &str,
) {
    let why = format!("{outcome:?}\nlogs:\n{logs}\n{context}");
    let Err(RuntimeError::OutOfMemory { used, limit }) = *outcome else {
        panic!("not the swap watch's out-of-memory kill: {why}");
    };
    assert_eq!(limit, cap, "the limit is not the lease's cap: {why}");
    assert!(used > cap, "killed while not past its cap: {why}");
    let kills: Vec<_> = logs.lines().filter_map(kill_line).collect();
    let [(own, swap, logged)] = kills[..] else {
        panic!("not one kill line from the swap watch: {kills:?}: {why}");
    };
    assert_eq!(logged, threshold, "the watch's threshold: {why}");
    assert!(own > threshold, "killed at or below the threshold: {why}");
    assert!(own <= swap && swap <= used, "own swap, swap, used: {why}");
}

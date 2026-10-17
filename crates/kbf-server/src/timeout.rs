//! A timed-out action's partial result as the server answers it (issue #45). REAPI
//! lets a server return what an action wrote before its timeout stopped it, with
//! `DEADLINE_EXCEEDED`. The daemon reports that as an `action_result` beside the
//! status; the server keeps only its stdout and stderr digests (an output of a
//! stopped action may be half written, and its exit code is no exit), and only those
//! whose blobs the CAS holds, so the client can fetch every digest it is given. The
//! partial result is never written to the action cache.

use kbf_front::{Backend, digest_from_proto};
use kbf_proto::reapi::{ActionResult, Digest};

/// The partial result to answer a timed-out action with, from what the daemon
/// `reported`: its stdout and stderr digests that the CAS holds, nothing else. `None`
/// when neither is held, or the CAS cannot say (the timeout is still answered).
pub(crate) async fn partial(cache: &Backend, reported: ActionResult) -> Option<ActionResult> {
    let logs = [reported.stdout_digest, reported.stderr_digest];
    let digests: Vec<_> = logs
        .iter()
        .flatten()
        .filter_map(|d| digest_from_proto(Some(d)).ok())
        .collect();
    // A CAS that cannot say leaves the partial result out; the timeout is answered.
    let missing = cache.find_missing(&digests).await.ok()?;
    let held = |log: Option<Digest>| {
        log.filter(|d| digest_from_proto(Some(d)).is_ok_and(|d| !missing.contains(&d)))
    };
    let [stdout, stderr] = logs;
    let partial = ActionResult {
        stdout_digest: held(stdout),
        stderr_digest: held(stderr),
        ..ActionResult::default()
    };
    (partial.stdout_digest.is_some() || partial.stderr_digest.is_some()).then_some(partial)
}

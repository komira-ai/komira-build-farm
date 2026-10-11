//! What the server logs when a daemon reports a lease failed for the farm's reason
//! (INTERNAL): the client is told only that the farm could not run the action, so the
//! daemon's reason must reach the server's log, or nobody can tell a missing image
//! from a full disk. A module of the `execute` test binary, which installs one log
//! capture for all its tests (`memory.rs`).

use crate::memory::{capture, lines_with};
use crate::support::{Cell, Job, done, failed, response};
use tonic::Code;

/// Catches: a farm failure (`INTERNAL` from the daemon, such as the container
/// driver's "image ... is not in this node's image store") logged without the daemon's
/// message or without the node, so the only record of why is lost; and the reason
/// leaked to the client in place of the farm's own words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_farm_failure_logs_the_daemons_reason_and_node() {
    capture();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("imageless-node", 4, 8).await;
    let job = Job::new("needs an image", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let why = "image docker://registry.test/base@sha256:00 is not in this node's image store";
    let mut report = failed(start.lease_id, Code::Internal);
    report.status.as_mut().expect("a status").message = why.to_owned();
    assert!(daemon.report(report).await.accepted);

    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");
    assert_eq!(status.message, "the farm could not run the action");
    let logged = lines_with(&["lease failed", "node=imageless-node", why, "WARN"]);
    assert_eq!(logged.len(), 1, "{logged:?}");
}

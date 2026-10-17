//! [`replicate`], [`stopping`] and the repair worker's stop, over an in-memory cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_mapsync::MapHolder;
use kbf_storage::Storage;
use kbf_storage::repair::{JOURNAL, JournalError, RepairConfig};
use kbf_storage::service::{Admission, Config, StorageService};
use tonic::transport::ClientTlsConfig;

use super::{ReplicationError, replicate, run_repairs, stopping};
use crate::node_map::NodeMap;

/// A deadline that only ends a hang.
const HANG: Duration = Duration::from_secs(60);

fn storage() -> Arc<dyn Storage> {
    Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ))
}

fn service(admission: Admission) -> StorageService {
    StorageService::new(
        storage(),
        admission,
        Config {
            write_bytes_in_flight: 1 << 20,
        },
    )
}

fn node() -> Arc<NodeMap> {
    Arc::new(NodeMap::new(Arc::new(MapHolder::new())))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-replication-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn replicated(
    admission: Admission,
    tls: Option<ClientTlsConfig>,
    dir: &Path,
) -> Result<super::Replicated, ReplicationError> {
    replicate(
        service(admission),
        storage(),
        &node(),
        tls,
        dir,
        RepairConfig::default(),
    )
}

/// Catches a node admitted by certificates that is not a link of the chain: it keeps a
/// repair journal in the directory it was given and has a worker for it. Mutant: the TLS
/// case taking the plain-text arm (no journal, no worker).
#[tokio::test]
async fn a_node_with_a_certificate_keeps_a_repair_journal_and_has_a_worker() {
    let dir = scratch("chain").join(super::REPAIR_DIR);
    let farm = replicated(Admission::Certified, Some(ClientTlsConfig::new()), &dir)
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(farm.worker.is_some());
    let repairs = farm.repairs.expect("a journal");
    assert!(repairs.open_records().is_empty());
    assert!(
        dir.join(JOURNAL).is_file(),
        "the journal is in its directory"
    );
}

/// Catches a node that admits callers in plain text copying blobs (a call over plain text
/// carries no role, so a node's copy cannot be told from a client's) or opening a journal
/// it has no use for: it judges by the map and makes no directory. Mutant: the plain-text
/// case building a chain (the service refuses it, and the directory is made).
#[tokio::test]
async fn a_plain_text_node_judges_by_the_map_and_keeps_no_journal() {
    let dir = scratch("plain").join(super::REPAIR_DIR);
    let farm = replicated(Admission::Open, None, &dir).unwrap_or_else(|e| panic!("{e}"));
    assert!(farm.worker.is_none() && farm.repairs.is_none());
    assert!(!dir.exists());
}

/// Catches a node that starts without a place to keep what it owes: the directory cannot be
/// made (something that is not a directory is in the way), and the node does not start.
#[tokio::test]
async fn a_node_whose_journal_directory_cannot_be_made_does_not_start() {
    let parent = scratch("dir");
    let file = parent.join("in-the-way");
    std::fs::write(&file, b"a file").expect("write");
    let dir = file.join(super::REPAIR_DIR);
    let error = replicated(Admission::Certified, Some(ClientTlsConfig::new()), &dir)
        .err()
        .expect("refused");
    assert!(
        matches!(&error, ReplicationError::Dir { dir: at, .. } if *at == dir),
        "{error}"
    );
    assert!(error.to_string().contains("repair journal directory"));
}

/// Catches a node that opens a damaged journal: guessing past damage could forget a debt,
/// so the node does not start, and says the journal is the reason.
#[tokio::test]
async fn a_node_whose_journal_is_damaged_does_not_start() {
    let dir = scratch("damaged").join(super::REPAIR_DIR);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join(JOURNAL), b"not a repair journal at all").expect("write");
    let error = replicated(Admission::Certified, Some(ClientTlsConfig::new()), &dir)
        .err()
        .expect("refused");
    assert!(
        matches!(
            error,
            ReplicationError::Journal(JournalError::Corrupt { .. })
        ),
        "{error}"
    );
}

/// Catches a chain node that takes callers it cannot tell apart: a certificate for its
/// peers with a service that admits every caller is refused by the service, and so by the
/// node.
#[tokio::test]
async fn a_chain_over_a_service_that_admits_every_caller_is_refused() {
    let dir = scratch("open-chain").join(super::REPAIR_DIR);
    let error = replicated(Admission::Open, Some(ClientTlsConfig::new()), &dir)
        .err()
        .expect("refused");
    assert!(matches!(error, ReplicationError::Service(_)), "{error}");
}

/// Catches a stop that fires before the node is told to shut down, and one that never fires
/// after: `stop` is pending while `shutdown` is, and resolves once `shutdown` has, which
/// still resolves as it did.
#[tokio::test]
async fn the_stop_follows_the_shutdown() {
    let (told, shutdown) = tokio::sync::oneshot::channel::<()>();
    let (shutdown, stop) = stopping(Box::pin(async move {
        let _ = shutdown.await;
    }));
    let mut stop = stop;
    assert!(stop.as_mut().now_or_never().is_none(), "not yet");
    told.send(()).expect("sent");
    shutdown.await;
    tokio::time::timeout(HANG, stop)
        .await
        .expect("the stop follows the shutdown");
}

/// Catches a stop that waits for ever when the node ends without being told to: dropping
/// the shutdown future (the listener failed) ends the stop too.
#[tokio::test]
async fn the_stop_follows_a_shutdown_that_is_dropped() {
    let (shutdown, stop) = stopping(Box::pin(std::future::pending()));
    drop(shutdown);
    tokio::time::timeout(HANG, stop)
        .await
        .expect("the stop ends with the shutdown");
}

/// Catches a repair worker that outlives the node's shutdown: the task `bind_node` starts
/// ends when `stop` resolves. Mutant: the worker run with a stop that never resolves (the
/// join never ends).
#[tokio::test]
async fn the_repair_worker_ends_when_it_is_stopped() {
    let dir = scratch("worker").join(super::REPAIR_DIR);
    let worker = replicated(Admission::Certified, Some(ClientTlsConfig::new()), &dir)
        .unwrap_or_else(|e| panic!("{e}"))
        .worker
        .expect("a worker");
    let (told, stop) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(run_repairs(
        worker,
        Box::pin(async move {
            let _ = stop.await;
        }),
    ));
    told.send(()).expect("sent");
    tokio::time::timeout(HANG, running)
        .await
        .expect("the worker ends")
        .expect("without a panic");
}

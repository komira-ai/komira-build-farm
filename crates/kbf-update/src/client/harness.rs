//! What the client's tests share: a client over a scratch directory and the fake release
//! service, with helpers to drive and read it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::*;
use crate::fixtures::{FakeSource, FakeSpace, PLATFORM, offer_of, scratch};
use crate::helper::SCHEMA;

/// How long a test waits for something that is about to happen. Not a measurement: only
/// a hung test is cut off by it.
pub(super) const PROMPT: Duration = Duration::from_secs(30);

pub(super) struct Rig {
    pub(super) handle: Handle,
    pub(super) rx: watch::Receiver<Status>,
    pub(super) task: JoinHandle<()>,
    pub(super) dir: PathBuf,
    pub(super) source: Arc<FakeSource>,
    pub(super) space: Arc<FakeSpace>,
}

impl Rig {
    /// A client over a host whose helper runs and has nothing to report.
    pub(super) fn new(name: &str) -> Self {
        let rig = Self::bare(name);
        rig.write_status(&status_json("idle", "", "", 0));
        rig
    }

    /// A client over a host with no status file.
    pub(super) fn bare(name: &str) -> Self {
        Self::over(&scratch(name), FakeSource::new(100))
    }

    pub(super) fn config(dir: &Path, space: &Arc<FakeSpace>) -> Config {
        let mut config = Config::new(dir.to_owned(), PLATFORM.to_owned(), "f".repeat(12));
        config.poll_every = Duration::from_secs(3600);
        config.backoff = Backoff {
            first: Duration::from_millis(1),
            max: Duration::from_millis(4),
        };
        config.space = Arc::clone(space) as Arc<dyn FreeSpace + Send + Sync>;
        config
    }

    pub(super) fn over(dir: &Path, source: Arc<FakeSource>) -> Self {
        let space = FakeSpace::plenty();
        let (handle, worker) = start(
            Self::config(dir, &space),
            Arc::clone(&source) as Arc<dyn Source>,
        )
        .expect("start");
        Self {
            rx: handle.status(),
            handle,
            task: tokio::spawn(worker.run()),
            dir: dir.to_owned(),
            source,
            space,
        }
    }

    /// A new client over the same directory and release service, as after a restart.
    pub(super) fn restart(self) -> Self {
        self.task.abort();
        Self::over(&self.dir, self.source)
    }

    pub(super) async fn until(&mut self, pred: impl Fn(&Status) -> bool) -> Status {
        tokio::time::timeout(PROMPT, self.rx.wait_for(|s| pred(s)))
            .await
            .expect("the status changed in time")
            .expect("the client runs")
            .clone()
    }

    pub(super) fn inbox(&self) -> PathBuf {
        self.dir.join("inbox")
    }

    /// The names in the inbox other than the orders directory, sorted.
    pub(super) fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.inbox())
            .expect("inbox")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n != "orders")
            .collect();
        names.sort();
        names
    }

    pub(super) fn write_status(&self, json: &str) {
        fs::write(self.dir.join("status.json"), json).expect("write status.json");
        self.handle.refresh();
    }

    /// Serves `data` and offers it as `generation`.
    pub(super) fn offer(&self, generation: u64, data: &[u8]) -> Offer {
        self.source.serve(data);
        let offer = offer_of(generation, data);
        self.handle.offer(offer.clone());
        offer
    }

    pub(super) fn orders(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.inbox().join("orders"))
            .expect("orders")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

pub(super) fn status_json(state: &str, installed: &str, staged: &str, acked: u64) -> String {
    let quote = |c: &str| {
        if c.is_empty() {
            "null".to_owned()
        } else {
            format!("\"{c}\"")
        }
    };
    format!(
        "{{\"schema\":{SCHEMA},\"installed_commit\":{},\"previous_commit\":null,\
         \"staged_commit\":{},\"state\":\"{state}\",\"refusal\":\"why not\",\
         \"acked_order_id\":{acked},\"staged\":{{\"protocol\":2,\"min_protocol\":1,\
         \"state_schema\":3}}}}",
        quote(installed),
        quote(staged)
    )
}

pub(super) fn order(id: u64, verb: Verb, commit: &str) -> Order {
    Order {
        order_id: id,
        epoch: 7,
        verb: Some(verb),
        commit: commit.to_owned(),
        window_s: 600,
    }
}

pub(super) const NO_LEASES: &dyn Fn() -> usize = &|| 0;

/// The status of a node that has acted on `generation` and is in `state`.
pub(super) fn at(generation: u64, state: State) -> impl Fn(&Status) -> bool {
    move |s| (s.offered_generation, s.state) == (generation, state)
}

impl Rig {
    /// Asks for order `id` of `verb` for `commit` with no lease live.
    pub(super) fn ask(&self, id: u64, verb: Verb, commit: &str) -> Decision {
        self.handle.order(&order(id, verb, commit), 7, NO_LEASES)
    }
}

/// The reason a decision refused, or "" when it did not.
pub(super) fn why(decision: &Decision) -> &str {
    decision.refusal().unwrap_or_default()
}

/// A rig whose helper holds `staged` as staged and `installed` as installed.
pub(super) fn rig_with_helper(name: &str, installed: &str, staged: &str) -> Rig {
    let rig = Rig::bare(name);
    rig.write_status(&status_json("staged", installed, staged, 0));
    rig
}

//! The stop signal, and the rig the tests of the parts of a handover build a serving
//! frontend from.

use std::net::{SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::{AbortHandle, AbortRegistration};
use kbf_front::{Backend, Cache, Described, Dispatch, Submission};
use kbf_sched::Request;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};
use tonic::Status;

use super::{Facts, Holder, Sockets, Stand, Stop};
use crate::drain::Drain;
use crate::farm::{EpochPolicy, Farm};
use crate::health::Readiness;
use crate::state_store::StateStore;

/// A directory of the test's own, removed when dropped.
pub(super) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("kbf-handover-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }

    /// Where a control socket is made.
    pub fn socket(&self) -> PathBuf {
        self.0.join("control.sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A state store over the fake that counts the writer's role being given up, can refuse
/// to give it up, can have its reads fail, and runs a hook when it is given up.
pub(super) struct Gives {
    inner: Fake,
    pub demoted: AtomicUsize,
    pub refuses_to_demote: AtomicBool,
    pub reads_fail: AtomicBool,
    pub reads_failed: AtomicUsize,
    pub reads_served: AtomicUsize,
    hook: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Gives {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            demoted: AtomicUsize::new(0),
            refuses_to_demote: AtomicBool::new(false),
            reads_fail: AtomicBool::new(false),
            reads_failed: AtomicUsize::new(0),
            reads_served: AtomicUsize::new(0),
            hook: Mutex::new(None),
        })
    }

    /// Runs `hook` each time the writer role is given up, just before.
    pub fn on_demote(&self, hook: impl Fn() + Send + 'static) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    pub fn put(&self, key: &str, value: Vec<u8>) {
        let put = Txn::new().put(key, value, kbf_state::Cond::Any);
        self.inner.txn(put).expect("store").expect("put");
    }
}

impl Store for Gives {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.reads_fail.load(Ordering::SeqCst) {
            self.reads_failed.fetch_add(1, Ordering::SeqCst);
            return Err(StoreError::Failed("the store cannot be read".to_owned()));
        }
        self.reads_served.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }

    fn demote(&self) -> Result<(), StoreError> {
        if let Some(hook) = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            hook();
        }
        self.demoted.fetch_add(1, Ordering::SeqCst);
        if self.refuses_to_demote.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the writer cannot stop".to_owned()));
        }
        Ok(())
    }
}

/// A request to run, distinguished by `tag`.
pub(super) fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// Queues `request` on `farm`.
pub(super) fn submit(farm: &Farm, request: Request) -> Result<kbf_front::Ticket, Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
    })
}

/// A serving frontend's parts, as a holder stands them down: a farm over a store, a drain,
/// the stops of the worker and operator listeners, the task that expires rows, and the
/// three listeners.
pub(super) struct Rig {
    pub dir: Scratch,
    pub store: Arc<Gives>,
    pub backend: Backend,
    pub farm: Arc<Farm>,
    pub drain: Arc<Drain>,
    pub worker: Stop,
    pub api: Stop,
    pub sweeper: AbortHandle,
    /// Kept so the sweeper is not aborted before it is told to be.
    pub _sweeping: AbortRegistration,
    listeners: [TcpListener; 3],
    pub facts: Facts,
}

impl Rig {
    pub fn new() -> Self {
        let store = Gives::new();
        // A store opened for a writer holds a generation.
        store.put(kbf_state::GENERATION, 7_u64.to_be_bytes().to_vec());
        let backend = Backend::from(Arc::new(Cache::memory()));
        let farm = Farm::new(
            backend.clone(),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: store.clone(),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
        let (closer, _closing) = kbf_front::closing();
        let (drain, _goaway) = Drain::new(closer, Arc::new(Readiness::default()));
        let listeners = [(); 3].map(|()| TcpListener::bind("127.0.0.1:0").expect("bind"));
        let [reapi, worker, api] = [0, 1, 2].map(|i| listeners[i].local_addr().expect("address"));
        let (sweeper, _sweeping) = AbortHandle::new_pair();
        Self {
            dir: Scratch::new(),
            store,
            backend,
            farm: Arc::new(farm),
            drain: Arc::new(drain),
            worker: Stop::new(),
            api: Stop::new(),
            sweeper,
            _sweeping,
            listeners,
            facts: Facts {
                reapi,
                worker,
                api: Some(api),
                state: PathBuf::from("/var/lib/kbf/state.db"),
            },
        }
    }

    /// Second handles on the three listeners, as a holder offers them.
    pub fn sockets(&self) -> Sockets {
        let dup = |i: usize| OwnedFd::from(self.listeners[i].try_clone().expect("a second handle"));
        Sockets {
            reapi: dup(0),
            worker: dup(1),
            api: Some(dup(2)),
        }
    }

    pub fn stand(&self, wait: Duration) -> Stand {
        Stand {
            drain: Arc::clone(&self.drain),
            farm: Arc::clone(&self.farm),
            worker: self.worker.clone(),
            api: self.api.clone(),
            sweeper: self.sweeper.clone(),
            store: self.store.clone(),
            wait,
        }
    }

    /// A holder on the rig's control socket for peers running as `uid`.
    pub fn holder(&self, uid: u32, wait: Duration) -> Holder {
        Holder::bind(
            &self.dir.socket(),
            uid,
            self.facts.clone(),
            self.sockets(),
            self.stand(wait),
        )
        .expect("a control socket")
    }

    /// The user the tests run as.
    pub fn uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    pub fn addr(&self, i: usize) -> SocketAddr {
        self.listeners[i].local_addr().expect("address")
    }
}

/// Catches: a stop that completes before it is fired (a listener would close while the
/// frontend serves), one that never completes once fired (a listener that is told to stop
/// and does not), a clone that does not share the signal, and one fired before anything
/// waits that is lost.
#[tokio::test]
async fn a_stop_completes_for_every_waiter_once_it_is_fired() {
    let stop = Stop::new();
    let clone = stop.clone();
    assert!(stop.fired().now_or_never().is_none(), "fired before it was");
    let waiting = tokio::spawn(stop.fired());
    clone.fire();
    waiting.await.expect("the waiter completes");
    assert!(stop.fired().now_or_never().is_some(), "a late waiter waits");
    assert!(clone.fired().now_or_never().is_some());
}

/// Catches: a stop that closes its listeners late (when the server next polls them: a
/// listener asleep in accept keeps its socket, and with it the address), and one that
/// does not close a listener registered after it was given.
#[tokio::test]
async fn a_stop_closes_the_listeners_registered_with_it_when_it_is_given() {
    let any = || "127.0.0.1:0".parse().expect("address");
    let stop = Stop::new();
    let socket = crate::listen::Socket::bind(any(), false).expect("bind");
    let addr = socket.local;
    let (incoming, latch) = socket.grpc(None).expect("registered");
    stop.closes(latch);
    assert!(
        std::net::TcpStream::connect(addr).is_ok(),
        "closed before the stop"
    );
    stop.fire();
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "still open after the stop"
    );
    drop(incoming);

    // Registered after the stop was given: closed at once.
    let socket = crate::listen::Socket::bind(any(), false).expect("bind");
    let addr = socket.local;
    let (_incoming, latch) = socket.grpc(None).expect("registered");
    stop.closes(latch);
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "a late listener stays open"
    );
}

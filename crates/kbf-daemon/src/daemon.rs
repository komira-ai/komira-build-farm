//! The daemon's main loop: one outbound mutual-TLS stream at a time to a server,
//! reconnecting when it ends, with the lease manager and the fence clock running
//! throughout, connected or not. Which server it tries next, and how long it waits
//! between rounds of attempts, is [`crate::connect`]'s: it never stops trying.
//!
//! A session sends Hello, which names this daemon process by an instance id drawn when
//! the daemon starts and the same on every stream (issue #140: the scheduler gives up at
//! once only the leases of the same process that a new stream leaves out), waits for
//! Welcome, then sends a Heartbeat at the interval Welcome names and handles what the
//! server sends: `HeartbeatAck` renews contact,
//! `LeaseOffer` is logged and runs nothing, `Start` runs a lease if it arrived within
//! the window it names (see `window`; a late one is dropped without a Result), on the
//! one runtime of the daemon's [`Runtimes`] that serves its kind (none: refused), and
//! `Cancel` kills a running lease. Results go out on the stream. Every Result is kept
//! until the server's `ResultAck` names its lease (issue #26): until then each
//! Heartbeat lists the lease in `running`, so the scheduler does not take it as lost,
//! and each new stream resends it right after Welcome, before its first Heartbeat. A
//! Result produced while disconnected is sent the same way. After those Results and
//! before the first Heartbeat, each stream sends the node's software status
//! (`NodeStatus`, see [`crate::status`]).
//!
//! A runtime that switches a Mac's GUI session for a whole-machine lease hands the
//! daemon its phase ([`Daemon::with_phase`]; `docs/design/worker-protocol.md#node-phases`):
//! every Hello carries the newest, and a change resends the Hello on the stream. A
//! Welcome that names another lease epoch than the switching lease's term, or a
//! `Cancel` for a lease that is not running here, has every runtime kill that lease,
//! so a runtime that keeps a lease across its own restart (the whole-machine runtime
//! does) cleans up after one the server no longer holds.
//!
//! A driver whose report changes while the daemon runs (the native driver re-checks its
//! Xcodes, issue #164) hands the daemon a [`DriverReport`] channel
//! ([`Daemon::with_driver_report`]); a driver with more than one survey sends each
//! through its own part of one [`crate::DriverWatch`], which merges them, so no survey
//! overwrites another's. Its entries join the report the daemon was started
//! with, and its Xcodes go into `NodeStatus`. Each change is taken when it arrives: when
//! the report changed, the Hello is resent on the stream (the server then places by the
//! new report, issue #25), and `NodeStatus` is sent again either way. A change while no
//! stream is up is taken by the next stream's Hello.
//!
//! Each lease is remembered with the server's lease epoch at its Start and the action
//! the Start named (issue #137). Every Result echoes that action, so the server can
//! refuse one that answers another operation. A Welcome that names another epoch
//! comes from a server that never granted those leases and will never accept their
//! Results: before anything else on that stream, the daemon kills their runs and
//! forgets their Results, unsent. A lease id of the old epoch can then never be
//! mistaken for one the new server grants, as an unacknowledged Result or a run that
//! a new Start would otherwise be taken to resend.
//!
//! The fence and the Start window read a [`Clock`] that counts suspended time (issue
//! #78); tokio's timers do not, so the loop never sleeps longer than `recheck_every`
//! (1 s) while a deadline is pending. Whatever wakes the daemon (a message, a heartbeat
//! tick, a finished run, that recheck, or, between streams, a connection, a stream or
//! a Welcome arriving), the fence is checked first: after a resume past T, no running
//! lease survives to have its Result sent, and neither a Welcome for a Hello sent after
//! the resume nor an acknowledgement of a heartbeat sent after it renews the contact
//! first.
//!
//! A fence kills without waiting (see [`crate::lease`]): the loop goes on while a run
//! is slow to stop. Once contact is lost and the stream has acknowledged nothing for
//! 1.5 T of time awake, the stream is ended and the daemon connects again and says
//! Hello; the fenced leases are listed on the new stream until their runs stop and
//! report.
//!
//! A v1 assumption: the server acknowledges every Result. ResultAck is an addition
//! within protocol version 1 (worker.proto), so Welcome's version check does not rule
//! out a server built before it; such a server would leave every Result kept, resent
//! on every stream and listed in every Heartbeat for the daemon's lifetime. No server
//! without ResultAck has been released, which is why the version was not raised for it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_proto::google::rpc::Code;
use kbf_proto::kbf::map::v1 as wire;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{
    self, DaemonMessage, Heartbeat, Hello, Phase, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_types::{LeaseId, NodePhase};
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout};
use tonic::transport::{ClientTlsConfig, Endpoint};

use crate::clock::{Clock, SystemClock};
use crate::config::{ConfigError, DaemonConfig};
use crate::connect::{
    Backoff, Resolve, Server, SystemResolver, Target, WARN_EVERY, WarnLimit, resolve_all,
    rotate_after, unit_random,
};
use crate::contact::Contact;
use crate::lease::{Done, Leases, failure, lease_id, proto_lease_id};
use crate::map::{MapLink, map_asked};
use crate::phases::LeasePhases;
use crate::pressure;
use crate::report::NodeReport;
use crate::runtimes::Runtimes;
use crate::status::{DriverReport, Software};
use crate::window::StartWindow;

/// The `kbf.worker.v1` protocol version this daemon speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Something the daemon did that an observer (a test, later the meter) may want.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A stream connected and its Hello went out; Welcome is awaited.
    HelloSent,
    /// A session began: the server answered Hello.
    Welcomed { heartbeat_interval: Duration },
    /// The server placed a lease here; nothing runs until its Start.
    Offered(LeaseId),
    /// A Start arrived after the window it names, or names a heartbeat this stream
    /// did not send: the lease was not run, and no Result is sent (issue #23).
    StartExpired(LeaseId),
    /// The server cancelled a running lease; its kill has begun.
    Cancelled(LeaseId),
    /// The server decided a Result: `accepted` says whether it became the operation's
    /// result. Either way the daemon forgets the Result: it is neither resent nor
    /// listed in heartbeats any more.
    ResultAcknowledged { lease: LeaseId, accepted: bool },
    /// No heartbeat sent in the last two intervals has been acknowledged.
    HeartbeatGap { silent_for: Duration },
    /// An acknowledgement arrived after a gap.
    ContactRestored,
    /// Contact was lost for the fence time; these leases are being killed. Each
    /// reports, fenced (lost), once its run has stopped.
    Fenced(Vec<LeaseId>),
    /// A Welcome named another lease epoch: these leases, granted under an earlier
    /// one, are dropped. Their runs are being killed, and no Result of them is sent.
    Superseded(Vec<LeaseId>),
    /// A welcomed session ended, with the reason.
    Disconnected(String),
    /// A lease's run ended (its Result is being sent): where its time went, as the
    /// `lease phases:` line logs it.
    LeaseEnded(LeasePhases),
    /// An attempt to connect ended before the server's Welcome (a refused connection, a
    /// TLS failure, a server answering `UNAVAILABLE`, ...), or a server's name resolved
    /// to no address. `server` names the URL, and the address when there was one.
    ConnectFailed { server: String, reason: String },
    /// Every address of a round of attempts failed: the daemon waits `wait` and starts
    /// the next round. `failed_rounds` counts the rounds since the last session.
    Retrying { failed_rounds: u32, wait: Duration },
}

/// Why a session ended.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("connect: {0}")]
    Connect(#[from] tonic::transport::Error),
    #[error("connect: no connection within {0:?}")]
    ConnectTimeout(Duration),
    #[error("stream: {0}")]
    Stream(#[from] tonic::Status),
    #[error("no Welcome within {0:?}")]
    WelcomeTimeout(Duration),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("no heartbeat acknowledged within {0:?}")]
    Silent(Duration),
}

impl SessionError {
    /// This error and every cause under it, outermost first, joined by `": "`: what
    /// the log says when a session ends (issue #170). A `tonic::transport::Error`
    /// displays as `transport error` alone; what failed (a refused connection, a TLS
    /// alert, a name that does not resolve) is only in its sources.
    ///
    /// Each variant's own text already includes the error it wraps, so the chain is
    /// followed from that error's source.
    #[must_use]
    pub fn with_causes(&self) -> String {
        let mut text = self.to_string();
        let wrapped = std::error::Error::source(self);
        let mut cause = wrapped.and_then(std::error::Error::source);
        while let Some(e) = cause {
            text.push_str(": ");
            text.push_str(&e.to_string());
            cause = e.source();
        }
        text
    }
}

/// A daemon: configuration, runtimes, node report and the state that outlives streams.
pub struct Daemon {
    config: DaemonConfig,
    /// The `--server` URLs, parsed.
    servers: Vec<Server>,
    /// The client TLS every connection uses, before its server name is set.
    tls: ClientTlsConfig,
    /// How server names become addresses, asked again every round.
    resolver: Arc<dyn Resolve>,
    /// Whether the current (or the last) stream got its Welcome.
    welcomed: bool,
    /// The report the daemon was started with, with a `drivers` entry for each of its
    /// runtimes.
    base: NodeReport,
    /// What Hello carries and heartbeats hash: `base` plus the driver's newest entries.
    report: NodeReport,
    /// What `NodeStatus` says about the operating system.
    software: Software,
    /// The driver's newest report, if it sends any; see [`Self::with_driver_report`].
    driver: Option<watch::Receiver<DriverReport>>,
    /// Every Xcode the driver's newest report names, ready or not.
    xcodes: Vec<worker::XcodeStatus>,
    /// The container images `NodeStatus` lists; see [`Self::with_container_images`].
    container_images: Vec<String>,
    /// The runtime's phase, if one reports it; see [`Self::with_phase`].
    phase: Option<watch::Receiver<NodePhase>>,
    /// The newest phase taken from it.
    phase_now: NodePhase,
    /// The memory backstop, if one is set; see [`Self::with_memory_backstop`]. Every
    /// Hello carries `phase_now`, or [`NodePhase::MemoryPressure`] in its place while
    /// the node is serving and the backstop has paused it.
    pressure: Option<Backstop>,
    /// Where the storage map of `Welcome` and `MapUpdate` goes, if the daemon routes by
    /// one; see [`Self::with_map`].
    map: Option<MapLink>,
    events: Option<mpsc::UnboundedSender<Event>>,
    leases: Leases,
    done: mpsc::UnboundedReceiver<Done>,
    contact: Contact,
    /// Send times of this stream's Hello and heartbeats, which Starts name.
    window: StartWindow,
    /// Results the server has not acknowledged yet, by lease.
    unacked: BTreeMap<LeaseId, worker::Result>,
    /// The lease epoch the newest Welcome named; `None` before the first, or when it
    /// named none (0).
    epoch: Option<u64>,
    /// Each lease acted on and not yet forgotten: the epoch of its Start and the action
    /// it named.
    granted: BTreeMap<LeaseId, Granted>,
    /// The suspend-counting clock that contact and the Start window read.
    clock: Arc<dyn Clock>,
    /// This daemon process's `Hello.instance_id`, the same on every stream it opens.
    instance: String,
    /// `Hello.predecessor_instance_id`: the daemon process whose runs this one's drivers
    /// killed before it started; empty when none is known.
    predecessor: String,
}

/// The memory backstop: the sampled pressure, the percentage above which a node that
/// runs leases takes no more, and whether it is paused now.
struct Backstop {
    stall: watch::Receiver<Option<f64>>,
    above: f64,
    paused: bool,
}

/// What the daemon remembers of a lease's Start until it forgets the lease.
struct Granted {
    /// The lease epoch in force when the Start arrived.
    epoch: Option<u64>,
    /// The action the Start named, which the lease's Result echoes.
    action: Option<Digest>,
}

/// What woke the session loop.
enum Wake {
    Message(ServerMessage),
    Beat,
    Done(Box<Done>),
    Recheck,
    Driver,
    Phase,
    Pressure,
    MapAsked,
}

impl Daemon {
    /// A daemon that will connect as `config` says. Reads the TLS files now, so a
    /// missing or unreadable file fails here rather than on the first connection, and
    /// detects the node's software ([`Software::detect`]).
    ///
    /// A Start runs on the runtime of `runtimes` that serves its kind (one runtime, an
    /// `Arc<R>`, converts). The node report is `report` with a `drivers` entry for every
    /// runtime's driver added, whether `report` lists them or not.
    pub fn new(
        config: DaemonConfig,
        runtimes: impl Into<Runtimes>,
        report: NodeReport,
    ) -> Result<Self, ConfigError> {
        let runtimes = runtimes.into();
        let drivers = runtimes.drivers().into_iter().map(|d| ("drivers", d));
        let report = report.with_entries(drivers);
        let servers = config
            .servers
            .iter()
            .map(|url| Server::parse(url))
            .collect::<Result<Vec<_>, _>>()?;
        if servers.is_empty() {
            return Err(ConfigError::NoServer);
        }
        let tls = config.tls.load()?;
        let (done_tx, done) = mpsc::unbounded_channel();
        let contact = Contact::new(config.fence_after);
        Ok(Self {
            config,
            servers,
            tls,
            resolver: Arc::new(SystemResolver),
            welcomed: false,
            base: report.clone(),
            report,
            software: Software::detect(),
            driver: None,
            xcodes: Vec::new(),
            container_images: Vec::new(),
            phase: None,
            phase_now: NodePhase::Serving,
            pressure: None,
            map: None,
            events: None,
            leases: Leases::new(runtimes, done_tx),
            done,
            contact,
            window: StartWindow::default(),
            unacked: BTreeMap::new(),
            epoch: None,
            granted: BTreeMap::new(),
            clock: Arc::new(SystemClock),
            instance: instance_id(),
            predecessor: String::new(),
        })
    }

    /// Puts the storage map the server sends (in `Welcome`, and in each `MapUpdate`) in
    /// `holder`, which keeps the newest and never goes back, and asks the server for a
    /// map when the holder is waiting for a generation it does not have
    /// ([`kbf_mapsync::MapHolder::reach`]). Without one the daemon ignores the map.
    #[must_use]
    pub fn with_map(mut self, holder: Arc<kbf_mapsync::MapHolder>) -> Self {
        self.map = Some(MapLink::new(holder));
        self
    }

    /// This daemon process's instance id (`Hello.instance_id`), drawn when it was built.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// Names `predecessor` in every Hello as the daemon process that ran on this node
    /// before this one (`Hello.predecessor_instance_id`). The caller must have killed
    /// every run that process left before this daemon runs: the server gives up that
    /// process's leases at once on the strength of it.
    #[must_use]
    pub fn with_predecessor(mut self, predecessor: String) -> Self {
        self.predecessor = predecessor;
        self
    }

    /// Sends every [`Event`] to `events` as well as to the log.
    #[must_use]
    pub fn with_events(mut self, events: mpsc::UnboundedSender<Event>) -> Self {
        self.events = Some(events);
        self
    }

    /// Adds `driver`'s entries to the report and its Xcodes to `NodeStatus`, now and
    /// each time it changes (see the module documentation).
    #[must_use]
    pub fn with_driver_report(mut self, driver: watch::Receiver<DriverReport>) -> Self {
        self.driver = Some(driver);
        self.take_driver_report();
        self
    }

    /// Takes the driver's newest report, if it sends any; returns whether the node
    /// report changed.
    fn take_driver_report(&mut self) -> bool {
        let Some(driver) = &mut self.driver else {
            return false;
        };
        let newest = driver.borrow_and_update().clone();
        let report = self.base.clone().with_entries(newest.entries);
        self.xcodes = newest.xcodes;
        let changed = report != self.report;
        self.report = report;
        changed
    }

    /// Sends `phase`'s value in every Hello, and resends the Hello on the stream each
    /// time it changes (see the module documentation).
    #[must_use]
    pub fn with_phase(mut self, phase: watch::Receiver<NodePhase>) -> Self {
        self.phase = Some(phase);
        self.take_phase();
        self
    }

    /// Takes the runtime's newest phase, if it reports one; returns whether it changed.
    fn take_phase(&mut self) -> bool {
        let Some(phase) = &mut self.phase else {
            return false;
        };
        let newest = phase.borrow_and_update().clone();
        let changed = newest != self.phase_now;
        self.phase_now = newest;
        changed
    }

    /// Stops taking new leases while `stall`, the node's memory stall time as a
    /// percentage ([`crate::pressure::watch`]), is above `above` and a lease runs: the
    /// Hello says [`NodePhase::MemoryPressure`] then, and resends when it changes (see
    /// [`crate::pressure`]). Leases already running are not touched. A runtime's own
    /// phase (a session switch, a quarantine) takes the place of this one.
    #[must_use]
    pub fn with_memory_backstop(mut self, stall: watch::Receiver<Option<f64>>, above: f64) -> Self {
        self.pressure = Some(Backstop {
            stall,
            above,
            paused: false,
        });
        self.take_pressure();
        self
    }

    /// The percentage of memory stall time above which the backstop pauses this node
    /// while it runs a lease; `None` without one ([`Self::with_memory_backstop`]).
    #[must_use]
    pub fn memory_backstop(&self) -> Option<f64> {
        self.pressure.as_ref().map(|backstop| backstop.above)
    }

    /// The phase every Hello carries: the runtime's, or memory pressure in place of
    /// serving while the backstop has the node paused.
    fn phase(&self) -> NodePhase {
        match &self.pressure {
            Some(backstop) if backstop.paused && self.phase_now == NodePhase::Serving => {
                NodePhase::MemoryPressure
            }
            _ => self.phase_now.clone(),
        }
    }

    /// Takes the newest pressure reading and the number of leases running; returns
    /// whether the phase a Hello carries changed.
    fn take_pressure(&mut self) -> bool {
        let leases = self.leases.running().len();
        let before = self.phase();
        let Some(backstop) = &mut self.pressure else {
            return false;
        };
        let stall = *backstop.stall.borrow_and_update();
        backstop.paused = pressure::paused(stall, backstop.above, leases);
        self.phase() != before
    }

    /// Lists `images` in every `NodeStatus`, sorted and each once: the container
    /// images the driver checked present at start. Status only; they route no work.
    #[must_use]
    pub fn with_container_images(mut self, images: impl IntoIterator<Item = String>) -> Self {
        let images: std::collections::BTreeSet<String> = images.into_iter().collect();
        self.container_images = images.into_iter().collect();
        self
    }

    /// The `NodeStatus` this node sends now: its software, every Xcode the driver's
    /// newest report names, ready or not, and the container images it was given.
    #[must_use]
    pub fn node_status(&self) -> worker::NodeStatus {
        worker::NodeStatus {
            xcodes: self.xcodes.clone(),
            container_images: self.container_images.clone(),
            ..self.software.status(&self.report)
        }
    }

    /// Resolves server names with `resolver` instead of the operating system's. A test
    /// passes one whose answer it changes between rounds.
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Reads `clock` for the fence and the Start window instead of [`SystemClock`]. A
    /// test passes one it can jump forward, as a resume does.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Runs the daemon until `shutdown` completes: connects, serves the session, and
    /// reconnects whenever the session ends, as [`crate::connect`] describes, for as
    /// long as it runs. v0 abandons running leases at shutdown; the scheduler
    /// re-dispatches them after G.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) {
        #[cfg(target_os = "linux")]
        crate::numa::log_layout();
        tokio::select! {
            () = self.reconnect_forever() => {}
            () = shutdown => tracing::info!("shutting down"),
        }
    }

    /// Rounds of connection attempts, without end: each round resolves every server
    /// again and tries each address in turn, starting after the last one tried; a
    /// round in which none was welcomed is followed by a growing wait.
    async fn reconnect_forever(&mut self) {
        let mut backoff = Backoff::new(self.config.reconnect_after, self.config.reconnect_max);
        let mut warn = WarnLimit::new(WARN_EVERY);
        let mut last = None;
        loop {
            let within = self.config.welcome_timeout;
            let (resolver, servers) = (Arc::clone(&self.resolver), self.servers.clone());
            let (targets, unresolved) =
                self.offline(resolve_all(&resolver, &servers, within)).await;
            for (server, reason) in unresolved {
                self.failed(&mut warn, server, &reason);
            }
            let mut welcomed = false;
            for target in rotate_after(targets, last) {
                last = Some(target.addr);
                let reason = match self.session(&target).await {
                    Ok(()) => "the server ended the stream".to_owned(),
                    Err(e) => e.with_causes(),
                };
                if self.welcomed {
                    tracing::warn!(server = %target, %reason, "session ended");
                    self.emit(Event::Disconnected(reason));
                    warn.welcomed();
                    welcomed = true;
                    break;
                }
                self.failed(&mut warn, target.to_string(), &reason);
            }
            let wait = if welcomed {
                backoff.welcomed(unit_random())
            } else {
                let wait = backoff.failed(unit_random());
                let failed_rounds = backoff.failed_rounds();
                tracing::debug!(failed_rounds, ?wait, "no server reached; waiting");
                self.emit(Event::Retrying {
                    failed_rounds,
                    wait,
                });
                wait
            };
            self.offline(sleep(wait)).await;
        }
    }

    /// Logs and reports an attempt that ended before Welcome: at WARN when `warn` allows,
    /// else at DEBUG.
    fn failed(&self, warn: &mut WarnLimit, server: String, reason: &str) {
        let (attempts, loud) = warn.failed(tokio::time::Instant::now());
        if loud {
            tracing::warn!(%server, %reason, attempts, "cannot reach a server; retrying");
        } else {
            tracing::debug!(%server, %reason, attempts, "cannot reach a server; retrying");
        }
        self.emit(Event::ConnectFailed {
            server,
            reason: reason.to_owned(),
        });
    }

    /// The endpoint for one attempt: `target`'s address, with the URL's host as the
    /// HTTP/2 authority and, unless `--tls-server-name` names another, as the name the
    /// server's certificate must carry; with the configured HTTP/2 keepalive.
    fn endpoint(&self, target: &Target) -> Result<Endpoint, SessionError> {
        let mut tls = self.tls.clone();
        if self.config.tls.server_name.is_none() {
            tls = tls.domain_name(target.server.host.clone());
        }
        let endpoint = Endpoint::from_shared(format!("https://{}", target.addr))?
            .origin(target.server.uri.clone())
            .tls_config(tls)?;
        Ok(self.config.keep_alive.apply(endpoint))
    }

    /// One stream to `target`, from connecting until it ends. Sets `welcomed` once the
    /// server's Welcome is taken.
    async fn session(&mut self, target: &Target) -> Result<(), SessionError> {
        self.welcomed = false;
        let endpoint = self.endpoint(target)?;
        let wait = self.config.welcome_timeout;
        let channel = self
            .offline(timeout(wait, endpoint.connect()))
            .await
            .map_err(|_| SessionError::ConnectTimeout(wait))??;
        let mut client = WorkerClient::new(channel);
        let (tx, rx) = unbounded();
        // A change that arrived while no stream was up.
        self.take_driver_report();
        self.take_phase();
        self.take_pressure();
        let hello_sent = self.clock.now();
        send(&tx, daemon_message::Message::Hello(self.hello()));
        self.emit(Event::HelloSent);

        let wait = self.config.welcome_timeout;
        let response = self
            .offline(timeout(wait, client.session(rx)))
            .await
            .map_err(|_| SessionError::WelcomeTimeout(wait))??;
        let mut inbound = response.into_inner();
        let first = self
            .offline(timeout(wait, inbound.message()))
            .await
            .map_err(|_| SessionError::WelcomeTimeout(wait))??;
        let (interval, epoch, map) = self.welcome(first)?;
        self.welcomed = true;
        self.contact.new_stream(interval);
        self.window.new_stream(hello_sent);
        // Before the Welcome renews contact: a lease whose fence passed while this
        // stream was set up must not outlive it (`offline` checked already; this keeps
        // the order true by construction here too).
        self.recheck();
        // Before any Result is resent or any Start of this stream is read.
        self.new_epoch(epoch);
        self.new_map_stream(map);
        if self.contact.confirm(hello_sent) {
            self.emit(Event::ContactRestored);
        }
        tracing::info!(server = %target, ?interval, "welcomed");
        self.emit(Event::Welcomed {
            heartbeat_interval: interval,
        });
        // Before the first Heartbeat, which lists these leases (issue #26).
        for result in self.unacked.values() {
            send(&tx, daemon_message::Message::Result(result.clone()));
        }
        send(&tx, daemon_message::Message::NodeStatus(self.node_status()));

        // When this stream last acknowledged a heartbeat (or welcomed), on tokio's
        // clock, which stops during a suspend: a resumed daemon gives its stream T
        // awake to show it still works.
        let mut heard = tokio::time::Instant::now();
        let mut seq = 0u64;
        let mut beat = tokio::time::interval(interval);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let wait = self.until_recheck();
            let wake = tokio::select! {
                msg = inbound.message() => match msg? {
                    Some(msg) => Wake::Message(msg),
                    None => return Ok(()),
                },
                _ = beat.tick() => Wake::Beat,
                Some(done) = self.done.recv() => Wake::Done(Box::new(done)),
                () = sleep(wait) => Wake::Recheck,
                Some(()) = driver_changed(self.driver.as_mut()) => Wake::Driver,
                Some(()) = phase_changed(self.phase.as_mut()) => Wake::Phase,
                Some(()) = pressure_changed(self.pressure.as_mut()) => Wake::Pressure,
                () = map_asked(self.map.as_mut()) => Wake::MapAsked,
            };
            // First, whatever woke the loop: the clock may have jumped over a suspend.
            self.recheck();
            self.check_silence(heard)?;
            match wake {
                Wake::Message(msg) => {
                    if let Some(server_message::Message::HeartbeatAck(_)) = msg.message {
                        heard = tokio::time::Instant::now();
                    }
                    self.on_message(msg, &tx);
                }
                Wake::Beat => {
                    seq += 1;
                    let now = self.clock.now();
                    self.contact.sent(seq, now);
                    self.window.sent(seq, now);
                    let heartbeat = Heartbeat {
                        seq,
                        report_hash: self.report.hash().to_vec(),
                        running: self.listed().into_iter().map(proto_lease_id).collect(),
                    };
                    send(&tx, daemon_message::Message::Heartbeat(heartbeat));
                }
                Wake::Done(done) => self.finished(Some(&tx), *done),
                Wake::Recheck => {}
                Wake::Driver => {
                    if self.take_driver_report() {
                        send(&tx, daemon_message::Message::Hello(self.hello()));
                    }
                    send(&tx, daemon_message::Message::NodeStatus(self.node_status()));
                }
                Wake::Phase => {
                    if self.take_phase() {
                        tracing::info!(phase = self.phase().name(), "node phase changed");
                        send(&tx, daemon_message::Message::Hello(self.hello()));
                    }
                }
                // The request for a map is sent below, whatever woke the loop.
                Wake::Pressure | Wake::MapAsked => {}
            }
            self.ask_map(&tx);
            // Whatever woke the loop, leases may have started or ended.
            if self.take_pressure() {
                tracing::info!(phase = self.phase().name(), "node phase changed");
                send(&tx, daemon_message::Message::Hello(self.hello()));
            }
        }
    }

    /// Takes the map a `Welcome` carried, the first of a new stream: the generation the
    /// holder waits for is asked for again on this stream, by [`Self::ask_map`]. These are
    /// methods rather than code in [`Self::session`] so that none of it is conditional
    /// there (the session is compiled once for each test binary, and the coverage tool
    /// counts the lines of the copy that covers most).
    fn new_map_stream(&mut self, map: Option<wire::Map>) {
        if let Some(link) = self.map.as_mut() {
            link.new_stream();
            link.take(map);
        }
    }

    /// Sends the `MapRequest` for a generation the holder waits for, once on a stream.
    fn ask_map(&mut self, tx: &UnboundedSender<DaemonMessage>) {
        if let Some(request) = self.map.as_mut().and_then(MapLink::request) {
            send(tx, daemon_message::Message::MapRequest(request));
        }
    }

    /// Ends the session when contact is lost and this stream has acknowledged no
    /// heartbeat since `heard` for 1.5 T of time awake: it is stuck, or the server
    /// gave this node up, and a new stream says Hello again. Half T past the fence, so
    /// the Results of runs that stop at once go out on this stream first.
    fn check_silence(&self, heard: tokio::time::Instant) -> Result<(), SessionError> {
        let silent_for = self.contact.fence_after().saturating_mul(3) / 2;
        if self.contact.lost(self.clock.now()) && heard.elapsed() >= silent_for {
            return Err(SessionError::Silent(silent_for));
        }
        Ok(())
    }

    /// Checks the first server message is an acceptable Welcome; returns its interval,
    /// the lease epoch it names (`None` for 0) and the storage map it carries.
    fn welcome(
        &self,
        first: Option<ServerMessage>,
    ) -> Result<(Duration, Option<u64>, Option<wire::Map>), SessionError> {
        let Some(server_message::Message::Welcome(w)) = first.and_then(|m| m.message) else {
            return Err(SessionError::Protocol(
                "the first server message is not Welcome".into(),
            ));
        };
        if w.protocol_version != PROTOCOL_VERSION {
            return Err(SessionError::Protocol(format!(
                "server speaks protocol version {}, this daemon {PROTOCOL_VERSION}",
                w.protocol_version
            )));
        }
        let interval = Duration::from_millis(w.heartbeat_interval_ms);
        // A gap is two intervals; it must be noticed well before the fence.
        if interval.is_zero() || interval.saturating_mul(2) >= self.contact.fence_after() {
            return Err(SessionError::Protocol(format!(
                "heartbeat interval {interval:?} does not fit the fence time {:?}",
                self.contact.fence_after()
            )));
        }
        Ok((interval, (w.epoch != 0).then_some(w.epoch), w.map))
    }

    /// Takes the lease epoch a Welcome named. When it names one, every lease granted
    /// under another is dropped: its run is killed and its Result forgotten, unsent
    /// (issue #137). A Welcome that names none drops nothing, and a lease granted while
    /// none was named is kept.
    fn new_epoch(&mut self, epoch: Option<u64>) {
        self.epoch = epoch;
        if let Some(lease) = self.phase_now.switching()
            && epoch.is_some_and(|epoch| epoch != lease.term)
        {
            // The server that granted it is gone: no Start for it will come again.
            tracing::warn!(%lease, ?epoch, "the switching lease is of an earlier epoch");
            self.leases.forget(lease);
        }
        let stale: Vec<LeaseId> = self
            .granted
            .keys()
            .copied()
            .filter(|id| self.superseded(*id))
            .collect();
        if stale.is_empty() {
            return;
        }
        let running = self.leases.running();
        for id in &stale {
            self.unacked.remove(id);
            if running.contains(id) {
                // Its Result is dropped when the run ends (`report`).
                self.leases.cancel(*id);
            } else {
                self.granted.remove(id);
            }
        }
        tracing::warn!(?stale, ?epoch, "leases of an earlier epoch dropped");
        self.emit(Event::Superseded(stale));
    }

    /// Whether lease `id` was granted under a lease epoch other than the newest
    /// Welcome's, when that Welcome named one.
    fn superseded(&self, id: LeaseId) -> bool {
        superseded(self.granted.get(&id).and_then(|g| g.epoch), self.epoch)
    }

    fn on_message(&mut self, msg: ServerMessage, tx: &UnboundedSender<DaemonMessage>) {
        match msg.message {
            Some(server_message::Message::HeartbeatAck(ack)) => {
                self.window.acknowledged(ack.seq);
                if self.contact.acknowledged(ack.seq).restored {
                    tracing::info!("contact restored");
                    self.emit(Event::ContactRestored);
                }
            }
            Some(server_message::Message::LeaseOffer(offer)) => {
                if let Some(id) = offer.lease_id.map(lease_id) {
                    tracing::info!(lease = %id, kind = %offer.kind, "lease offered");
                    self.emit(Event::Offered(id));
                }
            }
            Some(server_message::Message::Start(start)) => self.on_start(start, tx),
            Some(server_message::Message::ResultAck(ack)) => {
                if let Some(id) = ack.lease_id.map(lease_id) {
                    self.unacked.remove(&id);
                    self.granted.remove(&id);
                    tracing::info!(lease = %id, accepted = ack.accepted, "result acknowledged");
                    self.emit(Event::ResultAcknowledged {
                        lease: id,
                        accepted: ack.accepted,
                    });
                }
            }
            Some(server_message::Message::Cancel(cancel)) => {
                if let Some(id) = cancel.lease_id.map(lease_id) {
                    if self.leases.cancel(id) {
                        self.emit(Event::Cancelled(id));
                    } else {
                        self.leases.forget(id);
                    }
                }
            }
            Some(server_message::Message::Welcome(_)) => {
                tracing::warn!("a second Welcome on one stream ignored");
            }
            Some(server_message::Message::MapUpdate(update)) => {
                if let Some(link) = &self.map {
                    link.take(update.map);
                }
            }
            None => tracing::warn!("an empty server message ignored"),
        }
    }

    /// Runs the lease a Start names, unless its Result is still unacknowledged, it
    /// arrived after its window, or contact is lost (then it is refused with a Result).
    /// A lease it acts on is remembered ([`Self::remember`]).
    fn on_start(&mut self, start: worker::Start, tx: &UnboundedSender<DaemonMessage>) {
        let done = start.lease_id.map(lease_id);
        if let Some(id) = done.filter(|id| self.unacked.contains_key(id)) {
            // Its Result stands until acknowledged; running it again could
            // produce a second one.
            tracing::warn!(lease = %id, "a Start for a lease already reported ignored");
            return;
        }
        let valid_for = Duration::from_millis(start.valid_for_ms);
        if !self
            .window
            .allows(start.heartbeat_seq, valid_for, self.clock.now())
        {
            // The scheduler may have given the lease up and granted it again; it
            // gives it up here too, as a Start that never arrived.
            if let Some(id) = done {
                tracing::warn!(lease = %id, "a Start that arrived after its window not run");
                self.emit(Event::StartExpired(id));
            }
            return;
        }
        self.remember(done, &start);
        let refused = if self.contact.lost(self.clock.now()) {
            start.lease_id.map(|id| {
                failure(
                    lease_id(id),
                    Code::Unavailable,
                    "contact with the server is lost; not starting",
                )
            })
        } else {
            self.leases.start(start)
        };
        if let Some(result) = refused {
            self.report(Some(tx), result);
        }
    }

    /// Remembers lease `id` of `start`, if the Start names one and it is not remembered
    /// already: the lease epoch in force and the action the Start named.
    fn remember(&mut self, id: Option<LeaseId>, start: &worker::Start) {
        if let Some(id) = id {
            let granted = Granted {
                epoch: self.epoch,
                action: start.action_digest.clone(),
            };
            self.granted.entry(id).or_insert(granted);
        }
    }

    /// Declares a heartbeat gap, and fences, when their deadlines have passed. A fence
    /// returns at once: each fenced lease reports through `done` once its run stops.
    fn recheck(&mut self) {
        let now = self.clock.now();
        if let Some(silent_for) = self.contact.check_gap(now) {
            tracing::warn!(?silent_for, "heartbeat gap: no acknowledgement");
            self.emit(Event::HeartbeatGap { silent_for });
        }
        if !self.contact.lost(now) {
            return;
        }
        let fenced = self.leases.fence();
        if !fenced.is_empty() {
            self.emit(Event::Fenced(fenced));
        }
    }

    /// Reports a finished run.
    fn finished(
        &mut self,
        tx: Option<&UnboundedSender<DaemonMessage>>,
        (id, outcome, usage, phases): Done,
    ) {
        let result = self.leases.finished(id, outcome, usage, &phases);
        self.emit(Event::LeaseEnded(phases));
        self.report(tx, result);
    }

    /// How long to sleep before [`Self::check`] may have something to do: until its
    /// next deadline, but never longer than `recheck_every`, since the sleep runs on a
    /// clock that stops during suspend and the deadline on one that does not.
    fn until_recheck(&self) -> Duration {
        let fence = if !self.leases.unfenced() {
            None
        } else {
            self.contact.fence_deadline()
        };
        [fence, self.contact.gap_deadline()]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_secs(3600), |deadline| {
                deadline
                    .saturating_duration_since(self.clock.now())
                    .min(self.config.recheck_every)
            })
    }

    /// Runs `fut` while no stream is up, still finishing leases and fencing on time.
    /// The fence is checked after `fut` completes too, so what the caller does next
    /// (send a Hello, take a Welcome as contact) comes after any fence a resume made due.
    async fn offline<F: Future>(&mut self, fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        loop {
            let wait = self.until_recheck();
            let (out, done) = tokio::select! {
                out = &mut fut => (Some(out), None),
                Some(done) = self.done.recv() => (None, Some(done)),
                () = sleep(wait) => (None, None),
            };
            // First: the clock may have jumped over a suspend.
            self.recheck();
            if let Some(out) = out {
                return out;
            }
            if let Some(done) = done {
                self.finished(None, done);
            }
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: self.config.node_id.clone(),
            daemon_version: crate::DAEMON_VERSION.to_owned(),
            capabilities: self.report.capabilities().to_vec(),
            report_hash: self.report.hash().to_vec(),
            instance_id: self.instance.clone(),
            phase: wire_phase(&self.phase()),
            predecessor_instance_id: self.predecessor.clone(),
        }
    }

    /// Keeps `result` until its lease's ResultAck, and sends it on `tx` if a stream is
    /// up. Without one it goes out after the next Welcome. It echoes the action its
    /// lease's Start named. The Result of a lease of an earlier epoch is dropped.
    fn report(&mut self, tx: Option<&UnboundedSender<DaemonMessage>>, mut result: worker::Result) {
        // Every Result here names its lease: the lease manager builds them all.
        let id = result.lease_id.map_or(LeaseId::new(0, 0), lease_id);
        if self.superseded(id) {
            tracing::warn!(lease = %id, "the Result of a lease of an earlier epoch dropped");
            self.granted.remove(&id);
            return;
        }
        result.action_digest = self.granted.get(&id).and_then(|g| g.action.clone());
        if let Some(tx) = tx {
            send(tx, daemon_message::Message::Result(result.clone()));
        }
        self.unacked.insert(id, result);
    }

    /// What a Heartbeat lists as running: the leases running now, and those whose
    /// Result the server has not acknowledged (issue #26).
    fn listed(&self) -> BTreeSet<LeaseId> {
        self.leases
            .running()
            .into_iter()
            .chain(self.unacked.keys().copied())
            .collect()
    }

    fn emit(&self, event: Event) {
        if let Some(events) = &self.events {
            // An observer that went away does not stop the daemon.
            let _ = events.send(event);
        }
    }
}

/// A new daemon process's instance id: 128 random bits as 32 hex digits, from two
/// hashers the standard library keys from the operating system's random source. It is
/// drawn at every start, so a restarted daemon, a second daemon started with the same
/// certificate and a daemon on a cloned machine each draw their own (issue #140). It is
/// written down for its successor only: `kbf-node` keeps it in the scratch directory,
/// and the next daemon names it as its predecessor ([`Daemon::with_predecessor`]).
fn instance_id() -> String {
    let half = || RandomState::new().hash_one(std::process::id());
    format!("{:016x}{:016x}", half(), half())
}

/// Completes when `driver` has a report the daemon has not taken; never when there is
/// no driver channel, or its sender is gone (`None` then disables the select arm).
async fn driver_changed(driver: Option<&mut watch::Receiver<DriverReport>>) -> Option<()> {
    driver?.changed().await.ok()
}

/// Completes when `phase` has a value the daemon has not taken; never without one.
async fn phase_changed(phase: Option<&mut watch::Receiver<NodePhase>>) -> Option<()> {
    phase?.changed().await.ok()
}

/// Completes when the memory backstop has a reading the daemon has not taken; never
/// without one.
async fn pressure_changed(backstop: Option<&mut Backstop>) -> Option<()> {
    backstop?.stall.changed().await.ok()
}

/// `phase` as Hello carries it: absent for serving, as a daemon that predates the
/// field sends it.
fn wire_phase(phase: &NodePhase) -> Option<worker::NodePhase> {
    let (state, lease, reason) = match phase {
        NodePhase::Serving => return None,
        NodePhase::Preparing(lease) => (Phase::Preparing, Some(*lease), ""),
        NodePhase::Restoring(lease) => (Phase::Restoring, Some(*lease), ""),
        NodePhase::Quarantined(reason) => (Phase::Quarantined, None, reason.as_str()),
        NodePhase::MemoryPressure => (Phase::MemoryPressure, None, ""),
    };
    Some(worker::NodePhase {
        phase: state.into(),
        lease_id: lease.map(proto_lease_id),
        reason: reason.to_owned(),
    })
}

/// Queues a message on the stream. A message for a stream that is gone is dropped: a
/// Result stays in the daemon's unacknowledged set and is resent on the next stream.
fn send(tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
    let _ = tx.unbounded_send(DaemonMessage {
        message: Some(message),
    });
}

/// Whether a lease granted under the lease epoch `granted` is dropped once the newest
/// `Welcome` named `newest` (issue #137): only when both name one and they differ. A
/// Welcome that names none drops nothing, and a lease granted while none was named is
/// kept. The daemon's rule, public so a simulated daemon follows the same one.
#[must_use]
pub fn superseded(granted: Option<u64>, newest: Option<u64>) -> bool {
    newest.is_some() && granted.is_some() && granted != newest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches (issue #170): a session error logged as `connect: transport error`
    /// alone, without the cause under it (here, the refused connection), and a chain
    /// that repeats the wrapped error.
    #[tokio::test]
    async fn a_connect_error_names_its_cause() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let endpoint = Endpoint::from_shared(format!("http://127.0.0.1:{port}")).expect("url");
        let error = SessionError::from(endpoint.connect().await.expect_err("nothing listens"));
        let text = error.with_causes();
        assert!(text.starts_with("connect: transport error: "), "{text}");
        assert_eq!(text.matches("transport error").count(), 1, "{text}");
        assert!(text.to_lowercase().contains("refused"), "{text}");
        let plain = SessionError::Protocol("no Hello".to_owned());
        assert_eq!(plain.with_causes(), "protocol: no Hello");
    }
}

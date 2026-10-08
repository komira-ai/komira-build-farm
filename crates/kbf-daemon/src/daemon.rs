//! The daemon's main loop: one outbound mutual-TLS stream at a time to a server front,
//! reconnecting when it ends, with the lease manager and the fence clock running
//! throughout, connected or not.
//!
//! A session sends Hello, waits for Welcome, then sends a Heartbeat at the interval
//! Welcome names and handles what the server sends: `HeartbeatAck` renews contact,
//! `LeaseOffer` is logged and runs nothing, `Start` runs a lease if it arrived within
//! the window it names (see `window`; a late one is dropped without a Result), and
//! `Cancel` kills a running lease. Results go out on the stream. Every Result is kept
//! until the server's `ResultAck` names its lease (issue #26): until then each
//! Heartbeat lists the lease in `running`, so the scheduler does not take it as lost,
//! and each new stream resends it right after Welcome, before its first Heartbeat. A
//! Result produced while disconnected is sent the same way.
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
//! A v1 assumption: the server acknowledges every Result. ResultAck is an addition
//! within protocol version 1 (worker.proto), so Welcome's version check does not rule
//! out a server built before it; such a server would leave every Result kept, resent
//! on every stream and listed in every Heartbeat for the daemon's lifetime. No server
//! without ResultAck has been released, which is why the version was not raised for it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::{
    self, DaemonMessage, Heartbeat, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_types::LeaseId;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tonic::transport::Endpoint;

use crate::clock::{Clock, Moment, SystemClock};
use crate::config::{ConfigError, DaemonConfig};
use crate::contact::Contact;
use crate::lease::{Done, Leases, failure, lease_id, proto_lease_id};
use crate::report::NodeReport;
use crate::runtime::Runtime;
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
    /// Contact was lost for the fence time; these leases were killed.
    Fenced(Vec<LeaseId>),
    /// A session ended, with the reason.
    Disconnected(String),
}

/// Why a session ended.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("connect: {0}")]
    Connect(#[from] tonic::transport::Error),
    #[error("stream: {0}")]
    Stream(#[from] tonic::Status),
    #[error("no Welcome within {0:?}")]
    WelcomeTimeout(Duration),
    #[error("protocol: {0}")]
    Protocol(String),
}

/// A daemon: configuration, runtime, node report and the state that outlives streams.
pub struct Daemon<R> {
    config: DaemonConfig,
    endpoint: Endpoint,
    report: NodeReport,
    events: Option<mpsc::UnboundedSender<Event>>,
    leases: Leases<R>,
    done: mpsc::UnboundedReceiver<Done>,
    contact: Contact,
    /// Send times of this stream's Hello and heartbeats, which Starts name.
    window: StartWindow,
    /// Results the server has not acknowledged yet, by lease.
    unacked: BTreeMap<LeaseId, worker::Result>,
    /// The suspend-counting clock that contact and the Start window read.
    clock: Arc<dyn Clock>,
}

/// What woke the session loop.
enum Wake {
    Message(ServerMessage),
    Beat,
    Done(Box<Done>),
    Recheck,
}

impl<R: Runtime> Daemon<R> {
    /// A daemon that will connect as `config` says. Reads the TLS files now, so a
    /// missing or unreadable file fails here rather than on the first connection.
    pub fn new(
        config: DaemonConfig,
        runtime: Arc<R>,
        report: NodeReport,
    ) -> Result<Self, ConfigError> {
        if !config.server.starts_with("https://") {
            return Err(ConfigError::NotHttps(config.server));
        }
        let tls = config.tls.load()?;
        let endpoint = Endpoint::from_shared(config.server.clone())
            .and_then(|e| e.tls_config(tls))
            .map_err(|source| ConfigError::Server {
                url: config.server.clone(),
                source,
            })?;
        let (done_tx, done) = mpsc::unbounded_channel();
        let contact = Contact::new(config.fence_after);
        Ok(Self {
            config,
            endpoint,
            report,
            events: None,
            leases: Leases::new(runtime, done_tx),
            done,
            contact,
            window: StartWindow::default(),
            unacked: BTreeMap::new(),
            clock: Arc::new(SystemClock),
        })
    }

    /// Sends every [`Event`] to `events` as well as to the log.
    #[must_use]
    pub fn with_events(mut self, events: mpsc::UnboundedSender<Event>) -> Self {
        self.events = Some(events);
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
    /// reconnects after `reconnect_after` whenever the session ends. v0 abandons
    /// running leases at shutdown; the scheduler re-dispatches them after G.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) {
        tokio::select! {
            () = self.reconnect_forever() => {}
            () = shutdown => tracing::info!("shutting down"),
        }
    }

    async fn reconnect_forever(&mut self) {
        loop {
            let reason = match self.session().await {
                Ok(()) => "the server ended the stream".to_owned(),
                Err(e) => e.to_string(),
            };
            tracing::warn!(%reason, "session ended");
            self.emit(Event::Disconnected(reason));
            self.offline(sleep(self.config.reconnect_after)).await;
        }
    }

    /// One stream, from Hello until it ends.
    async fn session(&mut self) -> Result<(), SessionError> {
        let endpoint = self.endpoint.clone();
        let channel = self.offline(endpoint.connect()).await?;
        let mut client = WorkerClient::new(channel);
        let (tx, rx) = unbounded();
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
        let interval = self.welcome(first)?;
        self.contact.new_stream(interval);
        self.window.new_stream(hello_sent);
        // Before the Welcome renews contact: a lease whose fence passed while this
        // stream was set up must not outlive it (`offline` checked already; this keeps
        // the order true by construction here too).
        self.recheck(None).await;
        if self.contact.confirm(hello_sent) {
            self.emit(Event::ContactRestored);
        }
        tracing::info!(?interval, "welcomed");
        self.emit(Event::Welcomed {
            heartbeat_interval: interval,
        });
        // Before the first Heartbeat, which lists these leases (issue #26).
        for result in self.unacked.values() {
            send(&tx, daemon_message::Message::Result(result.clone()));
        }

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
            };
            // First, whatever woke the loop: the clock may have jumped over a suspend.
            self.recheck(Some(&tx)).await;
            match wake {
                Wake::Message(msg) => self.on_message(msg, &tx),
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
            }
        }
    }

    /// Checks the first server message is an acceptable Welcome; returns its interval.
    fn welcome(&self, first: Option<ServerMessage>) -> Result<Duration, SessionError> {
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
        Ok(interval)
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
                    tracing::info!(lease = %id, accepted = ack.accepted, "result acknowledged");
                    self.emit(Event::ResultAcknowledged {
                        lease: id,
                        accepted: ack.accepted,
                    });
                }
            }
            Some(server_message::Message::Cancel(cancel)) => {
                if let Some(id) = cancel.lease_id.map(lease_id)
                    && self.leases.cancel(id)
                {
                    self.emit(Event::Cancelled(id));
                }
            }
            Some(server_message::Message::Welcome(_)) => {
                tracing::warn!("a second Welcome on one stream ignored");
            }
            None => tracing::warn!("an empty server message ignored"),
        }
    }

    /// Runs the lease a Start names, unless its Result is still unacknowledged, it
    /// arrived after its window, or contact is lost (then it is refused with a Result).
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

    /// Declares a heartbeat gap, and fences, when their deadlines have passed. Returns
    /// the Results of fenced leases.
    async fn check(&mut self, now: Moment) -> Vec<worker::Result> {
        if let Some(silent_for) = self.contact.check_gap(now) {
            tracing::warn!(?silent_for, "heartbeat gap: no acknowledgement");
            self.emit(Event::HeartbeatGap { silent_for });
        }
        let running = self.leases.running();
        if running.is_empty() || !self.contact.lost(now) {
            return Vec::new();
        }
        let results = self.leases.fence().await;
        self.emit(Event::Fenced(running));
        results
    }

    /// Runs [`Self::check`] now, and reports the Results of any leases it fenced.
    async fn recheck(&mut self, tx: Option<&UnboundedSender<DaemonMessage>>) {
        let now = self.clock.now();
        for result in self.check(now).await {
            self.report(tx, result);
        }
    }

    /// Reports a finished run, unless it was fenced.
    fn finished(&mut self, tx: Option<&UnboundedSender<DaemonMessage>>, (id, outcome): Done) {
        if let Some(result) = self.leases.finished(id, outcome) {
            self.report(tx, result);
        }
    }

    /// How long to sleep before [`Self::check`] may have something to do: until its
    /// next deadline, but never longer than `recheck_every`, since the sleep runs on a
    /// clock that stops during suspend and the deadline on one that does not.
    fn until_recheck(&self) -> Duration {
        let fence = if self.leases.running().is_empty() {
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
            self.recheck(None).await;
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
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: self.report.capabilities().to_vec(),
            report_hash: self.report.hash().to_vec(),
        }
    }

    /// Keeps `result` until its lease's ResultAck, and sends it on `tx` if a stream is
    /// up. Without one it goes out after the next Welcome.
    fn report(&mut self, tx: Option<&UnboundedSender<DaemonMessage>>, result: worker::Result) {
        // Every Result here names its lease: the lease manager builds them all.
        let id = result.lease_id.map_or(LeaseId::new(0, 0), lease_id);
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

/// Queues a message on the stream. A message for a stream that is gone is dropped: a
/// Result stays in the daemon's unacknowledged set and is resent on the next stream.
fn send(tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
    let _ = tx.unbounded_send(DaemonMessage {
        message: Some(message),
    });
}

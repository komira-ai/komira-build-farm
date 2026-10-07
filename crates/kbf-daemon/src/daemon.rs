//! The daemon's main loop: one outbound mutual-TLS stream at a time to a server front,
//! reconnecting when it ends, with the lease manager and the fence clock running
//! throughout, connected or not.
//!
//! A session sends Hello, waits for Welcome, then sends a Heartbeat at the interval
//! Welcome names and handles what the server sends: `HeartbeatAck` renews contact,
//! `LeaseOffer` is logged and runs nothing, `Start` runs a lease. Results go out on the
//! stream; a Result produced while disconnected waits in an outbox for the next
//! Welcome. v0 limit: the protocol has no acknowledgement for a Result, so one written
//! into a stream that then breaks is not sent again.

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
use tokio::time::{Instant, sleep_until, timeout};
use tonic::transport::Endpoint;

use crate::config::{ConfigError, DaemonConfig};
use crate::contact::Contact;
use crate::lease::{Done, Leases, failure, lease_id, proto_lease_id};
use crate::report::NodeReport;
use crate::runtime::Runtime;

/// The `kbf.worker.v1` protocol version this daemon speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Something the daemon did that an observer (a test, later the meter) may want.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A session began: the server answered Hello.
    Welcomed { heartbeat_interval: Duration },
    /// The server placed a lease here; nothing runs until its Start.
    Offered(LeaseId),
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
    outbox: Vec<worker::Result>,
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
            outbox: Vec::new(),
        })
    }

    /// Sends every [`Event`] to `events` as well as to the log.
    #[must_use]
    pub fn with_events(mut self, events: mpsc::UnboundedSender<Event>) -> Self {
        self.events = Some(events);
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
            let wake = Instant::now() + self.config.reconnect_after;
            self.offline(sleep_until(wake)).await;
        }
    }

    /// One stream, from Hello until it ends.
    async fn session(&mut self) -> Result<(), SessionError> {
        let endpoint = self.endpoint.clone();
        let channel = self.offline(endpoint.connect()).await?;
        let mut client = WorkerClient::new(channel);
        let (tx, rx) = unbounded();
        let hello_sent = Instant::now();
        self.send(&tx, daemon_message::Message::Hello(self.hello()));

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
        if self.contact.confirm(hello_sent) {
            self.emit(Event::ContactRestored);
        }
        tracing::info!(?interval, "welcomed");
        self.emit(Event::Welcomed {
            heartbeat_interval: interval,
        });
        for result in std::mem::take(&mut self.outbox) {
            self.send(&tx, daemon_message::Message::Result(result));
        }

        let mut seq = 0u64;
        let mut beat = tokio::time::interval(interval);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let wake = self.next_check();
            tokio::select! {
                msg = inbound.message() => match msg? {
                    Some(msg) => self.on_message(msg, &tx),
                    None => return Ok(()),
                },
                _ = beat.tick() => {
                    seq += 1;
                    self.contact.sent(seq, Instant::now());
                    let heartbeat = Heartbeat {
                        seq,
                        report_hash: self.report.hash().to_vec(),
                        running: self.leases.running().into_iter().map(proto_lease_id).collect(),
                    };
                    self.send(&tx, daemon_message::Message::Heartbeat(heartbeat));
                }
                Some((id, outcome)) = self.done.recv() => {
                    if let Some(result) = self.leases.finished(id, outcome) {
                        self.send(&tx, daemon_message::Message::Result(result));
                    }
                }
                () = sleep_until(wake) => {
                    for result in self.check(Instant::now()).await {
                        self.send(&tx, daemon_message::Message::Result(result));
                    }
                }
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
            Some(server_message::Message::Start(start)) => {
                let refused = if self.contact.lost(Instant::now()) {
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
                    self.send(tx, daemon_message::Message::Result(result));
                }
            }
            Some(server_message::Message::Welcome(_)) => {
                tracing::warn!("a second Welcome on one stream ignored");
            }
            None => tracing::warn!("an empty server message ignored"),
        }
    }

    /// Declares a heartbeat gap, and fences, when their deadlines have passed. Returns
    /// the Results of fenced leases.
    async fn check(&mut self, now: Instant) -> Vec<worker::Result> {
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

    /// The next instant [`Self::check`] has something to do.
    fn next_check(&self) -> Instant {
        let fence = if self.leases.running().is_empty() {
            None
        } else {
            self.contact.fence_deadline()
        };
        [fence, self.contact.gap_deadline()]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))
    }

    /// Runs `fut` while no stream is up, still finishing leases and fencing on time.
    async fn offline<F: Future>(&mut self, fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        loop {
            let wake = self.next_check();
            tokio::select! {
                out = &mut fut => return out,
                Some((id, outcome)) = self.done.recv() => {
                    if let Some(result) = self.leases.finished(id, outcome) {
                        self.outbox.push(result);
                    }
                }
                () = sleep_until(wake) => {
                    let fenced = self.check(Instant::now()).await;
                    self.outbox.extend(fenced);
                }
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

    /// Queues a message on the stream. A Result that cannot be queued (the stream is
    /// gone) waits in the outbox; anything else is dropped.
    fn send(&mut self, tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
        let msg = DaemonMessage {
            message: Some(message),
        };
        if let Err(e) = tx.unbounded_send(msg)
            && let Some(daemon_message::Message::Result(result)) = e.into_inner().message
        {
            self.outbox.push(result);
        }
    }

    fn emit(&self, event: Event) {
        if let Some(events) = &self.events {
            // An observer that went away does not stop the daemon.
            let _ = events.send(event);
        }
    }
}

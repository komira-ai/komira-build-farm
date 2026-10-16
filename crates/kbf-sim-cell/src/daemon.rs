//! A fake daemon: one process, with the daemon's own rules for contact and epochs and
//! a fake run of each lease.
//!
//! Its contact with the server is a [`Contact`], the daemon's: a heartbeat every
//! interval the `Welcome` named, each recorded when sent and confirmed by its
//! acknowledgement; once contact is lost (T after the newest acknowledged send) every
//! run is killed, and once a gap is declared (two intervals without an
//! acknowledgement) the daemon opens a new stream. A `Welcome` drops the leases of an
//! earlier epoch by [`superseded`], the daemon's rule. A run ends after a few seconds
//! with a completed result; the result is kept until its `ResultAck`, listed in every
//! heartbeat and resent with each, and on every new stream. One run in ten is long. A `Start` is taken only on
//! the current stream, with contact confirmed, for a lease it does not hold.
//!
//! **The daemon half of `kbf-all-in-one`** ([`FakeDaemon::in_process`]) shares the
//! server's process. Its run stores the action's output ([`output_of`]) in the server
//! before it reports, and its result names that output: it reports a run only once the
//! server has acknowledged the output's upload. The process dies with the server's
//! ([`FakeDaemon::die`]: every run, staged output and unacknowledged result is lost)
//! and starts again with it ([`FakeDaemon::revive`]) as a new instance, on a new stream:
//! a message of the dead process's streams means nothing to it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_daemon::{Contact, FENCE_AFTER, Moment, superseded};
use kbf_sim::{Chance, Event, NodeId, NodeInput, Output, SimRng};
use kbf_types::{Digest, Effect, FarmTime, LeaseId, OperationId, Outcome, Resources, StateMachine};

use crate::{Msg, input_of, output_of};

/// How often the daemon checks its runs and its contact.
const POLL: Duration = Duration::from_secs(1);
const T_POLL: u64 = 0;
const T_HEARTBEAT: u64 = 1;
/// How long a run takes: between these, in milliseconds; one in ten takes
/// [`LONG_RUN`], longer than the scheduler's grace G, so a daemon cut off mid-run
/// must fence it before its lease is granted again.
const RUN_MS: (u64, u64) = (1_000, 6_000);
const LONG_RUN: Duration = Duration::from_secs(80);
/// A `Hello` not answered within this is sent again on a new stream.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// An output upload not acknowledged within this is sent again.
const UPLOAD_WAIT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
struct Run {
    epoch: Option<u64>,
    operation: OperationId,
    action: Digest,
    ends: FarmTime,
}

/// A finished run of the in-process half whose output is being stored: its action,
/// and when its upload was last sent.
#[derive(Clone, Copy, Debug)]
struct Staging {
    action: Digest,
    sent: FarmTime,
}

/// The daemon node.
#[derive(Debug)]
pub struct FakeDaemon {
    server: NodeId,
    instance: String,
    /// The instance's name without its incarnation, for [`FakeDaemon::revive`].
    name: String,
    capacity: Resources,
    contact: Contact,
    stream: u64,
    hello_sent: FarmTime,
    welcomed: bool,
    interval: Duration,
    seq: u64,
    epoch: Option<u64>,
    runs: BTreeMap<LeaseId, Run>,
    unacked: BTreeMap<LeaseId, (Digest, Outcome)>,
    /// Every lease this daemon has reported a result for.
    reported: BTreeSet<LeaseId>,
    /// The half of an all-in-one process: it stores each run's output first.
    in_process: bool,
    /// The in-process half's finished runs whose output upload is not acknowledged.
    staging: BTreeMap<LeaseId, Staging>,
    /// Dead with its process: it ignores every input until revived.
    dead: bool,
    /// Revived, and connects at its next poll.
    reviving: bool,
    /// How many times its process has started.
    incarnation: u64,
    /// The runs, staged outputs and unacknowledged results lost with the process.
    lost: usize,
    out: Vec<Output<Msg>>,
}

fn moment(t: FarmTime) -> Moment {
    Moment::from_origin(Duration::from_millis(t.as_millis()))
}

impl FakeDaemon {
    /// The tag of its poll timer, which checks its runs and its contact.
    pub const POLL: u64 = T_POLL;

    /// A daemon of `capacity` that connects to `server`.
    #[must_use]
    pub fn new(server: NodeId, instance: String, capacity: Resources) -> Self {
        Self {
            server,
            name: instance.clone(),
            instance,
            capacity,
            contact: Contact::new(FENCE_AFTER),
            stream: 0,
            hello_sent: FarmTime::default(),
            welcomed: false,
            interval: Duration::from_secs(5),
            seq: 0,
            epoch: None,
            runs: BTreeMap::new(),
            unacked: BTreeMap::new(),
            reported: BTreeSet::new(),
            in_process: false,
            staging: BTreeMap::new(),
            dead: false,
            reviving: false,
            incarnation: 1,
            lost: 0,
            out: Vec::new(),
        }
    }

    /// The daemon half of an all-in-one process whose server half is `server`: its
    /// instance is `<name>-<incarnation>`.
    #[must_use]
    pub fn in_process(server: NodeId, name: &str, capacity: Resources) -> Self {
        let mut daemon = Self::new(server, format!("{name}-1"), capacity);
        daemon.name = name.to_owned();
        daemon.in_process = true;
        daemon
    }

    /// The process died (the server half's did): every run, staged output and
    /// unacknowledged result goes with it, and so does contact. It ignores every input
    /// until [`FakeDaemon::revive`].
    pub fn die(&mut self) {
        self.dead = true;
        self.reviving = false;
        self.lost += self.runs.len() + self.staging.len() + self.unacked.len();
        self.runs.clear();
        self.staging.clear();
        self.unacked.clear();
        self.welcomed = false;
        self.epoch = None;
        self.contact = Contact::new(FENCE_AFTER);
    }

    /// The process started again: a new instance, which connects at its next poll on a
    /// new stream.
    pub fn revive(&mut self) {
        if !self.dead {
            return;
        }
        self.dead = false;
        self.reviving = true;
        self.incarnation += 1;
        self.instance = format!("{}-{}", self.name, self.incarnation);
    }

    /// The server's process went down (`up` false) or started (true): the daemon half
    /// of that process dies or starts with it; any other daemon is not touched.
    pub fn server_process(&mut self, up: bool) {
        if !self.in_process {
            return;
        }
        if up {
            self.revive();
        } else {
            self.die();
        }
    }

    /// The runs, staged outputs and unacknowledged results lost with the process, over
    /// every death.
    #[must_use]
    pub fn lost(&self) -> usize {
        self.lost
    }

    /// The runs going on at `now` and not fenced: the lease of each, and the term and
    /// operation it runs.
    pub fn unfenced_runs(&self, now: FarmTime) -> impl Iterator<Item = (LeaseId, OperationId)> {
        let fenced = self.contact.lost(moment(now));
        self.runs
            .iter()
            .filter(move |_| !fenced)
            .map(|(lease, run)| (*lease, run.operation))
    }

    /// Every lease this daemon has reported a result for.
    #[must_use]
    pub fn reported(&self) -> &BTreeSet<LeaseId> {
        &self.reported
    }

    /// What it has queued since it was last asked: messages and timers.
    pub fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    fn send(&mut self, msg: Msg) {
        let to = self.server.clone();
        self.out.push(Output::Send { to, msg });
    }

    fn connect(&mut self, now: FarmTime) {
        self.stream += 1;
        self.welcomed = false;
        self.hello_sent = now;
        self.send(Msg::Hello {
            stream: self.stream,
            instance: self.instance.clone(),
            capacity: self.capacity,
        });
    }

    fn resend_results(&mut self) {
        let results: Vec<_> = self.unacked.iter().map(|(l, r)| (*l, *r)).collect();
        for (lease, (action, outcome)) in results {
            let stream = self.stream;
            self.send(Msg::Report {
                stream,
                lease,
                action,
                outcome,
            });
        }
    }

    fn heartbeat(&mut self, now: FarmTime) {
        if !self.welcomed {
            return;
        }
        self.seq += 1;
        self.contact.sent(self.seq, moment(now));
        let running = self
            .runs
            .keys()
            .chain(self.staging.keys())
            .chain(self.unacked.keys())
            .copied()
            .collect();
        let (stream, seq) = (self.stream, self.seq);
        self.send(Msg::Heartbeat {
            stream,
            seq,
            running,
        });
        self.resend_results();
    }

    fn poll(&mut self, now: FarmTime) {
        if self.contact.lost(moment(now)) {
            // Fenced: every run is killed; its result is never sent.
            self.runs.clear();
            self.staging.clear();
        }
        let gap = self.contact.check_gap(moment(now)).is_some();
        let unanswered =
            !self.welcomed && now.saturating_duration_since(self.hello_sent) >= HELLO_WAIT;
        if std::mem::take(&mut self.reviving) || gap || unanswered {
            self.connect(now);
        }
        self.resend_uploads(now);
        let ended: Vec<LeaseId> = self
            .runs
            .iter()
            .filter(|(_, run)| run.ends <= now)
            .map(|(lease, _)| *lease)
            .collect();
        for lease in ended {
            let run = self.runs.remove(&lease).expect("listed above");
            if self.in_process {
                let output = output_of(&run.action);
                let staging = Staging {
                    action: run.action,
                    sent: now,
                };
                self.staging.insert(lease, staging);
                self.send(Msg::Upload { digest: output });
            } else {
                self.report(lease, run.action, input_of(&run.action));
            }
        }
    }

    /// Reports `lease`'s run of `action` completed, its result `result`; keeps the
    /// result until its `ResultAck`.
    fn report(&mut self, lease: LeaseId, action: Digest, result: Digest) {
        let outcome = Outcome::Completed {
            action_result: result,
        };
        self.unacked.insert(lease, (action, outcome));
        self.reported.insert(lease);
        let stream = self.stream;
        self.send(Msg::Report {
            stream,
            lease,
            action,
            outcome,
        });
    }

    /// Sends again each output upload not acknowledged within [`UPLOAD_WAIT`].
    fn resend_uploads(&mut self, now: FarmTime) {
        let mut again = Vec::new();
        for staging in self.staging.values_mut() {
            if now.saturating_duration_since(staging.sent) >= UPLOAD_WAIT {
                staging.sent = now;
                again.push(output_of(&staging.action));
            }
        }
        for digest in again {
            self.send(Msg::Upload { digest });
        }
    }

    /// The server stored `digest`: each staged run whose output it is reports now.
    fn stored(&mut self, digest: Digest) {
        let staged: Vec<(LeaseId, Digest)> = self
            .staging
            .iter()
            .filter(|(_, s)| output_of(&s.action) == digest)
            .map(|(lease, s)| (*lease, s.action))
            .collect();
        for (lease, action) in staged {
            self.staging.remove(&lease);
            self.report(lease, action, digest);
        }
    }

    /// Takes the epoch a `Welcome` named, and drops each lease granted under another.
    fn new_epoch(&mut self, epoch: u64) {
        self.epoch = Some(epoch);
        let newest = self.epoch;
        self.runs.retain(|_, run| !superseded(run.epoch, newest));
    }

    fn receive(&mut self, now: FarmTime, msg: Msg, entropy: u64) {
        match msg {
            Msg::Welcome {
                stream,
                epoch,
                interval_ms,
            } if stream == self.stream && !self.welcomed => {
                self.welcomed = true;
                self.interval = Duration::from_millis(interval_ms);
                self.contact.new_stream(self.interval);
                self.contact.confirm(moment(self.hello_sent));
                self.new_epoch(epoch);
                self.resend_results();
            }
            Msg::HeartbeatAck { stream, seq } if stream == self.stream => {
                self.contact.acknowledged(seq);
            }
            Msg::Start {
                stream,
                lease,
                operation,
                action,
            } if stream == self.stream && self.welcomed => {
                let held = self.runs.contains_key(&lease)
                    || self.staging.contains_key(&lease)
                    || self.unacked.contains_key(&lease);
                if held || self.contact.lost(moment(now)) || self.reported.contains(&lease) {
                    return;
                }
                let mut rng = SimRng::from_seed(entropy);
                let run = if rng.chance(Chance::percent(10)) {
                    LONG_RUN
                } else {
                    Duration::from_millis(rng.between(RUN_MS.0, RUN_MS.1))
                };
                let ends = now.saturating_add(run);
                let epoch = self.epoch;
                self.runs.insert(
                    lease,
                    Run {
                        epoch,
                        operation,
                        action,
                        ends,
                    },
                );
            }
            Msg::ResultAck { lease, .. } => {
                self.unacked.remove(&lease);
            }
            Msg::Cancel { stream, lease } if stream == self.stream => {
                self.runs.remove(&lease);
                self.staging.remove(&lease);
            }
            Msg::Uploaded { digest } => self.stored(digest),
            Msg::Reset { stream } if stream == self.stream => self.connect(now),
            _ => {}
        }
    }
}

impl StateMachine for FakeDaemon {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        // A dead process does nothing; its timers keep firing, standing for the next
        // process's.
        let dead = self.dead;
        match input.event {
            Event::Start => {
                self.connect(now);
                self.out.push(Output::Timer {
                    after: POLL,
                    tag: T_POLL,
                });
                self.out.push(Output::Timer {
                    after: self.interval,
                    tag: T_HEARTBEAT,
                });
            }
            Event::Timer { tag: T_POLL } => {
                if !dead {
                    self.poll(now);
                }
                self.out.push(Output::Timer {
                    after: POLL,
                    tag: T_POLL,
                });
            }
            Event::Timer { .. } => {
                if !dead {
                    self.heartbeat(now);
                }
                self.out.push(Output::Timer {
                    after: self.interval,
                    tag: T_HEARTBEAT,
                });
            }
            Event::Message { .. } if dead => {}
            Event::Message { msg, .. } => self.receive(now, msg, input.entropy),
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use kbf_types::OperationId;

    use super::*;
    use crate::action;

    fn input(at: u64, event: Event<Msg>) -> NodeInput<Msg> {
        NodeInput {
            now: FarmTime::from_millis(at),
            entropy: 1,
            event,
        }
    }

    fn message(at: u64, msg: Msg) -> NodeInput<Msg> {
        input(
            at,
            Event::Message {
                from: NodeId::from("server"),
                msg,
            },
        )
    }

    fn start(stream: u64, seq: u64) -> Msg {
        Msg::Start {
            stream,
            lease: LeaseId::new(1, seq),
            operation: OperationId(seq),
            action: action(seq),
        }
    }

    fn welcome(stream: u64) -> Msg {
        Msg::Welcome {
            stream,
            epoch: 7,
            interval_ms: 5_000,
        }
    }

    fn runs(d: &FakeDaemon) -> Vec<LeaseId> {
        d.unfenced_runs(FarmTime::from_millis(10))
            .map(|(l, _)| l)
            .collect()
    }

    /// Catches a daemon that takes a `Start` before its stream is welcomed, on an
    /// earlier stream, or again for a lease it has reported, and one that takes a
    /// `Welcome` of an earlier stream as its own.
    #[test]
    fn a_start_is_taken_once_on_the_welcomed_stream_only() {
        let mut d = FakeDaemon::new(
            NodeId::from("server"),
            "d-1".to_owned(),
            Resources::new(1_000, 1),
        );
        d.apply(input(0, Event::Start));
        d.apply(message(1, start(1, 1)));
        assert!(runs(&d).is_empty(), "taken before the Welcome");
        d.apply(message(2, welcome(0)));
        d.apply(message(3, start(1, 1)));
        assert!(runs(&d).is_empty(), "a Welcome of another stream was taken");
        d.apply(message(4, welcome(1)));
        d.apply(message(5, start(0, 2)));
        assert!(runs(&d).is_empty(), "taken on an earlier stream");
        d.apply(message(6, start(1, 1)));
        assert_eq!(runs(&d), [LeaseId::new(1, 1)]);
        // The run ends and is reported; a late copy of its `Start` runs nothing.
        d.apply(input(30_000, Event::Timer { tag: T_POLL }));
        assert!(d.reported().contains(&LeaseId::new(1, 1)));
        d.apply(message(30_001, start(1, 1)));
        assert!(runs(&d).is_empty(), "a reported lease ran again");
    }
}

//! A record as `kbf.invocation.v1` messages: its [`proto::Header`] and its rows as a
//! [`proto::Chunk`].

use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, LostWhy};

use crate::event::{Outcome, WallTime};
use crate::proto;
use crate::record::{ActionRow, Attempt, AttemptEnd, Counts, How, Invocation, Text};

impl Invocation {
    /// The record's header (section 3.1).
    #[must_use]
    pub fn header(&self) -> proto::Header {
        let c: &Counts = &self.counts;
        proto::Header {
            key: self.key.to_string(),
            tool: self.tool.clone().unwrap_or_default(),
            tool_version: self.tool_version.clone().unwrap_or_default(),
            correlated_invocations_id: self.correlated_invocations_id.clone().unwrap_or_default(),
            owner: self.owner.0.clone(),
            instances: self.instances.iter().cloned().collect(),
            first_seen_micros: self.first_seen.unix_micros(),
            last_seen_micros: self
                .last_seen
                .unix_micros()
                .saturating_sub(self.first_seen.unix_micros()),
            minted: self.minted,
            incomplete: !self.complete,
            rows_capped: self.rows_capped,
            counts: Some(proto::Counts {
                execute: c.execute,
                cache_hit_execute: c.cache_hit_execute,
                joined: c.joined,
                ran: c.ran,
                failed: c.failed,
                ac_hit: c.ac_hit,
                ac_miss: c.ac_miss,
                blobs_asked: c.blobs_asked,
                blobs_missing: c.blobs_missing,
                bytes_uploaded: c.bytes_uploaded,
                bytes_downloaded: c.bytes_downloaded,
            }),
            collisions: self.collisions.iter().map(ToString::to_string).collect(),
        }
    }

    /// Its rows from index `from` on, as one chunk, each time an offset from
    /// `first_seen`.
    #[must_use]
    pub fn chunk(&self, from: usize) -> proto::Chunk {
        let base = self.first_seen;
        proto::Chunk {
            actions: self.rows.iter().skip(from).map(|r| row(r, base)).collect(),
        }
    }
}

fn row(r: &ActionRow, base: WallTime) -> proto::Action {
    let listing = r.listing.as_ref();
    proto::Action {
        action: Some(digest(r.action)),
        instance: r.instance.clone(),
        target: r.target.as_ref().map(text),
        mnemonic: r.mnemonic.as_ref().map(text),
        configuration: r.configuration.as_ref().map(text),
        action_id: r.action_id.as_ref().map(text),
        how: match r.how {
            How::CacheHit => proto::How::CacheHit,
            How::CacheHitExecute => proto::How::CacheHitExecute,
            How::Joined => proto::How::Joined,
            How::Ran => proto::How::Ran,
        }
        .into(),
        operation: r.operation.clone().unwrap_or_default(),
        attempts: r.attempts.iter().map(|a| attempt(a, base)).collect(),
        outcome: r.outcome.as_ref().map(outcome),
        result: listing.and_then(|l| l.result).map(digest),
        command: r.command.map(digest),
        outputs: listing
            .map(|l| {
                l.outputs
                    .iter()
                    .map(|o| proto::Output {
                        path: o.path.clone(),
                        digest: Some(digest(o.digest)),
                        executable: o.executable,
                        directory: o.directory,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        outputs_truncated: r.outputs_truncated,
        stdout: listing.and_then(|l| l.stdout).map(digest),
        stderr: listing.and_then(|l| l.stderr).map(digest),
        usage: listing.and_then(|l| l.usage).map(|u| proto::Usage {
            cpu_user_micros: u.cpu_user_micros,
            cpu_system_micros: u.cpu_system_micros,
            peak_memory_bytes: u.peak_memory_bytes,
            wall_micros: u.wall_micros,
        }),
    }
}

/// `t` in microseconds after `base`; negative when earlier.
const fn offset(t: WallTime, base: WallTime) -> i64 {
    t.unix_micros()
        .wrapping_sub(base.unix_micros())
        .cast_signed()
}

fn attempt(a: &Attempt, base: WallTime) -> proto::Attempt {
    use proto::attempt_end::End;
    proto::Attempt {
        lease_term: a.lease.term,
        lease_seq: a.lease.seq,
        worker: a.worker.as_str().to_owned(),
        queued: offset(a.queued, base),
        granted: offset(a.granted, base),
        started: a.started.map(|t| offset(t, base)),
        ended: a.ended.map(|t| offset(t, base)),
        end: a.end.as_ref().map(|e| proto::AttemptEnd {
            end: Some(match e {
                AttemptEnd::Accepted => End::Result(proto::Accepted {}),
                AttemptEnd::Requeued(r) => End::Requeue(requeue(*r)),
                AttemptEnd::Failed(f, reason) => End::Failed(proto::AttemptFailure {
                    class: failure(*f).into(),
                    reason: reason.clone(),
                }),
            }),
        }),
    }
}

fn requeue(r: RequeueReason) -> proto::Requeue {
    use proto::RequeueReason as P;
    let (reason, booked, raised) = match r {
        RequeueReason::Silent => (P::Silent, 0, 0),
        RequeueReason::Replaced => (P::Replaced, 0, 0),
        RequeueReason::Reconnected => (P::Reconnected, 0, 0),
        RequeueReason::NotStarted => (P::NotStarted, 0, 0),
        RequeueReason::SessionSwitch => (P::SessionSwitch, 0, 0),
        RequeueReason::OutOfMemory { booked, raised } => (P::OutOfMemory, booked, raised),
        RequeueReason::NodeMemoryPressure => (P::NodeMemoryPressure, 0, 0),
        RequeueReason::Restarted => (P::Restarted, 0, 0),
        RequeueReason::Lost(LostWhy::DaemonRestarted) => (P::LostDaemonRestarted, 0, 0),
        RequeueReason::Lost(LostWhy::Fenced) => (P::LostFenced, 0, 0),
        RequeueReason::Infra => (P::Infra, 0, 0),
    };
    proto::Requeue {
        reason: reason.into(),
        booked,
        raised,
    }
}

const fn failure(f: Failure) -> proto::FailureClass {
    use proto::FailureClass as P;
    match f {
        Failure::Timeout => P::Timeout,
        Failure::Infra => P::Infra,
        Failure::Invalid => P::Invalid,
        Failure::OutOfMemory => P::OutOfMemory,
        Failure::NodeMemoryPressure => P::NodeMemoryPressure,
        Failure::MissingInput => P::MissingInput,
        Failure::Lost(LostWhy::DaemonRestarted) => P::LostDaemonRestarted,
        Failure::Lost(LostWhy::Fenced) => P::LostFenced,
    }
}

fn outcome(o: &Outcome) -> proto::Outcome {
    use proto::outcome::Outcome as P;
    proto::Outcome {
        outcome: Some(match o {
            Outcome::Ok { exit_code } => P::Ok(proto::Exited {
                exit_code: *exit_code,
            }),
            Outcome::Failed { code, message } => P::Failed(proto::Status {
                code: *code,
                message: message.clone(),
            }),
            Outcome::TimedOut => P::TimedOut(proto::TimedOut {}),
            Outcome::Refused(reason) => P::Refused(reason.clone()),
        }),
    }
}

fn text(t: &Text) -> proto::Text {
    proto::Text {
        value: t.value.clone(),
        truncated: t.truncated,
    }
}

fn digest(d: Digest) -> proto::Digest {
    proto::Digest {
        sha256: d.hash.to_vec(),
        size_bytes: d.size_bytes,
    }
}

//! The invocation record (docs/design/invocations.md, section 3): what the farm did for
//! one run of a build tool, built only from events.
//!
//! An [`Event`] is a call that carried an invocation id, or something the farm did for
//! an operation such a call queued or joined: a lease granted, started, requeued, the
//! operation finished. [`Records::apply`] routes each to its record: a call by its id
//! and caller (a caller whose authentication metadata differs from the first one's gets
//! its own record, `<id>~<n>`, section 2.4), a lease event to every record whose
//! Execute waits on that operation. An [`Invocation`] holds the header fields and
//! counters of section 3.1 and one [`ActionRow`] per action digest, in the order first
//! seen (section 3.2); a row copies its result's listing (outputs, stdout and stderr
//! digests, usage), so a page can list them after the result blob is gone.
//!
//! [`Invocation::header`] and [`Invocation::chunk`] give the record as
//! `kbf.invocation.v1` messages ([`proto`]), the form section 3.5 stores.
//!
//! [`Records::with_bound`] holds every record, and the operations their calls wait on,
//! within a memory bound after every event: it removes minted, then idle records,
//! then closes the largest open one to new rows and leaves out what still does not
//! fit (section 3.5, stage 1).
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. Wall times come in with the events. The recorder that feeds it
//! (`kbf_front::Recorder`, its channel and its task) and the read API are not here.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod encode;
mod event;
mod metrics;
mod record;
mod records;
mod run;

/// The generated `kbf.invocation.v1` messages.
#[allow(clippy::all, missing_docs)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/kbf.invocation.v1.rs"));
}

pub use event::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Listing, Metadata,
    Outcome, Output, Owner, Usage, WallTime,
};
pub use metrics::{Histogram, Metrics, REQUEUE_NAMES, failure_name, requeue_name};
pub use record::{
    ActionRow, Attempt, AttemptEnd, Counts, How, IDLE_AFTER, Invocation, OUTPUTS_PER_ROW,
    REASON_CAP, ROW_CAP, RecordKey, Restored, State, TEXT_CAP, Text,
};
pub use records::{Ended, EndedKind, Records};
pub use run::{
    ActionShape, Durations, MemoryKillKind, NumaBytes, Phase, ReapiTimes, Run, RunUsage, Target,
    TargetKind,
};

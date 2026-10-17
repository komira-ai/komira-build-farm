//! The control commands of the replicated log (docs/design/ha.md, section 4.1): the
//! farm machine's inputs, batched ([`FarmBatch`]), a worker's result with its
//! action-cache entry ([`Report`]), a new leader's first command ([`Takeover`]), a
//! determinism check ([`Checkpoint`]) and a machine-version raise ([`MachineVersion`]),
//! and their conversions to and from the `kbf.log.v1` messages.
//!
//! One machine input is not here yet: a node's newest `NodeStatus`, whose value
//! (`SoftwareView`) is kbf-server's own type.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::time::{Duration, SystemTime};

use kbf_caps::{Arch, Consumable, CpuCaps, NodeCaps};
use kbf_meta::ActionRecord;
use kbf_proto::kbf::log::v1 as proto;
use kbf_sched::{DaemonInstance, Request};
use kbf_types::{
    ActionKey, ControlRecord, Digest, Failure, FarmTime, LeaseGrant, LeaseId, LeaseKind, LostWhy,
    OperationId, Outcome, Qos, RefusalRecord, Resources, ResultRecord, WorkerId,
};
use proto::farm_input::Input as P;

use crate::Format;
use crate::meta::{action_record, digest, from_action_record, from_digest, required_digest};
use crate::{DecodeError, EncodeError};

/// Scheduler and farm inputs the leader batched, all at farm time `now`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FarmBatch {
    /// The farm time of every input.
    pub now: FarmTime,
    /// The leader's wall clock when it formed the batch: every wall-clock time the farm
    /// machine records while it applies the batch (a caller's queue time, a `Start`'s
    /// send time). Encoded as nanoseconds since the Unix epoch, so it must be at or
    /// after the epoch and within `u64` nanoseconds of it.
    pub wall: SystemTime,
    /// The inputs, in the order they apply.
    pub inputs: Vec<FarmInput>,
}

/// One input of a [`FarmBatch`]: a scheduler event (`kbf_sched::Event`, but for
/// `Report`, which is [`LogCommand::Report`](crate::LogCommand::Report)), or one of
/// the farm machine's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FarmInput {
    /// A worker registered.
    WorkerUp {
        /// The worker.
        worker: WorkerId,
        /// The daemon process that registered it.
        instance: DaemonInstance,
        /// The server found the daemon process before it stopped, its runs killed.
        predecessor_stopped: bool,
        /// What placement may book on it.
        capacity: Resources,
        /// What it offers.
        caps: NodeCaps,
    },
    /// A registered worker's capacity or capabilities changed on the same stream.
    Capacity {
        /// The worker.
        worker: WorkerId,
        /// What placement may book on it from now on.
        capacity: Resources,
        /// What it offers from now on.
        caps: NodeCaps,
    },
    /// A worker's heartbeat.
    Heartbeat {
        /// The worker.
        worker: WorkerId,
        /// The leases it holds, in the order it listed them.
        running: Vec<LeaseId>,
    },
    /// A caller asks for a request to run. It names no waiter: the farm machine assigns
    /// the caller's id when it applies the input.
    Submit(Request),
    /// A record the scheduler asked to commit.
    Committed(ControlRecord),
    /// The worker holding `lease` started `operation`.
    Started {
        /// The operation.
        operation: OperationId,
        /// The lease.
        lease: LeaseId,
    },
    /// A placement round and lease expiry.
    Tick,
    /// An operator cordons the worker.
    Cordon(WorkerId),
    /// An operator drains `worker`, pausing at `deadline`.
    Drain {
        /// The worker.
        worker: WorkerId,
        /// When the drain pauses if leases still run.
        deadline: FarmTime,
    },
    /// An operator returns the worker to placement.
    Uncordon(WorkerId),
    /// The worker's newest stream closed.
    Disconnected(WorkerId),
}

/// A worker's result at farm time `now`, with the action-cache entry it writes if the
/// scheduler accepts it (docs/design/ha.md, section 4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The farm time.
    pub now: FarmTime,
    /// The operation.
    pub operation: OperationId,
    /// The lease the worker holds.
    pub lease: LeaseId,
    /// What happened.
    pub outcome: Outcome,
    /// The entry written as a daemon's `PutAction`; `None` when the result writes
    /// none (a failure, an uncached action).
    pub action_entry: Option<ActionEntry>,
}

/// The action-cache entry of a [`Report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    /// The action digest (the key).
    pub action: Digest,
    /// The result and its closure.
    pub record: ActionRecord,
}

/// A new leader's first command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Takeover {
    /// The leader's Raft term.
    pub term: NonZeroU64,
    /// The farm time it starts from.
    pub now: FarmTime,
}

/// The SHA-256 digest of the leader's state at log index `index`
/// (docs/design/ha.md, section 4.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The log index the digest is of.
    pub index: u64,
    /// The digest.
    pub digest: [u8; 32],
}

/// A machine version (docs/design/ha.md, section 4.7): which apply behavior runs.
/// Never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineVersion(NonZeroU32);

impl MachineVersion {
    /// Version 1: the metadata log's first apply behavior, whose entries are
    /// [`Format::V1`].
    pub const V1: Self = Self(NonZeroU32::MIN);

    /// Version 2: version 1 plus the write tier's metadata commands, whose entries are
    /// [`Format::V2`] (docs/design/write-path.md, section 6.5). A log enters it by a
    /// committed `MachineVersion` entry naming it; every later entry is format 2.
    pub const V2: Self = Self(NonZeroU32::MIN.saturating_add(1));

    /// Version 3: version 2 plus `EvictObjects`, whose entries are [`Format::V3`]. A log
    /// enters it as it enters version 2.
    pub const V3: Self = Self(NonZeroU32::MIN.saturating_add(2));

    /// Version 4: version 3 plus node writers (`SetWriter`, and objects named by a
    /// storage node's member and boot), whose entries are [`Format::V4`]. A log enters it
    /// as it enters version 2.
    pub const V4: Self = Self(NonZeroU32::MIN.saturating_add(3));

    /// The entry format a log at this version writes and reads; `None` for a version
    /// this binary does not know (a later binary's).
    #[must_use]
    pub const fn format(self) -> Option<Format> {
        match self.0.get() {
            1 => Some(Format::V1),
            2 => Some(Format::V2),
            3 => Some(Format::V3),
            4 => Some(Format::V4),
            _ => None,
        }
    }

    /// Version `n`.
    #[must_use]
    pub const fn new(n: NonZeroU32) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

// ---- Encoding. ----

pub(crate) fn batch_to_proto(batch: &FarmBatch) -> Result<proto::FarmBatch, EncodeError> {
    let wall_time_ns = batch
        .wall
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .ok_or(EncodeError::WallTime)?;
    Ok(proto::FarmBatch {
        farm_time_ms: batch.now.as_millis(),
        wall_time_ns,
        inputs: batch.inputs.iter().map(input).collect::<Result<_, _>>()?,
    })
}

fn input(input: &FarmInput) -> Result<proto::FarmInput, EncodeError> {
    let i = match input {
        FarmInput::WorkerUp {
            worker,
            instance,
            predecessor_stopped,
            capacity,
            caps,
        } => P::WorkerUp(proto::WorkerUp {
            worker: worker.as_str().to_owned(),
            instance: instance.as_str().to_owned(),
            predecessor_stopped: *predecessor_stopped,
            capacity: Some(resources(capacity)),
            caps: Some(node_caps(caps)),
        }),
        FarmInput::Capacity {
            worker,
            capacity,
            caps,
        } => P::Capacity(proto::Capacity {
            worker: worker.as_str().to_owned(),
            capacity: Some(resources(capacity)),
            caps: Some(node_caps(caps)),
        }),
        FarmInput::Heartbeat { worker, running } => P::Heartbeat(proto::Heartbeat {
            worker: worker.as_str().to_owned(),
            running: running.iter().copied().map(lease_id).collect(),
        }),
        FarmInput::Submit(r) => P::Submit(proto::Submit {
            request: Some(request(r)?),
        }),
        FarmInput::Committed(r) => P::Committed(record(r)?),
        FarmInput::Started { operation, lease } => P::Started(proto::Started {
            operation: operation.0,
            lease: Some(lease_id(*lease)),
        }),
        FarmInput::Tick => P::Tick(proto::SchedulerTick {}),
        FarmInput::Cordon(w) => P::Cordon(proto::Cordon {
            worker: w.as_str().to_owned(),
        }),
        FarmInput::Drain { worker, deadline } => P::Drain(proto::Drain {
            worker: worker.as_str().to_owned(),
            deadline_ms: deadline.as_millis(),
        }),
        FarmInput::Uncordon(w) => P::Uncordon(proto::Uncordon {
            worker: w.as_str().to_owned(),
        }),
        FarmInput::Disconnected(w) => P::Disconnected(proto::Disconnected {
            worker: w.as_str().to_owned(),
        }),
    };
    Ok(proto::FarmInput { input: Some(i) })
}

const fn resources(r: &Resources) -> proto::Resources {
    proto::Resources {
        cpu_millis: r.cpu_millis,
        memory_bytes: r.memory_bytes,
        gpus: r.gpus,
        vms: r.vms,
    }
}

fn property((key, value): (&str, String)) -> proto::Property {
    proto::Property {
        key: key.to_owned(),
        value,
    }
}

fn node_caps(caps: &NodeCaps) -> proto::NodeCaps {
    proto::NodeCaps {
        cpu: Some(proto::CpuCaps {
            arch: match caps.cpu.arch() {
                Arch::X86_64 => proto::Arch::X8664,
                Arch::Arm64 => proto::Arch::Arm64,
            }
            .into(),
            features: caps.cpu.features().iter().cloned().collect(),
        }),
        exact: caps
            .exact
            .iter()
            .map(|(k, v)| property((k.as_str(), v.clone())))
            .collect(),
        members: caps
            .members
            .iter()
            .map(|(key, values)| proto::MemberSet {
                key: key.clone(),
                values: values.iter().cloned().collect(),
            })
            .collect(),
        consumables: caps
            .consumables
            .iter()
            .map(|(&c, &amount)| proto::ConsumableAmount {
                consumable: match c {
                    Consumable::Cpus => proto::Consumable::Cpus,
                    Consumable::MemGib => proto::Consumable::MemGib,
                    Consumable::NvmeGib => proto::Consumable::NvmeGib,
                    Consumable::Gpus => proto::Consumable::Gpus,
                }
                .into(),
                amount,
            })
            .collect(),
        drivers: caps.drivers.iter().cloned().collect(),
        devices: caps
            .devices
            .iter()
            .map(|(id, attributes)| proto::Device {
                id: id.clone(),
                attributes: attributes
                    .iter()
                    .map(|(k, v)| property((k.as_str(), v.clone())))
                    .collect(),
            })
            .collect(),
    }
}

fn request(r: &Request) -> Result<proto::Request, EncodeError> {
    Ok(proto::Request {
        key: Some(proto::ActionKey {
            instance: r.key.instance.clone(),
            action: Some(digest(&r.key.action)?),
        }),
        qos: Some(qos(&r.qos)),
        kind: lease_kind(r.kind).into(),
        resources: Some(resources(&r.resources)),
        needs: r.needs.properties().into_iter().map(property).collect(),
        hermetic: r.hermetic,
        do_not_cache: r.do_not_cache,
    })
}

fn qos(q: &Qos) -> proto::Qos {
    use proto::qos::Level;
    let level = match q {
        Qos::Interactive => Level::Builtin(proto::BuiltinQos::Interactive.into()),
        Qos::Ci => Level::Builtin(proto::BuiltinQos::Ci.into()),
        Qos::Batch => Level::Builtin(proto::BuiltinQos::Batch.into()),
        Qos::Custom(_) => Level::Custom(proto::CustomQos {
            name: q.name().to_owned(),
            urgency: u32::from(q.urgency()),
        }),
    };
    proto::Qos { level: Some(level) }
}

const fn lease_kind(kind: LeaseKind) -> proto::LeaseKind {
    match kind {
        LeaseKind::Action => proto::LeaseKind::Action,
        LeaseKind::WholeMachine => proto::LeaseKind::WholeMachine,
        LeaseKind::Vm => proto::LeaseKind::Vm,
    }
}

const fn lease_id(lease: LeaseId) -> proto::LeaseId {
    proto::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

fn record(r: &ControlRecord) -> Result<proto::ControlRecord, EncodeError> {
    use proto::control_record::Record;
    let record = match r {
        ControlRecord::Lease(g) => Record::Lease(proto::LeaseGrant {
            lease: Some(lease_id(g.lease)),
            operation: g.operation.0,
            worker: g.worker.as_str().to_owned(),
        }),
        ControlRecord::Result(r) => Record::Result(proto::ResultRecord {
            lease: Some(lease_id(r.lease)),
            operation: r.operation.0,
            outcome: Some(outcome(&r.outcome)?),
        }),
        ControlRecord::Refusal(r) => Record::Refusal(proto::RefusalRecord {
            operation: r.operation.0,
            reason: r.reason.clone(),
        }),
        _ => return Err(EncodeError::Record),
    };
    Ok(proto::ControlRecord {
        record: Some(record),
    })
}

fn outcome(o: &Outcome) -> Result<proto::Outcome, EncodeError> {
    use proto::outcome::Outcome as O;
    let o = match o {
        Outcome::Completed { action_result } => O::Completed(digest(action_result)?),
        Outcome::Failed(f) => O::Failed(
            match f {
                Failure::Timeout => proto::Failure::Timeout,
                Failure::Infra => proto::Failure::Infra,
                Failure::Invalid => proto::Failure::Invalid,
                Failure::OutOfMemory => proto::Failure::OutOfMemory,
                Failure::NodeMemoryPressure => proto::Failure::NodeMemoryPressure,
                Failure::MissingInput => proto::Failure::MissingInput,
                Failure::Lost(LostWhy::DaemonRestarted) => proto::Failure::LostDaemonRestarted,
                Failure::Lost(LostWhy::Fenced) => proto::Failure::LostFenced,
            }
            .into(),
        ),
    };
    Ok(proto::Outcome { outcome: Some(o) })
}

pub(crate) fn report_to_proto(r: &Report) -> Result<proto::Report, EncodeError> {
    Ok(proto::Report {
        farm_time_ms: r.now.as_millis(),
        operation: r.operation.0,
        lease: Some(lease_id(r.lease)),
        outcome: Some(outcome(&r.outcome)?),
        action_entry: r.action_entry.as_ref().map(entry).transpose()?,
    })
}

fn entry(e: &ActionEntry) -> Result<proto::ActionEntry, EncodeError> {
    Ok(proto::ActionEntry {
        action: Some(digest(&e.action)?),
        record: Some(action_record(&e.record)?),
    })
}

pub(crate) const fn takeover_to_proto(t: &Takeover) -> proto::Takeover {
    proto::Takeover {
        term: t.term.get(),
        farm_time_ms: t.now.as_millis(),
    }
}

pub(crate) fn checkpoint_to_proto(c: &Checkpoint) -> proto::Checkpoint {
    proto::Checkpoint {
        index: c.index,
        digest: c.digest.to_vec(),
    }
}

// ---- Decoding. ----

pub(crate) fn batch_from_proto(b: proto::FarmBatch) -> Result<FarmBatch, DecodeError> {
    Ok(FarmBatch {
        now: FarmTime::from_millis(b.farm_time_ms),
        wall: SystemTime::UNIX_EPOCH + Duration::from_nanos(b.wall_time_ns),
        inputs: b
            .inputs
            .into_iter()
            .map(from_input)
            .collect::<Result<_, _>>()?,
    })
}

fn from_input(input: proto::FarmInput) -> Result<FarmInput, DecodeError> {
    Ok(
        match input
            .input
            .ok_or(DecodeError::UnknownCommand("FarmInput"))?
        {
            P::WorkerUp(w) => FarmInput::WorkerUp {
                worker: WorkerId::new(w.worker),
                instance: DaemonInstance::new(w.instance),
                predecessor_stopped: w.predecessor_stopped,
                capacity: from_resources(w.capacity, "WorkerUp.capacity")?,
                caps: from_node_caps(w.caps, "WorkerUp.caps")?,
            },
            P::Capacity(c) => FarmInput::Capacity {
                worker: WorkerId::new(c.worker),
                capacity: from_resources(c.capacity, "Capacity.capacity")?,
                caps: from_node_caps(c.caps, "Capacity.caps")?,
            },
            P::Heartbeat(h) => FarmInput::Heartbeat {
                worker: WorkerId::new(h.worker),
                running: h.running.into_iter().map(from_lease_id).collect(),
            },
            P::Submit(s) => FarmInput::Submit(from_request(
                s.request.ok_or(DecodeError::Missing("Submit.request"))?,
            )?),
            P::Committed(r) => FarmInput::Committed(from_record(r)?),
            P::Started(s) => FarmInput::Started {
                operation: OperationId(s.operation),
                lease: required_lease_id(s.lease, "Started.lease")?,
            },
            P::Tick(proto::SchedulerTick {}) => FarmInput::Tick,
            P::Cordon(c) => FarmInput::Cordon(WorkerId::new(c.worker)),
            P::Drain(d) => FarmInput::Drain {
                worker: WorkerId::new(d.worker),
                deadline: FarmTime::from_millis(d.deadline_ms),
            },
            P::Uncordon(u) => FarmInput::Uncordon(WorkerId::new(u.worker)),
            P::Disconnected(d) => FarmInput::Disconnected(WorkerId::new(d.worker)),
        },
    )
}

fn from_resources(
    r: Option<proto::Resources>,
    field: &'static str,
) -> Result<Resources, DecodeError> {
    let r = r.ok_or(DecodeError::Missing(field))?;
    Ok(Resources {
        cpu_millis: r.cpu_millis,
        memory_bytes: r.memory_bytes,
        gpus: r.gpus,
        vms: r.vms,
    })
}

fn from_node_caps(
    caps: Option<proto::NodeCaps>,
    field: &'static str,
) -> Result<NodeCaps, DecodeError> {
    let caps = caps.ok_or(DecodeError::Missing(field))?;
    let cpu = caps.cpu.ok_or(DecodeError::Missing("NodeCaps.cpu"))?;
    let arch = match proto::Arch::try_from(cpu.arch) {
        Ok(proto::Arch::X8664) => Arch::X86_64,
        Ok(proto::Arch::Arm64) => Arch::Arm64,
        Ok(proto::Arch::Unspecified) | Err(_) => {
            return Err(DecodeError::UnknownValue {
                field: "CpuCaps.arch",
                value: cpu.arch,
            });
        }
    };
    // Maps and sets are collected as given; `decode`'s re-encoding refuses any that
    // were out of order or repeated a key.
    Ok(NodeCaps {
        cpu: CpuCaps::new(arch, cpu.features.into_iter().collect()),
        exact: caps.exact.into_iter().map(|p| (p.key, p.value)).collect(),
        members: caps
            .members
            .into_iter()
            .map(|m| (m.key, m.values.into_iter().collect::<BTreeSet<_>>()))
            .collect(),
        consumables: caps
            .consumables
            .into_iter()
            .map(from_consumable)
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        drivers: caps.drivers.into_iter().collect(),
        devices: caps
            .devices
            .into_iter()
            .map(|d| {
                (
                    d.id,
                    d.attributes.into_iter().map(|p| (p.key, p.value)).collect(),
                )
            })
            .collect(),
    })
}

fn from_consumable(c: proto::ConsumableAmount) -> Result<(Consumable, u64), DecodeError> {
    let consumable = match proto::Consumable::try_from(c.consumable) {
        Ok(proto::Consumable::Cpus) => Consumable::Cpus,
        Ok(proto::Consumable::MemGib) => Consumable::MemGib,
        Ok(proto::Consumable::NvmeGib) => Consumable::NvmeGib,
        Ok(proto::Consumable::Gpus) => Consumable::Gpus,
        Ok(proto::Consumable::Unspecified) | Err(_) => {
            return Err(DecodeError::UnknownValue {
                field: "ConsumableAmount.consumable",
                value: c.consumable,
            });
        }
    };
    Ok((consumable, c.amount))
}

fn from_request(r: proto::Request) -> Result<Request, DecodeError> {
    let key = r.key.ok_or(DecodeError::Missing("Request.key"))?;
    let kind = match proto::LeaseKind::try_from(r.kind) {
        Ok(proto::LeaseKind::Action) => LeaseKind::Action,
        Ok(proto::LeaseKind::WholeMachine) => LeaseKind::WholeMachine,
        Ok(proto::LeaseKind::Vm) => LeaseKind::Vm,
        Ok(proto::LeaseKind::Unspecified) | Err(_) => {
            return Err(DecodeError::UnknownValue {
                field: "Request.kind",
                value: r.kind,
            });
        }
    };
    let needs = kbf_caps::Request::parse(r.needs.iter().map(|p| (&*p.key, &*p.value)))?;
    Ok(Request {
        key: ActionKey {
            instance: key.instance,
            action: required_digest(key.action, "ActionKey.action")?,
        },
        qos: from_qos(r.qos.ok_or(DecodeError::Missing("Request.qos"))?)?,
        kind,
        resources: from_resources(r.resources, "Request.resources")?,
        needs,
        hermetic: r.hermetic,
        do_not_cache: r.do_not_cache,
    })
}

fn from_qos(q: proto::Qos) -> Result<Qos, DecodeError> {
    use proto::qos::Level;
    match q.level.ok_or(DecodeError::UnknownCommand("Qos"))? {
        Level::Builtin(b) => match proto::BuiltinQos::try_from(b) {
            Ok(proto::BuiltinQos::Interactive) => Ok(Qos::Interactive),
            Ok(proto::BuiltinQos::Ci) => Ok(Qos::Ci),
            Ok(proto::BuiltinQos::Batch) => Ok(Qos::Batch),
            Ok(proto::BuiltinQos::Unspecified) | Err(_) => Err(DecodeError::UnknownValue {
                field: "Qos.builtin",
                value: b,
            }),
        },
        Level::Custom(c) => {
            let urgency = u16::try_from(c.urgency).map_err(|_| DecodeError::Urgency(c.urgency))?;
            Ok(Qos::custom(c.name, urgency)?)
        }
    }
}

const fn from_lease_id(lease: proto::LeaseId) -> LeaseId {
    LeaseId::new(lease.term, lease.seq)
}

fn required_lease_id(
    lease: Option<proto::LeaseId>,
    field: &'static str,
) -> Result<LeaseId, DecodeError> {
    lease.map(from_lease_id).ok_or(DecodeError::Missing(field))
}

fn from_record(r: proto::ControlRecord) -> Result<ControlRecord, DecodeError> {
    use proto::control_record::Record;
    Ok(
        match r
            .record
            .ok_or(DecodeError::UnknownCommand("ControlRecord"))?
        {
            Record::Lease(g) => ControlRecord::Lease(LeaseGrant {
                lease: required_lease_id(g.lease, "LeaseGrant.lease")?,
                operation: OperationId(g.operation),
                worker: WorkerId::new(g.worker),
            }),
            Record::Result(r) => ControlRecord::Result(ResultRecord {
                lease: required_lease_id(r.lease, "ResultRecord.lease")?,
                operation: OperationId(r.operation),
                outcome: from_outcome(r.outcome, "ResultRecord.outcome")?,
            }),
            Record::Refusal(r) => ControlRecord::Refusal(RefusalRecord {
                operation: OperationId(r.operation),
                reason: r.reason,
            }),
        },
    )
}

fn from_outcome(o: Option<proto::Outcome>, field: &'static str) -> Result<Outcome, DecodeError> {
    use proto::outcome::Outcome as O;
    let o = o.ok_or(DecodeError::Missing(field))?;
    match o.outcome.ok_or(DecodeError::UnknownCommand("Outcome"))? {
        O::Completed(d) => Ok(Outcome::Completed {
            action_result: from_digest(d, "Outcome.completed")?,
        }),
        O::Failed(f) => Ok(Outcome::Failed(match proto::Failure::try_from(f) {
            Ok(proto::Failure::Timeout) => Failure::Timeout,
            Ok(proto::Failure::Infra) => Failure::Infra,
            Ok(proto::Failure::Invalid) => Failure::Invalid,
            Ok(proto::Failure::OutOfMemory) => Failure::OutOfMemory,
            Ok(proto::Failure::NodeMemoryPressure) => Failure::NodeMemoryPressure,
            Ok(proto::Failure::MissingInput) => Failure::MissingInput,
            Ok(proto::Failure::LostDaemonRestarted) => Failure::Lost(LostWhy::DaemonRestarted),
            Ok(proto::Failure::LostFenced) => Failure::Lost(LostWhy::Fenced),
            Ok(proto::Failure::Unspecified) | Err(_) => {
                return Err(DecodeError::UnknownValue {
                    field: "Outcome.failed",
                    value: f,
                });
            }
        })),
    }
}

pub(crate) fn report_from_proto(r: proto::Report) -> Result<Report, DecodeError> {
    Ok(Report {
        now: FarmTime::from_millis(r.farm_time_ms),
        operation: OperationId(r.operation),
        lease: required_lease_id(r.lease, "Report.lease")?,
        outcome: from_outcome(r.outcome, "Report.outcome")?,
        action_entry: r.action_entry.map(from_entry).transpose()?,
    })
}

fn from_entry(e: proto::ActionEntry) -> Result<ActionEntry, DecodeError> {
    Ok(ActionEntry {
        action: required_digest(e.action, "ActionEntry.action")?,
        record: from_action_record(e.record, "ActionEntry.record")?,
    })
}

pub(crate) fn takeover_from_proto(t: proto::Takeover) -> Result<Takeover, DecodeError> {
    Ok(Takeover {
        term: NonZeroU64::new(t.term).ok_or(DecodeError::Missing("Takeover.term"))?,
        now: FarmTime::from_millis(t.farm_time_ms),
    })
}

pub(crate) fn checkpoint_from_proto(c: proto::Checkpoint) -> Result<Checkpoint, DecodeError> {
    let len = c.digest.len();
    Ok(Checkpoint {
        index: c.index,
        digest: <[u8; 32]>::try_from(c.digest).map_err(|_| DecodeError::HashLength {
            field: "Checkpoint.digest",
            len,
        })?,
    })
}

pub(crate) fn machine_version_from_proto(
    v: proto::MachineVersion,
) -> Result<MachineVersion, DecodeError> {
    NonZeroU32::new(v.version)
        .map(MachineVersion)
        .ok_or(DecodeError::Missing("MachineVersion.version"))
}

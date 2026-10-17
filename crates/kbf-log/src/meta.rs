//! `kbf_meta::Command` to and from `kbf.log.v1.MetaCommand`.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use kbf_meta::{
    ActionRecord, Closure, Command, Epoch, Generation, LegacyWriter, Location, NodeWriter,
    ObjectId, Role, ServerId, StagedPut, StoreId, TierId, Touch, UnreachableReason, Writer,
};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction, FarmTime};
use proto::meta_command::Command as P;

use crate::{DecodeError, EncodeError, Format};

/// Whether `command` joined the log in [`Format::V2`](crate::Format::V2).
pub(crate) const fn since_v2(command: &Command) -> bool {
    matches!(
        command,
        Command::AddTier { .. }
            | Command::PutStaged { .. }
            | Command::SealStaged { .. }
            | Command::ObjectFlushed { .. }
            | Command::FlushConflict { .. }
    )
}

/// Whether `command` joined the log in [`Format::V3`](crate::Format::V3).
pub(crate) const fn since_v3(command: &Command) -> bool {
    matches!(command, Command::EvictObjects { .. })
}

/// Whether `command` joined the log in [`Format::V4`](crate::Format::V4): `SetWriter`,
/// the commands of a rebuild of the index, and any command that names an object of a
/// node writer or of another log's epoch.
pub(crate) fn since_v4(command: &Command) -> bool {
    let node = |o: &ObjectId| matches!(o.writer(), Writer::Node(_) | Writer::Legacy(_));
    match command {
        Command::SetWriter(_)
        | Command::AdoptSegment { .. }
        | Command::RebuildProgress { .. }
        | Command::RebuildDone => true,
        Command::PutBlob { location, .. } => node(&location.object),
        Command::PutBlobs(blobs) => blobs.iter().any(|(_, l)| node(&l.object)),
        Command::PutStaged { objects, .. } => objects.iter().any(|p| node(&p.object)),
        Command::ObjectUnreachable { object, .. }
        | Command::ObjectReachable { object, .. }
        | Command::SealStaged { object }
        | Command::ObjectFlushed { object }
        | Command::FlushConflict { object } => node(object),
        Command::EvictObjects { objects } => objects.iter().any(node),
        Command::Tick(_)
        | Command::AllocEpoch
        | Command::PutAction { .. }
        | Command::Touch(_)
        | Command::Collect
        | Command::AddTier { .. } => false,
    }
}

/// Whether `format` takes `command`: format 1 takes neither the format-2 nor the
/// format-3 commands, format 2 takes no format-3 command, and no format before 4 takes
/// a format-4 one (`SetWriter`, or a node writer's object in any command).
pub(crate) fn takes(format: Format, command: &Command) -> bool {
    if since_v4(command) {
        format.get() >= Format::V4.get()
    } else if since_v3(command) {
        format.get() >= Format::V3.get()
    } else if since_v2(command) {
        format.get() >= Format::V2.get()
    } else {
        true
    }
}

pub(crate) fn to_proto(command: &Command) -> Result<proto::MetaCommand, EncodeError> {
    let c = match command {
        Command::Tick(t) => P::Tick(proto::Tick {
            farm_time_ms: t.as_millis(),
        }),
        Command::AllocEpoch => P::AllocEpoch(proto::AllocEpoch {}),
        Command::PutBlob { digest, location } => P::PutBlob(put_blob(digest, location)?),
        Command::PutBlobs(blobs) => P::PutBlobs(proto::PutBlobs {
            blobs: blobs
                .iter()
                .map(|(d, l)| put_blob(d, l))
                .collect::<Result<_, _>>()?,
        }),
        Command::PutAction {
            role,
            action,
            record,
        } => P::PutAction(proto::PutAction {
            role: match role {
                Role::Daemon => proto::Role::Daemon,
                Role::Client => proto::Role::Client,
            }
            .into(),
            action: Some(digest(action)?),
            record: Some(action_record(record)?),
        }),
        Command::Touch(touch) => P::Touch(proto::Touch {
            blobs: digests(touch.blobs.iter())?,
            actions: digests(touch.actions.iter())?,
        }),
        Command::ObjectUnreachable { object, reason } => {
            P::ObjectUnreachable(proto::ObjectUnreachable {
                object: Some(object_id(*object)),
                reason: match reason {
                    UnreachableReason::Missing => proto::UnreachableReason::Missing,
                    UnreachableReason::Corrupt => proto::UnreachableReason::Corrupt,
                }
                .into(),
            })
        }
        Command::ObjectReachable { object, generation } => {
            P::ObjectReachable(proto::ObjectReachable {
                object: Some(object_id(*object)),
                generation: generation.get(),
            })
        }
        Command::Collect => P::Collect(proto::Collect {}),
        Command::AddTier { server, tier } => P::AddTier(proto::AddTier {
            server: server.get(),
            tier: tier.get(),
        }),
        Command::PutStaged { tier, objects } => P::PutStaged(proto::PutStaged {
            tier: tier.get(),
            objects: objects
                .iter()
                .map(staged_object)
                .collect::<Result<_, _>>()?,
        }),
        Command::SealStaged { object } => P::SealStaged(proto::SealStaged {
            object: Some(object_id(*object)),
        }),
        Command::ObjectFlushed { object } => P::ObjectFlushed(proto::ObjectFlushed {
            object: Some(object_id(*object)),
        }),
        Command::FlushConflict { object } => P::FlushConflict(proto::FlushConflict {
            object: Some(object_id(*object)),
        }),
        Command::EvictObjects { objects } => P::EvictObjects(proto::EvictObjects {
            objects: objects.iter().copied().map(object_id).collect(),
        }),
        Command::SetWriter(writer) => P::SetWriter(proto::SetWriter {
            member: writer.member().get(),
            boot: writer.boot().get(),
        }),
        Command::AdoptSegment { object, entries } => {
            let hashes = digests(entries.iter().map(|(d, _)| d))?;
            let blob =
                |(digest, (_, offset)): (proto::Digest, &(Digest, u64))| proto::AdoptedBlob {
                    digest: Some(digest),
                    offset: *offset,
                };
            P::AdoptSegment(proto::AdoptSegment {
                object: Some(object_id(*object)),
                entries: hashes.into_iter().zip(entries).map(blob).collect(),
            })
        }
        Command::RebuildProgress { after } => P::RebuildProgress(proto::RebuildProgress {
            after_key: after.clone(),
        }),
        Command::RebuildDone => P::RebuildDone(proto::RebuildDone {}),
    };
    Ok(proto::MetaCommand { command: Some(c) })
}

fn staged_object(put: &StagedPut) -> Result<proto::StagedObject, EncodeError> {
    let blobs: Result<_, _> = put.blobs.iter().map(staged_blob).collect();
    Ok(proto::StagedObject {
        object: Some(object_id(put.object)),
        end: put.end,
        seal: put.seal,
        blobs: blobs?,
    })
}

fn staged_blob((d, offset): &(Digest, u64)) -> Result<proto::StagedBlob, EncodeError> {
    Ok(proto::StagedBlob {
        digest: Some(digest(d)?),
        offset: *offset,
    })
}

/// An action-cache record: the result's digest and its closure, a set.
pub(crate) fn action_record(record: &ActionRecord) -> Result<proto::ActionRecord, EncodeError> {
    Ok(proto::ActionRecord {
        result: Some(digest(&record.result)?),
        closure: digests(record.closure.iter())?,
    })
}

pub(crate) fn digest(d: &Digest) -> Result<proto::Digest, EncodeError> {
    let function = match d.function {
        DigestFunction::Sha256 => proto::DigestFunction::Sha256,
        _ => return Err(EncodeError::DigestFunction),
    };
    Ok(proto::Digest {
        function: function.into(),
        hash: d.hash.to_vec(),
        size_bytes: d.size_bytes,
    })
}

fn digests<'a>(set: impl Iterator<Item = &'a Digest>) -> Result<Vec<proto::Digest>, EncodeError> {
    set.map(digest).collect()
}

const fn object_id(object: ObjectId) -> proto::ObjectId {
    let (epoch, member, boot, log) = match object.writer() {
        Writer::Epoch(e) => (e.get(), 0, 0, 0),
        Writer::Node(n) => (0, n.member().get(), n.boot().get(), 0),
        Writer::Legacy(l) => (l.epoch().get(), 0, 0, l.log().get()),
    };
    proto::ObjectId {
        epoch,
        seq: object.seq(),
        member,
        boot,
        log,
    }
}

fn put_blob(d: &Digest, location: &Location) -> Result<proto::PutBlob, EncodeError> {
    Ok(proto::PutBlob {
        digest: Some(digest(d)?),
        location: Some(proto::Location {
            store: u32::from(location.store.get()),
            object: Some(object_id(location.object)),
            offset: location.offset,
        }),
    })
}

pub(crate) fn from_proto(command: proto::MetaCommand) -> Result<Command, DecodeError> {
    Ok(
        match command
            .command
            .ok_or(DecodeError::UnknownCommand("MetaCommand"))?
        {
            P::Tick(t) => Command::Tick(FarmTime::from_millis(t.farm_time_ms)),
            P::AllocEpoch(proto::AllocEpoch {}) => Command::AllocEpoch,
            P::PutBlob(b) => {
                let (digest, location) = blob(b)?;
                Command::PutBlob { digest, location }
            }
            P::PutBlobs(b) => {
                Command::PutBlobs(b.blobs.into_iter().map(blob).collect::<Result<_, _>>()?)
            }
            P::PutAction(a) => {
                let role = match proto::Role::try_from(a.role) {
                    Ok(proto::Role::Daemon) => Role::Daemon,
                    Ok(proto::Role::Client) => Role::Client,
                    Ok(proto::Role::Unspecified) | Err(_) => {
                        return Err(DecodeError::UnknownValue {
                            field: "PutAction.role",
                            value: a.role,
                        });
                    }
                };
                Command::PutAction {
                    role,
                    action: required_digest(a.action, "PutAction.action")?,
                    record: from_action_record(a.record, "PutAction.record")?,
                }
            }
            P::Touch(t) => Command::Touch(Touch {
                blobs: set(t.blobs, "Touch.blobs")?.into_iter().collect(),
                actions: set(t.actions, "Touch.actions")?.into_iter().collect(),
            }),
            P::ObjectUnreachable(u) => {
                let reason = match proto::UnreachableReason::try_from(u.reason) {
                    Ok(proto::UnreachableReason::Missing) => UnreachableReason::Missing,
                    Ok(proto::UnreachableReason::Corrupt) => UnreachableReason::Corrupt,
                    Ok(proto::UnreachableReason::Unspecified) | Err(_) => {
                        return Err(DecodeError::UnknownValue {
                            field: "ObjectUnreachable.reason",
                            value: u.reason,
                        });
                    }
                };
                Command::ObjectUnreachable {
                    object: required_object(u.object, "ObjectUnreachable.object")?,
                    reason,
                }
            }
            P::ObjectReachable(r) => Command::ObjectReachable {
                object: required_object(r.object, "ObjectReachable.object")?,
                generation: Generation::new(r.generation),
            },
            P::Collect(proto::Collect {}) => Command::Collect,
            P::AddTier(a) => Command::AddTier {
                server: ServerId::new(a.server),
                tier: TierId::new(a.tier),
            },
            P::PutStaged(p) => Command::PutStaged {
                tier: TierId::new(p.tier),
                objects: p
                    .objects
                    .into_iter()
                    .map(from_staged_object)
                    .collect::<Result<_, _>>()?,
            },
            P::SealStaged(s) => Command::SealStaged {
                object: required_object(s.object, "SealStaged.object")?,
            },
            P::ObjectFlushed(f) => Command::ObjectFlushed {
                object: required_object(f.object, "ObjectFlushed.object")?,
            },
            P::FlushConflict(c) => Command::FlushConflict {
                object: required_object(c.object, "FlushConflict.object")?,
            },
            P::EvictObjects(e) => Command::EvictObjects {
                objects: object_set(e.objects, "EvictObjects.objects")?,
            },
            P::SetWriter(w) => Command::SetWriter(NodeWriter::new(
                NonZeroU64::new(w.member).ok_or(DecodeError::Missing("SetWriter.member"))?,
                NonZeroU64::new(w.boot).ok_or(DecodeError::Missing("SetWriter.boot"))?,
            )),
            P::AdoptSegment(a) => Command::AdoptSegment {
                object: required_object(a.object, "AdoptSegment.object")?,
                entries: a
                    .entries
                    .into_iter()
                    .map(|b| Ok((required_digest(b.digest, "AdoptedBlob.digest")?, b.offset)))
                    .collect::<Result<_, DecodeError>>()?,
            },
            P::RebuildProgress(p) => {
                if p.after_key.is_empty() {
                    return Err(DecodeError::Missing("RebuildProgress.after_key"));
                }
                Command::RebuildProgress { after: p.after_key }
            }
            P::RebuildDone(proto::RebuildDone {}) => Command::RebuildDone,
        },
    )
}

fn from_staged_object(o: proto::StagedObject) -> Result<StagedPut, DecodeError> {
    Ok(StagedPut {
        object: required_object(o.object, "StagedObject.object")?,
        end: o.end,
        seal: o.seal,
        blobs: o
            .blobs
            .into_iter()
            .map(|b| Ok((required_digest(b.digest, "StagedBlob.digest")?, b.offset)))
            .collect::<Result<_, DecodeError>>()?,
    })
}

fn blob(b: proto::PutBlob) -> Result<(Digest, Location), DecodeError> {
    let location = b.location.ok_or(DecodeError::Missing("PutBlob.location"))?;
    let store = u16::try_from(location.store).map_err(|_| DecodeError::Store(location.store))?;
    Ok((
        required_digest(b.digest, "PutBlob.digest")?,
        Location {
            store: StoreId::new(store),
            object: required_object(location.object, "Location.object")?,
            offset: location.offset,
        },
    ))
}

/// The action-cache record in field `field`, which must be present.
pub(crate) fn from_action_record(
    record: Option<proto::ActionRecord>,
    field: &'static str,
) -> Result<ActionRecord, DecodeError> {
    let record = record.ok_or(DecodeError::Missing(field))?;
    Ok(ActionRecord {
        result: required_digest(record.result, "ActionRecord.result")?,
        closure: set(record.closure, "ActionRecord.closure")?
            .into_iter()
            .collect::<Closure>(),
    })
}

pub(crate) fn required_digest(
    d: Option<proto::Digest>,
    field: &'static str,
) -> Result<Digest, DecodeError> {
    from_digest(d.ok_or(DecodeError::Missing(field))?, field)
}

pub(crate) fn from_digest(d: proto::Digest, field: &'static str) -> Result<Digest, DecodeError> {
    let function = match proto::DigestFunction::try_from(d.function) {
        Ok(proto::DigestFunction::Sha256) => DigestFunction::Sha256,
        Ok(proto::DigestFunction::Unspecified) | Err(_) => {
            return Err(DecodeError::UnknownValue {
                field,
                value: d.function,
            });
        }
    };
    let len = d.hash.len();
    let hash = <[u8; 32]>::try_from(d.hash).map_err(|_| DecodeError::HashLength { field, len })?;
    Ok(Digest::new(function, hash, d.size_bytes))
}

/// A set of digests, refused unless strictly increasing: one set, one encoding.
fn set(digests: Vec<proto::Digest>, field: &'static str) -> Result<Vec<Digest>, DecodeError> {
    let out = digests
        .into_iter()
        .map(|d| from_digest(d, field))
        .collect::<Result<Vec<_>, _>>()?;
    if out.windows(2).any(|w| w[0] >= w[1]) {
        return Err(DecodeError::SetOrder(field));
    }
    Ok(out)
}

/// A set of object ids, refused unless strictly increasing: one set, one encoding.
fn object_set(
    objects: Vec<proto::ObjectId>,
    field: &'static str,
) -> Result<BTreeSet<ObjectId>, DecodeError> {
    let ids: Vec<ObjectId> = objects
        .into_iter()
        .map(|o| from_object(o, field))
        .collect::<Result<_, _>>()?;
    if ids.windows(2).any(|w| w[0] >= w[1]) {
        return Err(DecodeError::SetOrder(field));
    }
    Ok(ids.into_iter().collect())
}

fn required_object(
    object: Option<proto::ObjectId>,
    field: &'static str,
) -> Result<ObjectId, DecodeError> {
    from_object(object.ok_or(DecodeError::Missing(field))?, field)
}

/// An object id: an epoch writer's when `member`, `boot` and `log` are all zero (any
/// epoch, as every format before 4 reads it), a node writer's when `epoch` and `log` are
/// zero and `member` and `boot` are not, another log's epoch writer's when `member` and
/// `boot` are zero and `epoch` and `log` are not; refused otherwise.
fn from_object(object: proto::ObjectId, field: &'static str) -> Result<ObjectId, DecodeError> {
    let proto::ObjectId {
        epoch,
        seq,
        member,
        boot,
        log,
    } = object;
    let writer = match (
        epoch,
        NonZeroU64::new(member),
        NonZeroU64::new(boot),
        NonZeroU64::new(log),
    ) {
        (_, None, None, None) => Writer::Epoch(Epoch::new(epoch)),
        (0, Some(member), Some(boot), None) => Writer::Node(NodeWriter::new(member, boot)),
        (epoch, None, None, Some(log)) if epoch != 0 => {
            Writer::Legacy(LegacyWriter::new(log, Epoch::new(epoch)))
        }
        _ => return Err(DecodeError::Writer(field)),
    };
    Ok(ObjectId::of(writer, seq))
}

//! `kbf_meta::Command` to and from `kbf.log.v1.MetaCommand`.

use kbf_meta::{
    ActionRecord, Closure, Command, Epoch, Generation, Location, ObjectId, Role, StoreId, Touch,
    UnreachableReason,
};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction, FarmTime};
use proto::meta_command::Command as P;

use crate::{DecodeError, EncodeError};

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
            record: Some(proto::ActionRecord {
                result: Some(digest(&record.result)?),
                closure: digests(record.closure.iter())?,
            }),
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
    };
    Ok(proto::MetaCommand { command: Some(c) })
}

fn digest(d: &Digest) -> Result<proto::Digest, EncodeError> {
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
    proto::ObjectId {
        epoch: object.epoch().get(),
        seq: object.seq(),
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
                let record = a.record.ok_or(DecodeError::Missing("PutAction.record"))?;
                Command::PutAction {
                    role,
                    action: required_digest(a.action, "PutAction.action")?,
                    record: ActionRecord {
                        result: required_digest(record.result, "ActionRecord.result")?,
                        closure: set(record.closure, "ActionRecord.closure")?
                            .into_iter()
                            .collect::<Closure>(),
                    },
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
                        UnreachableReason::Missing
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
        },
    )
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

fn required_digest(d: Option<proto::Digest>, field: &'static str) -> Result<Digest, DecodeError> {
    from_digest(d.ok_or(DecodeError::Missing(field))?, field)
}

fn from_digest(d: proto::Digest, field: &'static str) -> Result<Digest, DecodeError> {
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

fn required_object(
    object: Option<proto::ObjectId>,
    field: &'static str,
) -> Result<ObjectId, DecodeError> {
    let object = object.ok_or(DecodeError::Missing(field))?;
    Ok(ObjectId::new(Epoch::new(object.epoch), object.seq))
}

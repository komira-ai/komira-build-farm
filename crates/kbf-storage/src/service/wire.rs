//! Translating between the wire and kbf's own values: digests and ByteStream resource
//! names. (`kbf-front` has the same for its own services; the two are one change away
//! from being one, once the front goes through this crate.)

use kbf_meta::ActionWriteError;
use kbf_proto::reapi;
use kbf_proto::storage::{Refusal, refusal};
use kbf_types::{Digest, DigestFunction};
use tonic::Status;

/// A wire digest as kbf's, or INVALID_ARGUMENT. A digest that does not parse is refused,
/// never skipped: a call that dropped it would answer for fewer digests than it was
/// asked about.
pub(super) fn digest(d: Option<&reapi::Digest>) -> Result<Digest, Status> {
    let d = d.ok_or_else(|| Status::invalid_argument("a digest is required"))?;
    let size = u64::try_from(d.size_bytes).map_err(|_| {
        Status::invalid_argument(format!("digest size {} is negative", d.size_bytes))
    })?;
    Digest::parse(DigestFunction::Sha256, &format!("{}/{size}", d.hash))
        .map_err(|e| Status::invalid_argument(format!("digest {}/{size}: {e}", d.hash)))
}

/// kbf's digest as a wire digest.
pub(super) fn to_proto(d: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: d.hash_hex(),
        // A parsed digest's size came from an `i64`.
        size_bytes: i64::try_from(d.size_bytes).unwrap_or(i64::MAX),
    }
}

/// SHA-256 or unset (REAPI lets a client leave it unset for SHA-256).
pub(super) fn check_digest_function(f: i32) -> Result<(), Status> {
    if f == reapi::digest_function::Value::Unknown as i32
        || f == reapi::digest_function::Value::Sha256 as i32
    {
        return Ok(());
    }
    Err(Status::invalid_argument(format!(
        "digest function {f} is not supported; storage uses SHA-256"
    )))
}

/// The blob a ByteStream `Read` names: `{instance}/blobs/{sha256/}{hash}/{size}`. The
/// instance name is not read: a storage node holds one cache. (`compressed-blobs/` is
/// not served: no compressor is advertised.)
pub(super) fn read_resource(name: &str) -> Result<Digest, Status> {
    let parts: Vec<&str> = name.split('/').collect();
    anchored(
        name,
        &parts,
        "blobs",
        "no `blobs/` segment",
        |at| match blob_digest(name, &parts[at + 1..])? {
            (digest, []) => Ok(digest),
            _ => Err(bad(name, "unexpected segments after the size")),
        },
    )
}

/// The blob a ByteStream `Write` names:
/// `{instance}/uploads/{uuid}/blobs/{sha256/}{hash}/{size}{/metadata}`.
pub(super) fn write_resource(name: &str) -> Result<Digest, Status> {
    let parts: Vec<&str> = name.split('/').collect();
    let expected = "expected `uploads/{uuid}/blobs/{hash}/{size}`";
    anchored(name, &parts, "uploads", expected, |at| {
        match parts.get(at + 2..) {
            // Anything after the size is client metadata, which REAPI lets the server
            // ignore.
            Some(["blobs", rest @ ..]) => blob_digest(name, rest).map(|(digest, _)| digest),
            _ => Err(bad(name, expected)),
        }
    })
}

/// Reads a resource name at the first `anchor` segment where `parse` accepts what follows.
///
/// The segment is not simply the first one, because an instance name is any path:
/// `a/uploads/b/uploads/u/blobs/{hash}/7` has an instance, `a/uploads/b`, that contains
/// the anchor. A position is taken only where the whole structure after it parses, so
/// it is the first that is a real `uploads/{uuid}/blobs/...` (or `blobs/{digest}`).
/// When none is, the error is the first position's, which names what was expected.
fn anchored<T>(
    name: &str,
    parts: &[&str],
    anchor: &str,
    missing: &str,
    parse: impl Fn(usize) -> Result<T, Status>,
) -> Result<T, Status> {
    let mut first_refusal = None;
    for at in (0..parts.len()).filter(|at| parts[*at] == anchor) {
        match parse(at) {
            Ok(found) => return Ok(found),
            Err(refusal) => {
                first_refusal.get_or_insert(refusal);
            }
        }
    }
    match first_refusal {
        Some(refusal) => Err(refusal),
        None => Err(bad(name, missing)),
    }
}

/// Parses `{sha256/}{hash}/{size}`, returning the digest and the segments left over.
fn blob_digest<'a>(name: &str, parts: &'a [&'a str]) -> Result<(Digest, &'a [&'a str]), Status> {
    let parts = parts.strip_prefix(&["sha256"]).unwrap_or(parts);
    let [hash, size, rest @ ..] = parts else {
        return Err(bad(name, "expected `{hash}/{size}`"));
    };
    let digest = Digest::parse(DigestFunction::Sha256, &format!("{hash}/{size}"))
        .map_err(|e| bad(name, &e.to_string()))?;
    Ok((digest, rest))
}

fn bad(name: &str, why: &str) -> Status {
    Status::invalid_argument(format!("resource name {name:?}: {why}"))
}

/// An action-cache write refusal as the wire spells it.
pub(super) fn refusal_of(e: &ActionWriteError) -> Refusal {
    let (kind, lacking): (_, &[Digest]) = match e {
        ActionWriteError::NotDaemon => (refusal::Kind::NotDaemon, &[]),
        ActionWriteError::Absent(ds) => (refusal::Kind::Absent, ds),
        ActionWriteError::Unreachable(ds) => (refusal::Kind::Unreachable, ds),
    };
    let (blob, more) = match lacking {
        [first, more @ ..] => (Some(to_proto(first)), more.iter().map(to_proto).collect()),
        [] => (None, Vec::new()),
    };
    Refusal {
        kind: kind as i32,
        blob,
        more,
    }
}

/// The answer of a `Write` that has stored a blob of `size` bytes.
pub(super) fn committed(
    size: u64,
) -> tonic::Response<kbf_proto::google::bytestream::WriteResponse> {
    tonic::Response::new(kbf_proto::google::bytestream::WriteResponse {
        committed_size: i64::try_from(size).unwrap_or(i64::MAX),
    })
}

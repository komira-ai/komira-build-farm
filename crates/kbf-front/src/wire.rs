//! Translating between REAPI wire values and kbf's own: digests, digest functions,
//! ByteStream resource names and per-item statuses.

use kbf_proto::google::rpc;
use kbf_proto::reapi;
use kbf_types::{Digest, DigestFunction};
use tonic::{Code, Status};

/// The lowercase name REAPI uses for SHA-256 inside resource names.
const SHA256_NAME: &str = "sha256";

/// A REAPI digest as kbf's [`Digest`], or INVALID_ARGUMENT.
///
/// A digest that does not parse is refused, never skipped: a call that dropped it would
/// answer for fewer digests than it was asked about.
pub(crate) fn digest(d: Option<&reapi::Digest>) -> Result<Digest, Status> {
    let d = d.ok_or_else(|| Status::invalid_argument("a digest is required"))?;
    let size = u64::try_from(d.size_bytes).map_err(|_| {
        Status::invalid_argument(format!("digest size {} is negative", d.size_bytes))
    })?;
    Digest::parse(DigestFunction::Sha256, &format!("{}/{size}", d.hash))
        .map_err(|e| Status::invalid_argument(format!("digest {}/{size}: {e}", d.hash)))
}

/// Every digest of a request, or INVALID_ARGUMENT naming the first bad one.
pub(crate) fn digests(ds: &[reapi::Digest]) -> Result<Vec<Digest>, Status> {
    ds.iter().map(|d| digest(Some(d))).collect()
}

/// kbf's [`Digest`] as a REAPI digest.
pub(crate) fn digest_to_proto(d: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: d.hash_hex(),
        // A blob's size is far below `i64::MAX`; a digest that claims more could not
        // have been parsed from a request.
        size_bytes: i64::try_from(d.size_bytes).unwrap_or(i64::MAX),
    }
}

/// Accepts a request's `digest_function` if it is SHA-256 or unset (REAPI lets a client
/// leave it unset for SHA-256, inferring it from the hash length).
pub(crate) fn check_digest_function(f: i32) -> Result<(), Status> {
    let unknown = reapi::digest_function::Value::Unknown as i32;
    let sha256 = reapi::digest_function::Value::Sha256 as i32;
    if f == unknown || f == sha256 {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "digest function {f} is not supported; this cache uses SHA-256"
        )))
    }
}

/// A per-item status for batch responses, from a call-level outcome.
pub(crate) fn rpc_status(status: &Status) -> rpc::Status {
    rpc::Status {
        code: status.code() as i32,
        message: status.message().to_owned(),
        details: Vec::new(),
    }
}

/// The OK per-item status.
pub(crate) fn rpc_ok() -> rpc::Status {
    rpc::Status {
        code: Code::Ok as i32,
        message: String::new(),
        details: Vec::new(),
    }
}

/// A blob a ByteStream resource name names, and the instance name before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resource {
    /// Every segment before `blobs/` (a read) or `uploads/` (a write), joined by `/`;
    /// empty when there is none. Authorization reads it; the cache does not, since one
    /// cell has one cache.
    pub instance: String,
    /// The blob.
    pub digest: Digest,
}

/// The blob a ByteStream `Read` names: `{instance}/blobs/{sha256/}{hash}/{size}`.
///
/// Compressed reads (`compressed-blobs/...`) are refused because no compressor is
/// advertised.
pub(crate) fn read_resource(name: &str) -> Result<Resource, Status> {
    let parts: Vec<&str> = name.split('/').collect();
    let at = parts
        .iter()
        .position(|p| *p == "blobs" || *p == "compressed-blobs")
        .ok_or_else(|| bad_resource(name, "no `blobs/` segment"))?;
    if parts[at] == "compressed-blobs" {
        return Err(Status::invalid_argument(format!(
            "{name:?}: compressed reads are not supported; no compressor is advertised"
        )));
    }
    let (digest, rest) = blob_digest(name, &parts[at + 1..])?;
    if !rest.is_empty() {
        return Err(bad_resource(name, "unexpected segments after the size"));
    }
    Ok(Resource {
        instance: parts[..at].join("/"),
        digest,
    })
}

/// The blob a ByteStream `Write` or `QueryWriteStatus` names:
/// `{instance}/uploads/{uuid}/blobs/{sha256/}{hash}/{size}{/metadata}`.
pub(crate) fn write_resource(name: &str) -> Result<Resource, Status> {
    let parts: Vec<&str> = name.split('/').collect();
    let at = parts
        .iter()
        .position(|p| *p == "uploads")
        .ok_or_else(|| bad_resource(name, "no `uploads/` segment"))?;
    match parts.get(at + 2) {
        Some(&"blobs") => {}
        Some(&"compressed-blobs") => {
            return Err(Status::invalid_argument(format!(
                "{name:?}: compressed uploads are not supported; no compressor is advertised"
            )));
        }
        _ => return Err(bad_resource(name, "expected `uploads/{uuid}/blobs/`")),
    }
    // Anything after the size is client metadata, which REAPI lets the server ignore.
    let (digest, _metadata) = blob_digest(name, &parts[at + 3..])?;
    Ok(Resource {
        instance: parts[..at].join("/"),
        digest,
    })
}

/// Parses `{sha256/}{hash}/{size}` from the segments after `blobs`, returning the
/// digest and the segments left over.
fn blob_digest<'a>(name: &str, parts: &'a [&'a str]) -> Result<(Digest, &'a [&'a str]), Status> {
    let parts = match parts.first() {
        Some(&SHA256_NAME) => &parts[1..],
        _ => parts,
    };
    let [hash, size, rest @ ..] = parts else {
        return Err(bad_resource(name, "expected `{hash}/{size}`"));
    };
    let digest = Digest::parse(DigestFunction::Sha256, &format!("{hash}/{size}"))
        .map_err(|e| bad_resource(name, &e.to_string()))?;
    Ok((digest, rest))
}

fn bad_resource(name: &str, why: &str) -> Status {
    Status::invalid_argument(format!("resource name {name:?}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Catches: a parser that misreads the instance name (which authorization checks),
    /// the optional digest function segment or the trailing upload metadata, which
    /// would make a well-formed client read or write the wrong blob or fail, or be
    /// authorized against the wrong instance.
    #[test]
    fn resource_names_parse_every_spelling() {
        let want = Digest::parse(DigestFunction::Sha256, &format!("{HASH}/7")).unwrap();
        let at = |instance: &str| Resource {
            instance: instance.to_owned(),
            digest: want,
        };
        for (name, instance) in [
            (format!("blobs/{HASH}/7"), ""),
            (format!("main/ci/blobs/{HASH}/7"), "main/ci"),
            (format!("blobs/sha256/{HASH}/7"), ""),
        ] {
            assert_eq!(read_resource(&name).unwrap(), at(instance), "{name}");
        }
        for (name, instance) in [
            (format!("uploads/u-1/blobs/{HASH}/7"), ""),
            (
                format!("inst/a/uploads/u-1/blobs/sha256/{HASH}/7/some/metadata"),
                "inst/a",
            ),
        ] {
            assert_eq!(write_resource(&name).unwrap(), at(instance), "{name}");
        }
        for bad in [
            format!("blobs/{HASH}"),
            format!("blobs/{HASH}/-7"),
            format!("compressed-blobs/zstd/{HASH}/7"),
            "nothing".to_owned(),
        ] {
            assert!(read_resource(&bad).is_err(), "{bad}");
            let upload = format!("uploads/u-1/{bad}");
            assert!(write_resource(&upload).is_err(), "{upload}");
        }
        assert!(write_resource(&format!("uploads/blobs/{HASH}/7")).is_err());
        // Segments after the size are upload metadata on a write, but an error on a read.
        assert!(read_resource(&format!("blobs/{HASH}/7/extra")).is_err());
    }

    /// Catches: a negative size wrapping to a huge `u64`, or uppercase hex accepted as
    /// a second spelling of the same digest.
    #[test]
    fn proto_digests_are_checked() {
        let ok = reapi::Digest {
            hash: HASH.to_owned(),
            size_bytes: 3,
        };
        assert_eq!(digest_to_proto(&digest(Some(&ok)).unwrap()), ok);
        let negative = reapi::Digest {
            size_bytes: -1,
            ..ok.clone()
        };
        let upper = reapi::Digest {
            hash: HASH.to_uppercase(),
            ..ok
        };
        for bad in [negative, upper] {
            assert_eq!(
                digest(Some(&bad)).unwrap_err().code(),
                Code::InvalidArgument
            );
        }
        assert_eq!(digest(None).unwrap_err().code(), Code::InvalidArgument);
    }
}

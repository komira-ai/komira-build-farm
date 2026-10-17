//! The blob and the instance a ByteStream resource name names, as the raw path needs
//! them to route and to authorize.
//!
//! Unlike [`crate::wire`], which serves reads from this process and refuses
//! `compressed-blobs/`, this accepts both spellings: the name is still plain text, and
//! whether a node serves compressed blobs is the node's answer. The digest of a
//! compressed name is the uncompressed blob's, which is what routes it.

use kbf_types::Digest;
use tonic::Status;

use super::Method;
use crate::wire;

/// What a resource name says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Resource {
    /// Every segment before `blobs/`, `compressed-blobs/` or `uploads/`; what the
    /// authorizers read.
    pub instance: String,
    pub digest: Digest,
}

/// `{instance}/{blobs|compressed-blobs/{compressor}}/{sha256/}{hash}/{size}` for a read,
/// `{instance}/uploads/{uuid}/{...same...}{/metadata}` for the other calls.
pub(super) fn parse(method: Method, name: &str) -> Result<Resource, Status> {
    let parts: Vec<&str> = name.split('/').collect();
    let upload = method != Method::Read;
    let (at, blobs) = if upload {
        let at = parts
            .iter()
            .position(|p| *p == "uploads")
            .ok_or_else(|| wire::bad_resource(name, "no `uploads/` segment"))?;
        (at, parts.get(at + 2..).unwrap_or_default())
    } else {
        let at = parts
            .iter()
            .position(|p| matches!(*p, "blobs" | "compressed-blobs"))
            .ok_or_else(|| wire::bad_resource(name, "no `blobs/` segment"))?;
        (at, &parts[at..])
    };
    let instance = parts[..at].join("/");
    let rest = match blobs {
        ["blobs", rest @ ..] => rest,
        ["compressed-blobs", _compressor, rest @ ..] => rest,
        _ => {
            return Err(wire::bad_resource(
                name,
                "expected `uploads/{uuid}/blobs/` or `compressed-blobs/{compressor}/`",
            ));
        }
    };
    let (digest, trailing) = wire::blob_digest(name, rest)?;
    // Anything after the size is upload metadata, which REAPI lets the server ignore.
    if !upload && !trailing.is_empty() {
        return Err(wire::bad_resource(
            name,
            "unexpected segments after the size",
        ));
    }
    Ok(Resource { instance, digest })
}

#[cfg(test)]
mod tests {
    use kbf_types::DigestFunction;

    use super::*;

    const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn want(instance: &str) -> Resource {
        Resource {
            instance: instance.to_owned(),
            digest: Digest::parse(DigestFunction::Sha256, &format!("{HASH}/7")).unwrap(),
        }
    }

    /// Catches: an instance name misread (authorization would check the wrong one), a
    /// compressed name refused or its compressor segment read as the hash, and the
    /// upload metadata or the optional `sha256/` read as part of the digest.
    #[test]
    fn every_spelling_parses() {
        for (method, name, instance) in [
            (Method::Read, format!("blobs/{HASH}/7"), ""),
            (Method::Read, format!("a/b/blobs/sha256/{HASH}/7"), "a/b"),
            (
                Method::Read,
                format!("a/compressed-blobs/zstd/{HASH}/7"),
                "a",
            ),
            (
                Method::Write,
                format!("uploads/u/blobs/{HASH}/7/meta/x"),
                "",
            ),
            (
                Method::Write,
                format!("i/uploads/u/compressed-blobs/zstd/{HASH}/7"),
                "i",
            ),
            (
                Method::QueryWriteStatus,
                format!("i/uploads/u/blobs/{HASH}/7"),
                "i",
            ),
        ] {
            assert_eq!(parse(method, &name).unwrap(), want(instance), "{name}");
        }
    }

    /// Catches: a malformed name routed anyway (it would go to some node's wrong
    /// shard), and a read with trailing segments accepted.
    #[test]
    fn malformed_names_are_invalid_argument() {
        for (method, name) in [
            (Method::Read, "nothing".to_owned()),
            (Method::Read, format!("blobs/{HASH}")),
            (Method::Read, format!("blobs/{HASH}/7/extra")),
            (Method::Read, "compressed-blobs".to_owned()),
            (Method::Write, format!("blobs/{HASH}/7")),
            (Method::Write, format!("uploads/u/other/{HASH}/7")),
            (Method::Write, "uploads".to_owned()),
        ] {
            let e = parse(method, &name).unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}");
        }
    }
}

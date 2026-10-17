//! The gRPC code each [`StorageError`] answers with.

use std::sync::Arc;

use kbf_meta::ActionWriteError;
use kbf_types::{Digest, DigestFunction};
use tonic::Code;

use super::StorageError;

fn digest(byte: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [byte; 32], 1)
}

/// An error the store's backend gave.
#[derive(Debug, thiserror::Error)]
#[error("the disk is gone")]
struct Backend;

/// Catches a code changed for any error: a store that could not answer reported as
/// NOT_FOUND (which a caller takes as "absent, upload it") or a client's mistake as
/// UNAVAILABLE (which it retries forever), and a refused action-cache write answered as
/// anything but the code that says why, and a read offset past the end answered as a
/// malformed request (INVALID_ARGUMENT) where ByteStream says OUT_OF_RANGE.
#[test]
fn each_error_answers_its_code_and_message() {
    let (a, b) = (digest(1), digest(2));
    let cases = [
        (StorageError::NotFound(a), Code::NotFound),
        (StorageError::Unreachable(a), Code::Unavailable),
        (
            StorageError::DigestMismatch {
                claimed: a,
                actual: b,
            },
            Code::InvalidArgument,
        ),
        (
            StorageError::Corrupt {
                digest: a,
                actual: b,
            },
            Code::DataLoss,
        ),
        (
            StorageError::OutOfRange {
                digest: a,
                offset: 9,
            },
            Code::OutOfRange,
        ),
        (StorageError::Invalid("bad".into()), Code::InvalidArgument),
        (
            StorageError::StaleMap { current: 7 },
            Code::FailedPrecondition,
        ),
        (ActionWriteError::NotDaemon.into(), Code::PermissionDenied),
        (
            ActionWriteError::Absent(vec![a]).into(),
            Code::FailedPrecondition,
        ),
        (
            ActionWriteError::Unreachable(vec![a]).into(),
            Code::Unavailable,
        ),
        (
            StorageError::Unavailable(Arc::new(Backend)),
            Code::Unavailable,
        ),
        (StorageError::Internal("torn".into()), Code::Internal),
        (
            StorageError::StaleMap { current: 7 },
            Code::FailedPrecondition,
        ),
    ];
    for (error, code) in cases {
        let message = error.to_string();
        let status = tonic::Status::from(error);
        assert_eq!(status.code(), code, "{message}");
        assert_eq!(status.message(), message);
    }
}

/// Catches an unavailable store's error replaced by a message of its own: the
/// backend's own words are what an operator reads.
#[test]
fn unavailable_says_what_the_backend_said() {
    let error = StorageError::Unavailable(Arc::new(Backend));
    assert_eq!(error.to_string(), "the disk is gone");
}

/// Catches a STALE_MAP answer that does not say which generation the node holds (the
/// caller could not know how new a map to fetch), or that names it in a spelling other
/// than the metadata entry every storage call already carries the caller's generation in.
#[test]
fn a_stale_map_status_carries_the_nodes_generation() {
    let status = tonic::Status::from(StorageError::StaleMap { current: 7 });
    let header = status
        .metadata()
        .get(crate::service::MAP_GENERATION_HEADER)
        .expect("the generation entry");
    assert_eq!(header.to_str().expect("ASCII"), "7");
    assert!(status.message().starts_with("STALE_MAP"), "{status:?}");
    let refused = tonic::Status::from(StorageError::from(ActionWriteError::Absent(vec![digest(
        1,
    )])));
    assert!(
        refused
            .metadata()
            .get(crate::service::MAP_GENERATION_HEADER)
            .is_none()
    );
}

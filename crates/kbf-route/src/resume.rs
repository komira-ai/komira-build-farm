//! A read that goes on where it stopped.

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;

use crate::router::Router;

/// How many times one read goes on after a failure.
pub const MAX_RESUMES: u32 = 3;

/// Where a read has got to.
struct Progress {
    router: Router,
    digest: Digest,
    /// The offset the next byte is at.
    next: u64,
    /// The bytes still to come of a read with a limit; `None` for a read to the end.
    remaining: Option<u64>,
    /// The stream being passed on, `None` once the read has ended.
    stream: Option<ReadStream>,
    resumes: u32,
}

/// `first`, the stream of the read of `digest` from `offset` (for `limit` bytes, or to the
/// end if 0), passed on chunk by chunk. When it fails, the read is made again from the
/// offset reached, through the router, so another owner may answer it; the failure
/// reaches the caller only when the read has been resumed [`MAX_RESUMES`] times, or
/// cannot be.
pub(crate) fn resumed(
    router: Router,
    digest: Digest,
    offset: u64,
    limit: u64,
    first: ReadStream,
) -> ReadStream {
    let progress = Progress {
        router,
        digest,
        next: offset,
        remaining: (limit > 0).then_some(limit),
        stream: Some(first),
        resumes: 0,
    };
    stream::unfold(progress, step).boxed()
}

async fn step(mut p: Progress) -> Option<(Result<Bytes, StorageError>, Progress)> {
    loop {
        let item = p.stream.as_mut()?.next().await;
        match item {
            Some(Ok(chunk)) => {
                let len = chunk.len() as u64;
                p.next += len;
                p.remaining = p.remaining.map(|left| left.saturating_sub(len));
                if p.remaining == Some(0) {
                    p.stream = None;
                }
                return Some((Ok(chunk), p));
            }
            None => return None,
            Some(Err(failure)) => {
                p.stream = None;
                if p.resumes == MAX_RESUMES {
                    return Some((Err(failure), p));
                }
                p.resumes += 1;
                let limit = p.remaining.unwrap_or(0);
                match p.router.open(p.digest, p.next, limit).await {
                    Ok(stream) => p.stream = Some(stream),
                    Err(e) => return Some((Err(e), p)),
                }
            }
        }
    }
}

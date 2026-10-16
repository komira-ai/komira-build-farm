//! Multipart upload: how [`S3Store`](super::S3Store) writes an object larger than one
//! request should carry, a part at a time.
//!
//! CreateMultipartUpload, then UploadPart for each part in order (each held in memory
//! only while it is sent), then CompleteMultipartUpload naming every part's ETag. Any
//! failure before the object is complete (the body's, a part's, the completion's,
//! including a conditional write refused at completion) aborts the upload with
//! AbortMultipartUpload before the error is returned, so no incomplete upload is left
//! holding storage. A write dropped mid-upload (its caller went away) aborts it from a
//! task of its own. An abort that itself fails is logged; a bucket lifecycle rule
//! (`AbortIncompleteMultipartUpload`) is the backstop for those and for a crash.
//!
//! S3's rules, which [`part_size`] keeps: every part but the last is at least
//! [`MIN_PART_BYTES`], none is over [`MAX_PART_BYTES`], and an object has at most
//! [`MAX_PARTS`] parts.

use std::sync::Arc;

use bytes::Bytes;
use reqwest::{Method, StatusCode};

use super::{Conn, failure, transport, xml};
use crate::body::Parts;
use crate::{BodyStream, ObjectKey, ObjectStoreError};

/// The smallest part S3 takes, except the last: 5 MiB.
pub const MIN_PART_BYTES: u64 = 5 << 20;

/// The largest part S3 takes: 5 GiB, the limit of one PUT.
pub const MAX_PART_BYTES: u64 = 5 << 30;

/// The most parts one object may have.
pub const MAX_PARTS: u64 = 10_000;

/// The part size of any object that fits in [`MAX_PARTS`] of them (156.25 GiB):
/// 16 MiB, what one part holds in memory while it is sent.
pub const PART_BYTES: u64 = 16 << 20;

/// The largest object written with one PUT; a larger one is a multipart upload. One
/// part's worth, so a streamed write of any size holds at most [`PART_BYTES`] (or
/// [`part_size`] of a larger object) of its body at once. Below S3's 5 GiB limit for
/// one PUT.
pub const MULTIPART_THRESHOLD: u64 = PART_BYTES;

const MIB: u64 = 1 << 20;

// One PUT carries at most what S3 takes in one.
const _: () = assert!(MULTIPART_THRESHOLD <= MAX_PART_BYTES);

/// The part size of an object of `len` bytes: [`PART_BYTES`], or for an object too
/// large for [`MAX_PARTS`] parts of that size, the least whole number of MiB that fits
/// it in [`MAX_PARTS`] parts. `None` when even [`MAX_PART_BYTES`] parts do not (an
/// object over 10,000 parts of 5 GiB, about 48.8 TiB).
#[must_use]
pub fn part_size(len: u64) -> Option<u64> {
    let least = len.div_ceil(MAX_PARTS).div_ceil(MIB).checked_mul(MIB)?;
    let size = least.max(PART_BYTES);
    (size <= MAX_PART_BYTES).then_some(size)
}

/// Writes `body`, `len` bytes, under `key` as a multipart upload.
pub(super) async fn put(
    conn: &Arc<Conn>,
    key: &ObjectKey,
    len: u64,
    body: BodyStream,
) -> Result<(), ObjectStoreError> {
    let size = part_size(len).ok_or(ObjectStoreError::Unsupported(
        "an object larger than 10,000 parts of 5 GiB",
    ))?;
    let upload = create(conn, key).await?;
    match send(&upload, len, size, body).await {
        Ok(()) => {
            upload.finished();
            Ok(())
        }
        Err(e) => {
            upload.abort().await;
            Err(e)
        }
    }
}

/// An upload in progress. Dropped while open, it is aborted from a task of its own.
struct Upload {
    conn: Arc<Conn>,
    key: ObjectKey,
    id: String,
    open: bool,
}

impl Upload {
    /// The upload completed: nothing to abort.
    fn finished(mut self) {
        self.open = false;
    }

    /// Aborts the upload; a failure is logged (the lifecycle rule is the backstop).
    async fn abort(mut self) {
        self.open = false;
        abort(&self.conn, &self.key, &self.id).await;
    }
}

impl Drop for Upload {
    /// Aborts an upload still open from a task of its own; outside a runtime it is left
    /// for the bucket's lifecycle rule.
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let (conn, key, id) = (
            Arc::clone(&self.conn),
            self.key.clone(),
            std::mem::take(&mut self.id),
        );
        let runtime = tokio::runtime::Handle::try_current().ok();
        runtime.map(|r| r.spawn(async move { abort(&conn, &key, &id).await }));
    }
}

/// CreateMultipartUpload.
async fn create(conn: &Arc<Conn>, key: &ObjectKey) -> Result<Upload, ObjectStoreError> {
    let query = [("uploads", String::new())];
    let resp = conn
        .send(Method::POST, Some(key), &query, Vec::new(), Bytes::new())
        .await?;
    if resp.status() != StatusCode::OK {
        return Err(failure(resp).await);
    }
    let body = resp.bytes().await.map_err(transport)?;
    Ok(Upload {
        conn: Arc::clone(conn),
        key: key.clone(),
        id: xml::upload_id(&body)?,
        open: true,
    })
}

/// Every part, in order, then the completion.
async fn send(
    upload: &Upload,
    len: u64,
    size: u64,
    body: BodyStream,
) -> Result<(), ObjectStoreError> {
    let mut parts = Parts::new(body, len);
    let mut etags = Vec::new();
    // `Parts` answers `None` only after the body has ended with exactly `len` bytes,
    // so an upload is never completed over a body that failed at its end.
    while let Some(part) = parts.next(size).await? {
        let number = etags.len() + 1;
        etags.push(upload_part(upload, number, part).await?);
    }
    complete(upload, &etags).await
}

/// UploadPart: answers the part's ETag.
async fn upload_part(
    upload: &Upload,
    number: usize,
    part: Bytes,
) -> Result<String, ObjectStoreError> {
    let query = [
        ("partNumber", number.to_string()),
        ("uploadId", upload.id.clone()),
    ];
    let resp = upload
        .conn
        .send(Method::PUT, Some(&upload.key), &query, Vec::new(), part)
        .await?;
    if resp.status() != StatusCode::OK {
        return Err(failure(resp).await);
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    etag.ok_or_else(|| {
        ObjectStoreError::Protocol(format!(
            "part {number} of {} answered without an ETag",
            upload.key
        ))
    })
}

/// CompleteMultipartUpload, conditional as `put_new` is when the store claims it. S3
/// may answer 200 with an `<Error>` body: that is a failure too.
async fn complete(upload: &Upload, etags: &[String]) -> Result<(), ObjectStoreError> {
    let conn = &upload.conn;
    let mut headers = vec![("content-type", "application/xml".to_owned())];
    if conn.capabilities.conditional_put {
        headers.push(("if-none-match", "*".to_owned()));
    }
    let query = [("uploadId", upload.id.clone())];
    let body = Bytes::from(xml::complete_request(etags));
    let resp = conn
        .send(Method::POST, Some(&upload.key), &query, headers, body)
        .await?;
    match resp.status() {
        StatusCode::OK => {
            let body = resp.bytes().await.map_err(transport)?;
            xml::complete_answer(&body)
        }
        // Asked only of a store that claims conditional writes.
        StatusCode::PRECONDITION_FAILED => Err(ObjectStoreError::AlreadyExists(upload.key.clone())),
        _ => Err(failure(resp).await),
    }
}

/// AbortMultipartUpload; a failure is logged. An upload the store no longer knows
/// (completed after all, or aborted already) needs nothing more.
async fn abort(conn: &Conn, key: &ObjectKey, id: &str) {
    if let Err(e) = try_abort(conn, key, id).await {
        tracing::warn!(%key, error = %e, "a multipart upload could not be aborted; the bucket's lifecycle rule must remove it");
    }
}

async fn try_abort(conn: &Conn, key: &ObjectKey, id: &str) -> Result<(), ObjectStoreError> {
    let query = [("uploadId", id.to_owned())];
    let resp = conn
        .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
        .await?;
    match resp.status() {
        StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
        _ => Err(failure(resp).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Catches: a part size that breaks S3's rules at the edges: over 10,000 parts (an
    /// object one byte past what 16 MiB parts hold), a part under 5 MiB that is not the
    /// last, a part over 5 GiB, or an object that does fit refused. Every size it gives
    /// is a whole number of MiB, so parts line up with nothing smaller.
    #[test]
    fn part_sizes_keep_s3s_rules_at_the_boundaries() {
        assert_eq!(part_size(1), Some(PART_BYTES));
        assert_eq!(part_size(MULTIPART_THRESHOLD + 1), Some(PART_BYTES));
        let most_at_16_mib = PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most_at_16_mib), Some(PART_BYTES));
        assert_eq!(part_size(most_at_16_mib + 1), Some(PART_BYTES + MIB));
        // Over 5 GiB, S3's single-PUT limit: still 16 MiB parts.
        assert_eq!(part_size(5 * GIB + 1), Some(PART_BYTES));
        let most = MAX_PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most), Some(MAX_PART_BYTES));
        assert_eq!(part_size(most + 1), None);
        assert_eq!(part_size(u64::MAX), None);
        for len in [
            MULTIPART_THRESHOLD + 1,
            5 * GIB + 1,
            most_at_16_mib + 1,
            1 << 40,
            most - 1,
            most,
        ] {
            let size = part_size(len).expect("fits");
            assert!(len.div_ceil(size) <= MAX_PARTS, "{len}: too many parts");
            assert!((MIN_PART_BYTES..=MAX_PART_BYTES).contains(&size), "{len}");
            assert_eq!(size % MIB, 0, "{len}");
        }
    }
}

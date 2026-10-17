//! `GET /v1/blobs/{digest}` on the operator API listener: a CAS blob's bytes over HTTP,
//! for the links of a build's page (`docs/design/invocations.md` section 4.6,
//! `docs/api.md`).
//!
//! - **Off unless the policy names it.** The route runs the REAPI policy
//!   (`--reapi-auth-policy`): its authenticator on the request, then its
//!   `blobs.getAuthorizer` ([`kbf_auth::Policy::blobs_get`]) on the `instance`
//!   parameter. Without that key, without `invocations.getAuthorizer` (the build
//!   pages, [`crate::http_invocations`], whose links it serves), or without a policy
//!   file, every request is `404`. `--invocations-open` fills in `allow` for both.
//! - **Verified bytes.** The blob is read with [`Cache::read_blob`]: the whole blob is
//!   checked against its digest before a byte is sent, and the read is a touch.
//!   (ByteStream Read does not hold the blob: it reads in pieces and checks as it goes,
//!   `Cache::read_range`.)
//! - **Never a page.** Every answer, errors included, carries
//!   `X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox`. The bytes
//!   are `application/octet-stream` with `Content-Disposition: attachment`; `?name=` is
//!   sent only as an RFC 8187 `filename*`, percent-encoded. `?inline=text` serves a blob
//!   of at most [`INLINE_MAX_BYTES`] as `text/plain; charset=utf-8`, `inline`.
//! - **A memory budget.** A read holds the whole blob in memory (verification needs
//!   all of it), and the bytes of every answer still being sent count against
//!   [`READ_BUDGET_BYTES`]. A read waits in line (first come, first served) up to
//!   [`READ_BUDGET_WAIT`] for its share, then is `503` with `Retry-After`, before a
//!   byte is read; a digest larger than the largest blob the cache accepts is `404`. The cache keeps one digest per blob, so it cannot check a part of
//!   one, and a second read to stream from would send bytes nobody checked.
//! - **One byte range.** `Range: bytes=...` with a single range is answered `206`, one
//!   that cannot be satisfied `416`; anything else (several ranges, another unit, a
//!   malformed value) is ignored and the whole blob is sent, as RFC 9110 allows.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    RANGE, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kbf_auth::{Authenticator, Authorizer, Policy, authorize};
use kbf_front::{Cache, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_types::{Digest, DigestFunction};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::{Code, Status};

/// The largest blob `?inline=text` serves: 4 MiB. A larger one is refused `400`, and
/// is still served as an attachment.
pub const INLINE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The most blob bytes the route holds at once, over all the answers it is reading
/// or sending: 1 GiB, [`kbf_front::MAX_BLOB_BYTES`], so the largest blob the cache accepts can
/// always be served when nothing else is.
pub const READ_BUDGET_BYTES: usize = 1 << 30;

/// How long a read waits in line for its share of [`READ_BUDGET_BYTES`] before it is
/// answered `503`: 10 seconds.
pub const READ_BUDGET_WAIT: Duration = Duration::from_secs(10);

/// The `Retry-After` of that `503`, in seconds.
const RETRY_AFTER_SECONDS: &str = "1";

/// The route's path prefix; the digest follows it.
const PREFIX: &str = "/v1/blobs/";

/// The call name refusals are logged with.
const CALL: &str = "GET /v1/blobs";

/// The bytes RFC 8187 lets a `filename*` value carry unencoded (`attr-char`): letters,
/// digits and `!#$&+-.^_`|~`. Every other byte is percent-encoded.
const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// Who may read blobs: the REAPI policy's authenticator and its `blobs.getAuthorizer`.
#[derive(Clone)]
pub struct BlobReaders {
    authenticator: Arc<dyn Authenticator>,
    authorizer: Arc<dyn Authorizer>,
}

impl BlobReaders {
    /// The readers `policy` allows, or `None` (the route is off) when it names no
    /// `blobs.getAuthorizer`, or no `invocations.getAuthorizer`: the blob route serves
    /// what the build pages link to, and is off with them
    /// (`docs/design/invocations.md` section 4.3).
    #[must_use]
    pub fn from_policy(policy: &Policy) -> Option<Self> {
        policy.invocations_get.as_ref()?;
        let authorizer = Arc::clone(policy.blobs_get.as_ref()?);
        Some(Self {
            authenticator: Arc::clone(&policy.authenticator),
            authorizer,
        })
    }
}

struct BlobState<M, O> {
    cache: Arc<Cache<M, O>>,
    readers: Option<BlobReaders>,
    /// One permit per byte of [`READ_BUDGET_BYTES`]; an answer's bytes hold theirs
    /// until the last of them is dropped.
    budget: Arc<Semaphore>,
    /// The whole budget in bytes: the largest blob a read is admitted for.
    largest: u64,
}

impl<M, O> BlobState<M, O> {
    /// The state of a route that holds at most `budget` blob bytes at once (at most
    /// [`Semaphore::MAX_PERMITS`]; the route's is [`READ_BUDGET_BYTES`]).
    fn new(cache: Arc<Cache<M, O>>, readers: Option<BlobReaders>, budget: usize) -> Self {
        Self {
            cache,
            readers,
            budget: Arc::new(Semaphore::new(budget)),
            largest: u64::try_from(budget).unwrap_or(u64::MAX),
        }
    }
}

/// A blob's bytes and the budget they hold: dropped with the last `Bytes` that
/// shares them.
struct Held {
    bytes: Bytes,
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for Held {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// The blob route over `cache`; with `readers` `None` it answers every request `404`.
pub fn router<M, O>(cache: Arc<Cache<M, O>>, readers: Option<BlobReaders>) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    Router::new()
        .route("/v1/blobs/{digest}", get(blob::<M, O>))
        .with_state(Arc::new(BlobState::new(cache, readers, READ_BUDGET_BYTES)))
}

async fn blob<M, O>(State(state): State<Arc<BlobState<M, O>>>, parts: Parts) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let mut answer = match answer(&state, &parts).await {
        Ok(served) | Err(served) => served,
    };
    let headers = answer.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    answer
}

/// The answer to one request, before the headers every answer carries.
async fn answer<M, O>(state: &BlobState<M, O>, parts: &Parts) -> Result<Response, Response>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let Some(readers) = &state.readers else {
        return Err(off(
            "blobs",
            "blob reads over HTTP are off: the REAPI policy names no blobs.getAuthorizer, \
             or no invocations.getAuthorizer, and --invocations-open is not given",
        ));
    };
    let caller = readers
        .authenticator
        .authenticate(parts)
        .await
        .map_err(|e| {
            tracing::warn!(call = CALL, error = %e.message(), "blob read not authenticated");
            refusal(&e)
        })?;
    let text = parts.uri.path().strip_prefix(PREFIX).unwrap_or_default();
    let digest = parse_digest(text).ok_or_else(|| {
        let why = format!("{text:?} is not a digest: <64 lowercase hex digits>-<size>");
        error(StatusCode::BAD_REQUEST, &why)
    })?;
    let query = Query::parse(parts.uri.query().unwrap_or_default())
        .map_err(|why| error(StatusCode::BAD_REQUEST, &why))?;
    authorize(&*readers.authorizer, &caller, CALL, &query.instance)
        .await
        .map_err(|e| refusal(&e))?;
    if query.inline && digest.size_bytes > INLINE_MAX_BYTES {
        let why = format!(
            "inline=text serves at most {INLINE_MAX_BYTES} bytes, and {digest} has more; \
             leave out inline to download it"
        );
        return Err(error(StatusCode::BAD_REQUEST, &why));
    }
    let permit = admit(Arc::clone(&state.budget), state.largest, &digest).await?;
    let bytes = state
        .cache
        .read_blob(&digest)
        .await
        .map_err(|e| refusal(&Status::from(e)))?;
    let bytes = Bytes::from_owner(Held {
        bytes,
        _permit: permit,
    });
    Ok(served(&bytes, &query, parts.headers.get(RANGE)))
}

/// `digest`'s share of the read budget, one permit per byte, taken in the order the
/// reads asked (the semaphore is fair: a large read at the head of the queue is not
/// passed by smaller ones behind it).
///
/// # Errors
/// `404` for a digest larger than the whole budget, `largest` bytes: the route's is
/// [`kbf_front::MAX_BLOB_BYTES`], so the cache never holds such a blob;
/// `503` with `Retry-After` and `"busy": true` when the share is not free within
/// [`READ_BUDGET_WAIT`].
async fn admit(
    budget: Arc<Semaphore>,
    largest: u64,
    digest: &Digest,
) -> Result<OwnedSemaphorePermit, Response> {
    let Some(permits) = u32::try_from(digest.size_bytes)
        .ok()
        .filter(|&n| u64::from(n) <= largest)
    else {
        // INVARIANT: the cache holds no blob over MAX_BLOB_BYTES (the route's
        // `largest`). `Cache::store_blobs`, the one way bytes are stored, refuses one;
        // ByteStream Write refuses it before buffering and batches are capped at
        // MAX_BATCH_TOTAL_BYTES. So this 404 is the answer a read would give.
        let why = format!(
            "{digest} is larger than the largest blob this cache accepts \
             ({largest} bytes), so it is not held"
        );
        return Err(error(StatusCode::NOT_FOUND, &why));
    };
    tokio::time::timeout(READ_BUDGET_WAIT, budget.acquire_many_owned(permits))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| busy(digest))
}

/// The `503` of a read whose share of the budget was not free in time: `Retry-After`
/// and `"busy": true`, so a client can tell it from a blob that cannot be read.
fn busy(digest: &Digest) -> Response {
    let wait = READ_BUDGET_WAIT.as_secs();
    let why = format!(
        "the reads in progress hold the memory {digest} needs ({READ_BUDGET_BYTES} bytes \
         for all reads), and it was not free within {wait} s; try again"
    );
    let body = serde_json::json!({ "error": why, "busy": true }).to_string();
    let headers = [
        (CONTENT_TYPE, HeaderValue::from_static(crate::api::JSON)),
        (RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECONDS)),
    ];
    (StatusCode::SERVICE_UNAVAILABLE, headers, body).into_response()
}

/// The bytes of a blob read, as the query and the `Range` header ask.
fn served(bytes: &Bytes, query: &Query, range: Option<&HeaderValue>) -> Response {
    let (kind, media) = if query.inline {
        ("inline", "text/plain; charset=utf-8")
    } else {
        ("attachment", "application/octet-stream")
    };
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(media));
    headers.insert(
        CONTENT_DISPOSITION,
        content_disposition(kind, query.name.as_deref()),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let len = bytes.len() as u64;
    let (code, body) = match wanted(range, len) {
        Wanted::Whole => (StatusCode::OK, bytes.clone()),
        Wanted::Part { first, last } => {
            headers.insert(CONTENT_RANGE, header(format!("bytes {first}-{last}/{len}")));
            // Both bounds are below the blob's length, which fits in memory.
            let part = bytes.slice(first as usize..=last as usize);
            (StatusCode::PARTIAL_CONTENT, part)
        }
        Wanted::Unsatisfiable => {
            headers.insert(CONTENT_RANGE, header(format!("bytes */{len}")));
            (StatusCode::RANGE_NOT_SATISFIABLE, Bytes::new())
        }
    };
    (code, headers, Body::from(body)).into_response()
}

/// A header value built from visible ASCII, which is always one.
fn header(text: String) -> HeaderValue {
    HeaderValue::try_from(text).expect("visible ASCII is a header value")
}

/// `Content-Disposition: <kind>`, with `; filename*=UTF-8''<name>` when a name is
/// given, every byte of it outside [`ATTR_CHAR`] percent-encoded (RFC 6266 and
/// RFC 8187): a quote, a `;` or a line break in the name can never end the value or
/// add a parameter.
fn content_disposition(kind: &'static str, name: Option<&str>) -> HeaderValue {
    match name {
        None => HeaderValue::from_static(kind),
        Some(name) => {
            let encoded = utf8_percent_encode(name, ATTR_CHAR);
            header(format!("{kind}; filename*=UTF-8''{encoded}"))
        }
    }
}

/// `<64 lowercase hex digits>-<size>`, the digest form of the operator API, or `None`.
pub(crate) fn parse_digest(text: &str) -> Option<Digest> {
    let (hash, size) = text.split_once('-')?;
    let reapi = format!("{hash}/{size}");
    Digest::parse(DigestFunction::Sha256, &reapi).ok()
}

/// What a request's query asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Query {
    /// `instance`: the instance name the authorizer is asked about; empty when absent.
    instance: String,
    /// `name`: the file name for `Content-Disposition`; `None` when absent or empty.
    name: Option<String>,
    /// `inline=text`.
    inline: bool,
}

impl Query {
    /// The query `text` (after the `?`), decoded as a form is.
    ///
    /// # Errors
    /// An unknown key, a key given twice, or an `inline` other than `text`.
    fn parse(text: &str) -> Result<Self, String> {
        let mut query = Self::default();
        let mut seen: Vec<String> = Vec::new();
        for (key, value) in form_urlencoded::parse(text.as_bytes()) {
            if seen.iter().any(|k| *k == key) {
                return Err(format!("{key:?} is given more than once"));
            }
            match &*key {
                "instance" => query.instance = value.into_owned(),
                "name" => query.name = Some(value.into_owned()).filter(|n| !n.is_empty()),
                "inline" if value == "text" => query.inline = true,
                "inline" => return Err(format!("inline={value:?}: the one form is inline=text")),
                _ => {
                    return Err(format!(
                        "unknown parameter {key:?}; the parameters are instance, name, inline"
                    ));
                }
            }
            seen.push(key.into_owned());
        }
        Ok(query)
    }
}

/// What part of a blob of some length a `Range` header asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wanted {
    /// All of it: no header, or one that is ignored.
    Whole,
    /// Bytes `first..=last`, both inside the blob.
    Part {
        /// The first byte.
        first: u64,
        /// The last byte, included.
        last: u64,
    },
    /// A range that starts at or past the end (or a suffix of an empty blob): `416`.
    Unsatisfiable,
}

/// The part of a blob of `len` bytes that `range` (a `Range` header's value) asks
/// for. One range of the unit `bytes` (any case) is honoured; several ranges, another
/// unit or a malformed value are ignored ([`Wanted::Whole`]). A last byte past the
/// end is read as the end.
fn wanted(range: Option<&HeaderValue>, len: u64) -> Wanted {
    let Some((unit, spec)) = range
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.split_once('='))
    else {
        return Wanted::Whole;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Wanted::Whole;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Wanted::Whole;
    }
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // `bytes=-n`: the last n bytes.
        return match number(last) {
            Some(n) if n > 0 && len > 0 => Wanted::Part {
                first: len - n.min(len),
                last: len - 1,
            },
            Some(_) => Wanted::Unsatisfiable,
            None => Wanted::Whole,
        };
    }
    let Some(first) = number(first) else {
        return Wanted::Whole;
    };
    // `bytes=a-`: from a to the end.
    let last = match number(last) {
        None if last.is_empty() => u64::MAX,
        Some(last) if last >= first => last,
        _ => return Wanted::Whole,
    };
    if first >= len {
        return Wanted::Unsatisfiable;
    }
    Wanted::Part {
        first,
        last: last.min(len - 1),
    }
}

/// A decimal number of digits only (no sign), or `None`; one too large for a `u64`
/// is `None` too.
fn number(text: &str) -> Option<u64> {
    if text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// The answer to a refusal of `status`'s code: 401, 403, 400, 404, 503 or 500.
pub(crate) fn refusal(status: &Status) -> Response {
    let code = match status.code() {
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::InvalidArgument => StatusCode::BAD_REQUEST,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(code, status.message())
}

/// The `404` of a route that is off: `{"error": <why>, "off": <which>}`, so a client
/// never reads it as "no such record" (`docs/design/invocations.md` 4.1).
pub(crate) fn off(which: &str, why: &str) -> Response {
    let body = serde_json::json!({ "error": why, "off": which }).to_string();
    (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, crate::api::JSON)],
        body,
    )
        .into_response()
}

/// A JSON error answer, as the rest of the API gives.
pub(crate) fn error(code: StatusCode, why: &str) -> Response {
    let body = serde_json::json!({ "error": why }).to_string();
    (code, [(CONTENT_TYPE, crate::api::JSON)], body).into_response()
}

#[cfg(test)]
mod tests;

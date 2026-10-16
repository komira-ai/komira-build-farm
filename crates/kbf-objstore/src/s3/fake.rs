//! An S3 service on loopback, for tests (feature `testkit`): PUT, ranged GET, and
//! multipart upload (create, parts, complete, abort), with S3's rules on
//! part sizes enforced at completion and conditional writes (`If-None-Match: *`) on a
//! PUT and a completion.
//!
//! It keeps what a test asks about: the uploads begun and not yet completed or aborted
//! ([`FakeS3::incomplete_uploads`]), how many single PUTs and parts it took and the
//! largest request body ([`FakeS3::stats`]). A test can fail any step of an upload
//! once ([`FakeS3::fail`]).
//!
//! Bytes are kept compactly: a run of zeros is counted, not stored, so an object of
//! many GiB of zeros costs the test nothing to hold. It does not check signatures.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{Credentials, S3Config, S3Store};
use crate::{Capabilities, ObjectKey};

/// The runs bytes are cut into when kept: a run of all zeros is only counted.
const RUN: usize = 64 << 10;

static ZEROS: [u8; RUN] = [0; RUN];

/// What the service took, for a test to check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeStats {
    /// PUTs of a whole object.
    pub single_puts: u64,
    /// Parts of multipart uploads.
    pub parts: u64,
    /// Multipart uploads completed.
    pub completed: u64,
    /// Multipart uploads aborted.
    pub aborted: u64,
    /// The largest request body received.
    pub largest_body: u64,
}

/// Bytes kept compactly.
#[derive(Clone, Debug, Default)]
struct Kept {
    /// Runs in order: `Err(n)` is `n` zero bytes.
    runs: Vec<Result<Bytes, u64>>,
    len: u64,
}

impl Kept {
    fn of(bytes: &[u8]) -> Self {
        let mut kept = Self::default();
        for run in bytes.chunks(RUN) {
            if run == &ZEROS[..run.len()] {
                kept.push(Err(run.len() as u64));
            } else {
                kept.push(Ok(Bytes::copy_from_slice(run)));
            }
        }
        kept
    }

    fn push(&mut self, run: Result<Bytes, u64>) {
        let n = match &run {
            Ok(b) => b.len() as u64,
            Err(n) => *n,
        };
        self.len += n;
        match (self.runs.last_mut(), run) {
            (Some(Err(zeros)), Err(more)) => *zeros += more,
            (_, run) => self.runs.push(run),
        }
    }

    fn extend(&mut self, other: &Self) {
        for run in &other.runs {
            self.push(run.clone());
        }
    }

    /// Bytes `[from, to)`.
    fn read(&self, from: u64, to: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::try_from(to - from).unwrap_or(0));
        let mut at = 0u64;
        for run in &self.runs {
            let n = match run {
                Ok(b) => b.len() as u64,
                Err(n) => *n,
            };
            let (start, end) = (from.max(at), to.min(at + n));
            if start < end {
                let (a, b) = ((start - at) as usize, (end - at) as usize);
                match run {
                    Ok(bytes) => out.extend_from_slice(&bytes[a..b]),
                    Err(_) => out.resize(out.len() + (b - a), 0),
                }
            }
            at += n;
            if at >= to {
                break;
            }
        }
        out
    }
}

/// A request [`FakeS3::fail`] makes the service fail, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// CreateMultipartUpload answers 500.
    Create,
    /// The upload of part `n` (of any upload) answers 500.
    Part(u32),
    /// The next part's answer has no ETag.
    NoEtag,
    /// CompleteMultipartUpload answers 500; the upload stays open.
    Complete,
    /// CompleteMultipartUpload answers 200 with an `<Error>` body, as S3 does when a
    /// completion fails after its answer started; the upload stays open.
    CompleteLate,
    /// AbortMultipartUpload answers 500; the upload stays open.
    Abort,
}

impl State {
    /// Whether `failure` was asked for; it is then used up.
    fn take(&mut self, failure: Failure) -> bool {
        let at = self.failures.iter().position(|f| *f == failure);
        at.map(|i| self.failures.remove(i)).is_some()
    }
}

/// One multipart upload in progress.
#[derive(Debug)]
struct Upload {
    key: String,
    parts: BTreeMap<u32, (String, Kept)>,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Kept>,
    uploads: BTreeMap<String, Upload>,
    next: u64,
    failures: Vec<Failure>,
    stats: FakeStats,
    requests: Vec<String>,
}

/// An S3 service on loopback (module docs). It serves until the runtime it was
/// started on stops.
#[derive(Debug, Clone)]
pub struct FakeS3 {
    endpoint: String,
    state: Arc<Mutex<State>>,
}

impl FakeS3 {
    /// Starts the service on a free loopback port, on the current runtime.
    ///
    /// # Panics
    /// No loopback port could be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let serving = accepted.map(|(sock, _)| serve(sock, Arc::clone(&shared)));
                serving.map(tokio::spawn).ok();
            }
        });
        Self { endpoint, state }
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// A store for bucket `kbf-fake` on this service, claiming `capabilities`.
    ///
    /// # Panics
    /// Never: the configuration is valid.
    #[must_use]
    pub fn store(&self, capabilities: Capabilities) -> S3Store {
        S3Store::new(S3Config {
            endpoint: self.endpoint.clone(),
            region: "us-east-1".to_owned(),
            bucket: "kbf-fake".to_owned(),
            credentials: Credentials::new("fake-key", "fake-secret"),
            capabilities,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        })
        .expect("a valid configuration")
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fails the next request `failure` names, once.
    pub fn fail(&self, failure: Failure) {
        self.lock().failures.push(failure);
    }

    /// Multipart uploads begun and neither completed nor aborted.
    #[must_use]
    pub fn incomplete_uploads(&self) -> usize {
        self.lock().uploads.len()
    }

    /// What the service took so far.
    #[must_use]
    pub fn stats(&self) -> FakeStats {
        self.lock().stats
    }

    /// The length of the object under `key`, if there is one.
    #[must_use]
    pub fn object_len(&self, key: &ObjectKey) -> Option<u64> {
        self.lock().objects.get(key.as_str()).map(|k| k.len)
    }

    /// Every request so far, as `METHOD /path?query`.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.lock().requests.clone()
    }
}

/// One request: method, path, query pairs, lowercase headers, body.
struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// Serves one connection's requests, one after another, until it closes.
async fn serve(mut sock: TcpStream, state: Arc<Mutex<State>>) -> Option<()> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; RUN];
    loop {
        let mut head_end = end_of_head(&buf);
        while head_end.is_none() {
            let n = sock.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
            buf.extend_from_slice(&chunk[..n]);
            head_end = end_of_head(&buf);
        }
        let head_end = head_end?;
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut request = parse_head(&head)?;
        let len: usize = request
            .headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        buf.reserve((head_end + len).saturating_sub(buf.len()));
        while buf.len() < head_end + len {
            let n = sock.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
            buf.extend_from_slice(&chunk[..n]);
        }
        let rest = buf.split_off(head_end + len);
        request.body = buf.split_off(head_end);
        buf = rest;
        let answer = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            handle(&mut state, &request)
        };
        sock.write_all(&answer).await.ok()?;
    }
}

/// Where a request's head ends in `buf`, past its blank line, once it is all there.
fn end_of_head(buf: &[u8]) -> Option<usize> {
    let blank = buf.windows(4).position(|w| w == b"\r\n\r\n");
    blank.map(|at| at + 4)
}

fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Some(Request {
        method,
        path: path.to_owned(),
        query,
        headers,
        body: Vec::new(),
    })
}

fn answer(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn error(status: &str, code: &str) -> Vec<u8> {
    let body = format!("<Error><Code>{code}</Code><Message>{code}</Message></Error>");
    answer(status, &[], body.as_bytes())
}

fn handle(state: &mut State, request: &Request) -> Vec<u8> {
    let query: Vec<String> = request
        .query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let line = format!("{} {}?{}", request.method, request.path, query.join("&"));
    state.requests.push(line);
    let body_len = request.body.len() as u64;
    state.stats.largest_body = state.stats.largest_body.max(body_len);
    // `/bucket/key`: the key is everything after the bucket.
    let key = request.path.trim_start_matches('/');
    let key = key.split_once('/').map_or("", |(_, k)| k).to_owned();
    let q = |name: &str| request.query.get(name).cloned();
    let conditional = request.headers.get("if-none-match").map(String::as_str) == Some("*");
    let method = request.method.as_str();
    match (method, q("uploads"), q("uploadId"), q("partNumber")) {
        ("POST", Some(_), None, None) => {
            if state.take(Failure::Create) {
                return error("500 Internal Server Error", "InternalError");
            }
            state.next += 1;
            let id = format!("upload-{}", state.next);
            let parts = BTreeMap::new();
            state.uploads.insert(id.clone(), Upload { key, parts });
            let body = format!(
                "<InitiateMultipartUploadResult><UploadId>{id}</UploadId>\
                 </InitiateMultipartUploadResult>"
            );
            answer("200 OK", &[], body.as_bytes())
        }
        ("PUT", None, Some(id), Some(number)) => {
            let number: u32 = number.parse().unwrap_or(0);
            let (failed, no_etag) = (
                state.take(Failure::Part(number)),
                state.take(Failure::NoEtag),
            );
            let upload = state.uploads.get_mut(&id).filter(|_| !failed);
            let Some(upload) = upload else {
                return error("500 Internal Server Error", "InternalError");
            };
            let etag = format!("\"{id}-{number}-{body_len}\"");
            let kept = Kept::of(&request.body);
            upload.parts.insert(number, (etag.clone(), kept));
            state.stats.parts += 1;
            let headers = if no_etag {
                vec![]
            } else {
                vec![("etag", etag)]
            };
            answer("200 OK", &headers, b"")
        }
        ("POST", None, Some(id), None) => complete(state, &id, &key, conditional, &request.body),
        ("DELETE", None, Some(id), None) => {
            if state.take(Failure::Abort) || state.uploads.remove(&id).is_none() {
                return error("500 Internal Server Error", "InternalError");
            }
            state.stats.aborted += 1;
            answer("204 No Content", &[], b"")
        }
        ("PUT", None, None, None) => {
            if conditional && state.objects.contains_key(&key) {
                return error("412 Precondition Failed", "PreconditionFailed");
            }
            state.objects.insert(key, Kept::of(&request.body));
            state.stats.single_puts += 1;
            answer("200 OK", &[], b"")
        }
        ("GET", None, None, None) => get(state, &key, request.headers.get("range")),
        _ => error("501 Not Implemented", "NotImplemented"),
    }
}

/// The object an upload's completion names: its parts, in the order the body lists
/// them, each with its ETag and every one but the last at least 5 MiB; or the S3 error
/// code that refuses it.
fn assemble(upload: &Upload, body: &[u8]) -> Result<Kept, &'static str> {
    let text = String::from_utf8_lossy(body);
    let doc = roxmltree::Document::parse(&text).map_err(|_| "MalformedXML")?;
    let field = |part: roxmltree::Node<'_, '_>, name: &str| {
        let node = part.children().find(|n| n.tag_name().name() == name);
        node.and_then(|n| n.text()).map(str::to_owned)
    };
    let parts = doc.root_element().children().filter(|n| n.is_element());
    let named: Vec<(Option<u32>, Option<String>)> = parts
        .map(|p| {
            (
                field(p, "PartNumber").and_then(|n| n.parse().ok()),
                field(p, "ETag"),
            )
        })
        .collect();
    let mut object = Kept::default();
    for (i, (number, etag)) in named.iter().enumerate() {
        let kept = number.and_then(|n| upload.parts.get(&n));
        let kept = kept
            .filter(|(e, _)| Some(e) == etag.as_ref())
            .ok_or("InvalidPart")?;
        let small = i + 1 < named.len() && kept.1.len < super::MIN_PART_BYTES;
        (!small).then_some(()).ok_or("EntityTooSmall")?;
        object.extend(&kept.1);
    }
    Ok(object)
}

/// CompleteMultipartUpload ([`assemble`]); conditional on `If-None-Match: *`.
fn complete(state: &mut State, id: &str, key: &str, conditional: bool, body: &[u8]) -> Vec<u8> {
    let upload = state.uploads.get(id).filter(|u| u.key == key);
    let object = match upload.ok_or("NoSuchUpload").and_then(|u| assemble(u, body)) {
        Ok(object) => object,
        Err(code) => return error("400 Bad Request", code),
    };
    if state.take(Failure::CompleteLate) {
        let body = "<Error><Code>InternalError</Code><Message>late</Message></Error>";
        return answer("200 OK", &[], body.as_bytes());
    }
    if state.take(Failure::Complete) {
        return error("500 Internal Server Error", "InternalError");
    }
    if conditional && state.objects.contains_key(key) {
        return error("412 Precondition Failed", "PreconditionFailed");
    }
    state.uploads.remove(id);
    state.objects.insert(key.to_owned(), object);
    state.stats.completed += 1;
    let done =
        "<CompleteMultipartUploadResult><ETag>\"done\"</ETag></CompleteMultipartUploadResult>";
    answer("200 OK", &[], done.as_bytes())
}

/// A GET of one `bytes=first-last` range (the S3 store asks for nothing else).
fn get(state: &State, key: &str, range: Option<&String>) -> Vec<u8> {
    let Some(object) = state.objects.get(key) else {
        return error("404 Not Found", "NoSuchKey");
    };
    let parsed = range
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
    let Some((first, last)) = parsed.filter(|(f, l)| f < &object.len && f <= l) else {
        return error("416 Range Not Satisfiable", "InvalidRange");
    };
    let end = (last + 1).min(object.len);
    let content_range = format!("bytes {first}-{}/{}", end - 1, object.len);
    let bytes = object.read(first, end);
    answer(
        "206 Partial Content",
        &[("content-range", content_range)],
        &bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: kept bytes that read back other than written (a zero run merged with
    /// its neighbour's length wrong, a range cut at the wrong run).
    #[test]
    fn kept_bytes_read_back_as_written() {
        let mut bytes = vec![0u8; 3 * RUN + 10];
        bytes[RUN + 5] = 7;
        bytes[3 * RUN + 9] = 9;
        let kept = Kept::of(&bytes);
        assert_eq!(kept.len, bytes.len() as u64);
        assert_eq!(kept.read(0, kept.len), bytes);
        assert_eq!(kept.read(RUN as u64, RUN as u64 + 6), bytes[RUN..RUN + 6]);
        let mut joined = Kept::of(&bytes[..RUN]);
        joined.extend(&Kept::of(&bytes[RUN..]));
        assert_eq!(joined.read(1, joined.len), bytes[1..]);
    }
}

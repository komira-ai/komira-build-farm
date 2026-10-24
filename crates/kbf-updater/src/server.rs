//! The roll API on a unix socket.
//!
//! The only peer is the front door that forwards operators' requests (the user
//! `--proxy-uid` names), and the trust boundary is narrow on purpose:
//!
//! - A connection whose `SO_PEERCRED` uid is not the configured proxy uid is closed without a
//!   byte read or written ([`peer_allowed`]).
//! - The updater trusts the `x-kbf-*` identity headers the proxy sets (`jti`,
//!   `job_workflow_ref`, `ref`, `sha`) and does nothing with them but write them to the audit
//!   log, one line per request. It never sees a credential: a request that carries an
//!   `Authorization` header is refused, because the proxy strips it and one that arrives means
//!   the proxy failed; and an identity header sent twice is refused as well, because the proxy
//!   strips the ones a caller sent and sets its own once.
//! - The body is capped at [`MAX_BODY`], every refusal is a code of the closed
//!   [`ErrorCode`] set, and no answer names a host's address or any inventory but the aliases
//!   of the roll the caller asked about.
//!
//! One request per connection, one thread per connection (at most
//! [`Settings::max_connections`] at once), a timeout on every read and write and a deadline
//! on the whole request.

use std::collections::VecDeque;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::Updater;
use crate::api::{ApiError, ErrorCode, Roll, RollRequest};
use crate::http::{ReadError, Request, read_request, write_response};

/// Where one audit line goes. The binary writes it to stderr (the journal).
pub type AuditSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The identity headers the proxy sets and the updater records.
const IDENTITY: [&str; 4] = [
    "x-kbf-jti",
    "x-kbf-job-workflow-ref",
    "x-kbf-ref",
    "x-kbf-sha",
];

/// The longest identity value or path written to the audit log, in characters.
const AUDIT_FIELD_MAX: usize = 256;

/// How many finished rolls `GET /rolls` lists.
const RECENT_MAX: usize = 16;

/// What the server needs to know besides the updater.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The one uid whose connections are served.
    pub proxy_uid: u32,
    /// How long one read or one write on a connection may take.
    pub io_timeout: Duration,
    /// How long the whole request, head and body, may take to arrive: a sender that stays
    /// under `io_timeout` with every read does not hold a connection for longer than this.
    pub request_deadline: Duration,
    /// Connections served at once; the next one is closed unread.
    pub max_connections: usize,
}

impl Settings {
    pub fn new(proxy_uid: u32) -> Settings {
        Settings {
            proxy_uid,
            io_timeout: Duration::from_secs(10),
            request_deadline: Duration::from_secs(30),
            max_connections: 16,
        }
    }
}

/// Rolls that finished since this process started, newest first. `roll.json` holds only the
/// latest roll, so this is all `GET /rolls` can list beyond it.
#[derive(Default)]
pub struct Recent(Mutex<VecDeque<Roll>>);

impl Recent {
    /// Remember `roll` if it is finished; a roll already listed is replaced, not repeated.
    pub fn push(&self, roll: &Roll) {
        if !roll.state.is_terminal() {
            return;
        }
        let mut list = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        list.retain(|r| r.id != roll.id);
        list.push_front(roll.clone());
        list.truncate(RECENT_MAX);
    }

    pub(crate) fn list_except(&self, id: Option<&str>) -> Vec<Roll> {
        let list = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        list.iter()
            .filter(|r| Some(r.id.as_str()) != id)
            .cloned()
            .collect()
    }
}

/// The check on a connection's peer: it must be known and must be the proxy.
pub fn peer_allowed(peer: Option<u32>, proxy_uid: u32) -> bool {
    peer == Some(proxy_uid)
}

/// The uid of the process at the other end, from `SO_PEERCRED`.
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live for the call and `len` is the size of `cred`; the
    // descriptor is borrowed from `stream`, which outlives the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// No peer credentials off Linux: every peer is refused.
#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> Option<u32> {
    None
}

/// A stream whose every read is bounded by the time left to the whole request.
struct Deadline<'a> {
    stream: &'a UnixStream,
    per_read: Duration,
    end: Instant,
}

impl Deadline<'_> {
    fn exceeded() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the request deadline passed before the request arrived",
        )
    }
}

impl std::io::Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Deadline::exceeded());
        }
        self.stream
            .set_read_timeout(Some(left.min(self.per_read)))?;
        let read = (&*self.stream).read(buf);
        if read.is_err() && Instant::now() >= self.end {
            return Err(Deadline::exceeded());
        }
        read
    }
}

/// An answer to give, and what to write down about it.
struct Answer {
    status: u16,
    body: String,
    code: Option<ErrorCode>,
    roll_id: Option<String>,
}

impl Answer {
    fn ok(status: u16, body: String, roll_id: &str) -> Answer {
        Answer {
            status,
            body,
            code: None,
            roll_id: Some(roll_id.to_owned()),
        }
    }

    fn refused(e: &ApiError) -> Answer {
        Answer {
            status: e.code.status(),
            body: serde_json::to_string(e).unwrap_or_default(),
            code: Some(e.code),
            roll_id: e.roll_id.clone(),
        }
    }
}

fn clip(s: &str) -> String {
    s.chars().take(AUDIT_FIELD_MAX).collect()
}

/// A roll id the updater could have issued: `roll-` and digits.
fn is_roll_id(s: &str) -> bool {
    s.strip_prefix("roll-")
        .is_some_and(|n| !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit()))
}

pub struct Server {
    updater: Arc<Updater>,
    settings: Settings,
    audit: AuditSink,
    /// Rolls to run, for the thread that drives them.
    drive: Sender<String>,
    recent: Arc<Recent>,
    active: AtomicUsize,
}

/// Counts a connection while it is served, whichever way the thread ends.
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Server {
    pub fn new(
        updater: Arc<Updater>,
        settings: Settings,
        audit: AuditSink,
        drive: Sender<String>,
        recent: Arc<Recent>,
    ) -> Server {
        Server {
            updater,
            settings,
            audit,
            drive,
            recent,
            active: AtomicUsize::new(0),
        }
    }

    fn audit(&self, line: &serde_json::Value) {
        (self.audit)(&line.to_string());
    }

    /// Accept until `stop` is set (and one more connection arrives to wake the accept).
    pub fn serve(self: &Arc<Server>, listener: &UnixListener, stop: &AtomicBool) {
        // An accept that fails (out of file descriptors, say) is skipped: the connection
        // cap keeps the process from using them up itself.
        for stream in listener.incoming().flatten() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if self.active.fetch_add(1, Ordering::SeqCst) >= self.settings.max_connections {
                self.active.fetch_sub(1, Ordering::SeqCst);
                self.audit(&json!({"event": "connection_refused", "why": "too many connections"}));
                continue;
            }
            let me = Arc::clone(self);
            std::thread::spawn(move || {
                let _slot = Slot(&me.active);
                me.connection(stream);
            });
        }
    }

    /// Serve one connection: check the peer, read the request, answer it, write the audit
    /// line.
    pub fn connection(&self, mut stream: UnixStream) {
        let peer = peer_uid(&stream);
        if !peer_allowed(peer, self.settings.proxy_uid) {
            self.audit(&json!({"event": "peer_refused", "peer_uid": peer}));
            return;
        }
        let _ = stream.set_write_timeout(Some(self.settings.io_timeout));
        let mut reader = Deadline {
            stream: &stream,
            per_read: self.settings.io_timeout,
            end: Instant::now() + self.settings.request_deadline,
        };
        let (request, answer) = match read_request(&mut reader) {
            Ok(request) => {
                let answer = self.answer(&request);
                (Some(request), answer)
            }
            Err(ReadError::Refused(e)) => (None, Answer::refused(&e)),
            Err(ReadError::Gone(why)) => {
                self.audit(&json!({"event": "connection_lost", "peer_uid": peer, "why": why}));
                return;
            }
        };
        let written = write_response(&mut stream, answer.status, &answer.body).is_ok();
        let seen = |name: &str| request.as_ref().and_then(|r| r.header(name)).map(clip);
        self.audit(&json!({
            "event": "request",
            "peer_uid": peer,
            "method": request.as_ref().map(|r| clip(&r.method)),
            "path": request.as_ref().map(|r| clip(&r.path)),
            "status": answer.status,
            "code": answer.code,
            "roll_id": answer.roll_id,
            "written": written,
            "jti": seen(IDENTITY[0]),
            "job_workflow_ref": seen(IDENTITY[1]),
            "ref": seen(IDENTITY[2]),
            "sha": seen(IDENTITY[3]),
        }));
    }

    fn answer(&self, req: &Request) -> Answer {
        match Server::guard(req).and_then(|()| self.route(req)) {
            Ok(answer) => answer,
            Err(e) => Answer::refused(&e),
        }
    }

    /// The trust boundary, before any route is looked at.
    fn guard(req: &Request) -> Result<(), ApiError> {
        if req.header("authorization").is_some() {
            return Err(ApiError::new(
                ErrorCode::CredentialsPresent,
                "an Authorization header reached the updater; the proxy must strip it",
            ));
        }
        if IDENTITY.iter().any(|name| req.repeated(name)) {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "an identity header is repeated; the proxy sets each once",
            ));
        }
        Ok(())
    }

    fn route(&self, req: &Request) -> Result<Answer, ApiError> {
        let path = req.path.as_str();
        let segments: Vec<&str> = path.split('/').collect();
        match (req.method.as_str(), segments.as_slice()) {
            ("POST", ["", "v1", "updater", "rolls"]) => self.post_roll(&req.body),
            ("GET", ["", "v1", "updater", "rolls"]) => self.list(),
            ("GET", ["", "v1", "updater", "rolls", id]) => self.get(id),
            ("POST", ["", "v1", "updater", "rolls", id, "abort"]) => self.abort(id),
            (
                _,
                ["", "v1", "updater", "rolls"]
                | ["", "v1", "updater", "rolls", _]
                | ["", "v1", "updater", "rolls", _, "abort"],
            ) => Err(ApiError::new(
                ErrorCode::MethodNotAllowed,
                format!("{} is not allowed here", clip(&req.method)),
            )),
            _ => Err(ApiError::new(ErrorCode::NotFound, "no such route")),
        }
    }

    fn post_roll(&self, body: &[u8]) -> Result<Answer, ApiError> {
        let request = RollRequest::from_json(body)?;
        let started = self.updater.start(&request)?;
        let id = started.roll.id.clone();
        if started.created && self.drive.send(id.clone()).is_err() {
            self.audit(&json!({"event": "driver_gone", "roll_id": id}));
        }
        let status = if started.created { 202 } else { 200 };
        let body = json!({"roll_id": id, "state": started.roll.state}).to_string();
        Ok(Answer::ok(status, body, &id))
    }

    fn list(&self) -> Result<Answer, ApiError> {
        let current = self.updater.current()?;
        let id = current.as_ref().map(|r| r.id.as_str());
        let body = json!({"current": current, "recent": self.recent.list_except(id)});
        Ok(Answer {
            status: 200,
            body: body.to_string(),
            code: None,
            roll_id: None,
        })
    }

    fn get(&self, id: &str) -> Result<Answer, ApiError> {
        let roll = self.updater.get(Server::known(id)?)?;
        Ok(Answer::ok(
            200,
            serde_json::to_string(&roll).unwrap_or_default(),
            id,
        ))
    }

    fn abort(&self, id: &str) -> Result<Answer, ApiError> {
        let roll = self.updater.abort(Server::known(id)?)?;
        self.recent.push(&roll);
        Ok(Answer::ok(
            200,
            serde_json::to_string(&roll).unwrap_or_default(),
            id,
        ))
    }

    /// A path segment that could be a roll id, or the 404 for one that cannot (the text is
    /// not echoed).
    fn known(id: &str) -> Result<&str, ApiError> {
        if is_roll_id(id) {
            Ok(id)
        } else {
            Err(ApiError::new(ErrorCode::UnknownRoll, "no such roll"))
        }
    }
}

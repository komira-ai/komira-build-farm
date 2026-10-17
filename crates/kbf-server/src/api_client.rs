//! A blocking client of the operator API ([`crate::api`]), for `kbf cluster`.
//!
//! The API listens in plain HTTP on the server's own host and takes writes from
//! loopback only, so this is one HTTP/1.1 request per connection over a [`TcpStream`]:
//! no TLS, no redirects, no keep-alive. It lives next to the routes it calls, so the
//! paths are the same constants the server mounts them at, and the server's tests run
//! against it.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crate::api::{JSON, node_path};

/// How long a connect, a write or a read may take before the call fails. A drain
/// answers at once (it does not wait for the streams), so a server that does not answer
/// in this time is not one to wait for.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes of an answer that are read.
const REPLY_MAX: u64 = 1024 * 1024;

/// Why a call did not get an answer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server could not be reached, or the connection failed.
    #[error("{addr}: {source}")]
    Io {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    /// The answer is not an HTTP/1.1 response this client reads.
    #[error("{addr}: {why}")]
    Malformed { addr: SocketAddr, why: &'static str },
    /// The token file cannot be read, or holds no token.
    #[error("token file {path}: {why}")]
    Token { path: String, why: String },
}

/// An answer: the status code and the body.
#[derive(Debug)]
pub struct Reply {
    /// The HTTP status code.
    pub status: u16,
    /// The body, which the API always sends as JSON.
    pub body: String,
}

impl Reply {
    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The operator API at `addr`, with the token its writes need. It has no `Debug`: the
/// token must not reach a log line.
pub struct OperatorClient {
    addr: SocketAddr,
    token: Option<String>,
}

impl OperatorClient {
    /// A client of the API at `addr`; `token` is sent with every request (reads
    /// ignore it).
    #[must_use]
    pub const fn new(addr: SocketAddr, token: Option<String>) -> Self {
        Self { addr, token }
    }

    /// The token in the file at `path`, without surrounding whitespace.
    ///
    /// # Errors
    /// The file cannot be read, or holds nothing but whitespace. Its mode is not
    /// checked here: the server checked the mode of its own copy.
    pub fn read_token(path: &Path) -> Result<String, ClientError> {
        let fail = |why: String| ClientError::Token {
            path: path.display().to_string(),
            why,
        };
        let content = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
        let token = content.trim();
        if token.is_empty() {
            return Err(fail("it holds no token".to_owned()));
        }
        Ok(token.to_owned())
    }

    /// `GET path`.
    ///
    /// # Errors
    /// See [`ClientError`]; a non-2xx status is a [`Reply`], not an error.
    pub fn get(&self, path: &str) -> Result<Reply, ClientError> {
        self.call("GET", path, None)
    }

    /// `POST path` with `body` (empty for none), sent as JSON.
    ///
    /// # Errors
    /// See [`ClientError`]; a non-2xx status is a [`Reply`], not an error.
    pub fn post(&self, path: &str, body: &str) -> Result<Reply, ClientError> {
        self.call("POST", path, Some(body))
    }

    /// `POST /v1/nodes/{node}:{verb}` with `body`.
    ///
    /// # Errors
    /// As [`OperatorClient::post`].
    pub fn node_action(&self, node: &str, verb: &str, body: &str) -> Result<Reply, ClientError> {
        self.post(&node_path(node, verb), body)
    }

    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<Reply, ClientError> {
        let io = |source| ClientError::Io {
            addr: self.addr,
            source,
        };
        let mut stream = TcpStream::connect_timeout(&self.addr, TIMEOUT).map_err(io)?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {JSON}\r\n",
            self.addr
        );
        if let Some(token) = &self.token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(body) = body {
            let length = body.len();
            request.push_str(&format!(
                "Content-Type: {JSON}\r\nContent-Length: {length}\r\n"
            ));
        }
        request.push_str("\r\n");
        request.push_str(body.unwrap_or_default());
        stream.write_all(request.as_bytes()).map_err(io)?;
        let mut raw = Vec::new();
        stream.take(REPLY_MAX).read_to_end(&mut raw).map_err(io)?;
        parse_reply(&raw).map_err(|why| ClientError::Malformed {
            addr: self.addr,
            why,
        })
    }
}

/// The status and body of a response the server closed the connection after (every
/// request asks for `Connection: close`, so the body runs to the end of the stream, or
/// to its `Content-Length`).
fn parse_reply(raw: &[u8]) -> Result<Reply, &'static str> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the response has no end of headers")?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| "the response head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut words = status_line.split(' ');
    if !words.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err("the response is not HTTP/1.x");
    }
    let status = words
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("the status line has no status code")?;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("a chunked response (the operator API sends none)");
        }
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = &raw[end + 4..];
    if let Some(length) = length {
        if body.len() < length {
            return Err("the response body is shorter than its Content-Length");
        }
        body = &body[..length];
    }
    let body = String::from_utf8(body.to_vec()).map_err(|_| "the response body is not UTF-8")?;
    Ok(Reply { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;

    /// A server that answers one connection with `raw` and closes it.
    fn answering(raw: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a call");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(raw);
        });
        addr
    }

    /// Catches an answer this client cannot read becoming a reply, or an error that
    /// does not name the server and what is wrong with it; and a server nothing listens
    /// at becoming anything but an error naming it.
    #[test]
    fn a_call_names_the_server_that_failed_it() {
        // A write with a token: every arm of `call` runs in this copy of it (coverage
        // counts the best single build of a function: the unit-test one must reach the
        // token and body arms the integration tests reach too).
        let garbage = answering(b"not http at all");
        let client = OperatorClient::new(garbage, Some("token".to_owned()));
        let err = client.post("/", "{}").expect_err("garbage");
        assert!(matches!(err, ClientError::Malformed { .. }), "{err:?}");
        assert!(err.to_string().starts_with(&garbage.to_string()), "{err}");

        let plain = answering(b"nor this");
        let err = OperatorClient::new(plain, None)
            .get("/")
            .expect_err("garbage");
        assert!(matches!(err, ClientError::Malformed { .. }), "{err:?}");

        let again = answering(b"still not http");
        let err = OperatorClient::new(again, None)
            .node_action("n", "drain", "")
            .expect_err("garbage");
        assert!(matches!(err, ClientError::Malformed { .. }), "{err:?}");

        let gone = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr")
        };
        let err = OperatorClient::new(gone, None)
            .get("/")
            .expect_err("nothing there");
        assert!(matches!(err, ClientError::Io { .. }), "{err:?}");
        assert!(err.to_string().starts_with(&gone.to_string()), "{err}");
    }

    /// Catches a token file read with its newline (the bearer would never match), an
    /// empty or missing one accepted, and an error that does not name the file.
    #[test]
    fn a_token_is_the_trimmed_content_of_its_file() {
        let dir = std::env::temp_dir().join(format!("kbf-api-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        let file = |name: &str, content: &str| {
            let path = dir.join(name);
            std::fs::write(&path, content).expect("write");
            path
        };
        let good = file("good", "  abc123\n");
        assert_eq!(
            OperatorClient::read_token(&good).expect("a token"),
            "abc123"
        );
        for (name, content) in [("empty", ""), ("blank", " \n")] {
            let path = file(name, content);
            let err = OperatorClient::read_token(&path).expect_err(name);
            assert!(
                err.to_string().contains(&path.display().to_string()),
                "{err}"
            );
        }
        let missing = dir.join("missing");
        let err = OperatorClient::read_token(&missing).expect_err("missing");
        assert!(
            err.to_string().contains(&missing.display().to_string()),
            "{err}"
        );
    }

    fn reply(raw: &str) -> Result<Reply, &'static str> {
        parse_reply(raw.as_bytes())
    }

    /// Catches a body cut at the wrong place (a Content-Length ignored or trusted past
    /// the bytes read) and a status that is not the line's.
    #[test]
    fn a_reply_is_its_status_and_its_content_length_of_body() {
        let ok = reply("HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\n{} \nextra").expect("parses");
        assert_eq!((ok.status, ok.body.as_str()), (200, "{} \n"));
        assert!(ok.is_success());
        let refused =
            reply("HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}").expect("401");
        assert_eq!((refused.status, refused.is_success()), (401, false));
        let odd = reply("HTTP/1.1 200 OK\r\nnot a header line\r\nContent-Type: x\r\nContent-Length: 2\r\n\r\nok")
            .expect("odd");
        assert_eq!(odd.body, "ok", "a line that is no header is skipped");
        let to_end = reply("HTTP/1.0 200 OK\r\n\r\nrest").expect("no length");
        assert_eq!(to_end.body, "rest");
    }

    /// Catches an answer this client would misread rather than refuse.
    #[test]
    fn what_is_not_a_plain_response_is_refused() {
        for raw in [
            "",
            "HTTP/1.1 200 OK\r\ncontent-length: 4",
            "SMTP/1.1 200 OK\r\n\r\n",
            "HTTP/1.1 two OK\r\n\r\n",
            "HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nshort",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            assert!(reply(raw).is_err(), "{raw:?}");
        }
    }
}

//! Just enough HTTP/1.1 for the updater's unix socket: read one request, write one answer,
//! close. The only peer is the front door of the socket, which sends one request per connection, so there is
//! no keep-alive, no chunked body, no `Expect`, no pipelining: a request that needs any of that
//! is refused, not guessed at.

use std::io::{Read, Write};

use crate::api::{ApiError, ErrorCode, MAX_BODY};

/// The longest request head (request line and headers) read, in bytes.
pub const MAX_HEAD: usize = 8192;

/// The most headers a request may carry.
const MAX_HEADERS: usize = 32;

/// One request, read.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lower-cased name, value as text, in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether header `name` (lower case) appears more than once.
    pub fn repeated(&self, name: &str) -> bool {
        self.headers.iter().filter(|(n, _)| n == name).count() > 1
    }
}

/// Why no request was read.
#[derive(Debug)]
pub enum ReadError {
    /// The connection failed, timed out or closed before a whole request arrived: there is
    /// nobody to answer.
    Gone(String),
    /// The bytes are a request the updater refuses, with the answer to give.
    Refused(ApiError),
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> ReadError {
    ReadError::Refused(ApiError::new(code, message))
}

fn too_long() -> ReadError {
    refuse(
        ErrorCode::BadRequest,
        format!("the request head is longer than {MAX_HEAD} bytes"),
    )
}

fn gone(e: &std::io::Error) -> ReadError {
    ReadError::Gone(e.to_string())
}

/// Read one request from `r`: the head up to [`MAX_HEAD`] bytes, then a body of at most
/// [`MAX_BODY`] bytes named by `Content-Length`. A body length over the cap is refused before
/// the body is read.
pub fn read_request(r: &mut impl Read) -> Result<Request, ReadError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let (head_len, method, path, headers) = loop {
        let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut slots);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                let headers = req
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_ascii_lowercase(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect::<Vec<_>>();
                let method = req.method.unwrap_or_default().to_owned();
                break (n, method, req.path.unwrap_or_default().to_owned(), headers);
            }
            Ok(httparse::Status::Partial) if buf.len() >= MAX_HEAD => return Err(too_long()),
            Ok(httparse::Status::Partial) => {}
            Err(e) => {
                return Err(refuse(
                    ErrorCode::BadRequest,
                    format!("unreadable request head: {e}"),
                ));
            }
        }
        // Never past the cap: a head that is not complete by then is too long.
        let room = chunk.len().min(MAX_HEAD - buf.len());
        let n = r.read(&mut chunk[..room]).map_err(|e| gone(&e))?;
        if n == 0 {
            return Err(ReadError::Gone("closed before a whole request".to_owned()));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(refuse(
            ErrorCode::BadRequest,
            "Transfer-Encoding is not accepted; send a Content-Length",
        ));
    }
    if request.repeated("content-length") {
        return Err(refuse(ErrorCode::BadRequest, "Content-Length is repeated"));
    }
    let want = match request.header("content-length") {
        None => 0,
        Some(v) => v.trim().parse::<usize>().map_err(|_| {
            refuse(
                ErrorCode::BadRequest,
                "Content-Length is not a number of bytes",
            )
        })?,
    };
    if want > MAX_BODY {
        return Err(refuse(
            ErrorCode::BodyTooLarge,
            format!("the body is longer than {MAX_BODY} bytes"),
        ));
    }
    let mut body = buf.split_off(head_len);
    while body.len() < want {
        let n = r.read(&mut chunk).map_err(|e| gone(&e))?;
        if n == 0 {
            return Err(ReadError::Gone("closed before the whole body".to_owned()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(want);
    request.body = body;
    Ok(request)
}

/// The reason phrase of a status the updater sends.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Write the answer: a JSON body, the length, and `Connection: close`.
pub fn write_response(w: &mut impl Write, status: u16, body: &str) -> std::io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read(raw: &[u8]) -> Result<Request, ReadError> {
        read_request(&mut Cursor::new(raw.to_vec()))
    }

    /// The code `raw` is refused with, as the debug text of the error (a refusal is the only
    /// error that has `ApiError { code` in it).
    fn refused(raw: &[u8]) -> String {
        format!("{:?}", read(raw).err())
    }

    fn assert_refused(raw: &[u8], code: ErrorCode) {
        let got = refused(raw);
        assert!(
            got.contains(&format!("Refused(ApiError {{ code: {code:?},")),
            "{got}"
        );
    }

    /// A reader that hands out one byte per call, as a slow peer would, then fails.
    struct Drip(Vec<u8>, usize);

    impl Read for Drip {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.1 >= self.0.len() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            out[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }

    /// Catches a request read wrongly: method, path, lower-cased headers and a body that
    /// arrives with the head or after it, a byte at a time.
    #[test]
    fn a_request_is_read_whole_however_it_arrives() {
        let raw = b"POST /v1/updater/rolls HTTP/1.1\r\nHost: x\r\nX-Kbf-Jti: abc\r\n\
                    Content-Length: 5\r\n\r\nhello";
        let r = read(raw).expect("request");
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("POST", "/v1/updater/rolls")
        );
        assert_eq!(r.header("x-kbf-jti"), Some("abc"));
        assert_eq!(r.header("nope"), None);
        assert_eq!(r.body, b"hello");
        let dripped = read_request(&mut Drip(raw.to_vec(), 0)).expect("dripped");
        assert_eq!(dripped.body, b"hello");
        // A request with no body and no length reads an empty body.
        assert!(
            read(b"GET / HTTP/1.1\r\n\r\n")
                .expect("get")
                .body
                .is_empty()
        );
        // Bytes past the declared length are not part of the body.
        let extra = read(b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nabcdef").expect("x");
        assert_eq!(extra.body, b"ab");
    }

    /// Catches a body cap that is applied after the read (a 4 GiB length would be buffered),
    /// is off by one, or that a repeated or chunked length gets around.
    #[test]
    fn the_body_cap_is_checked_on_the_declared_length() {
        let head = |n: usize| format!("POST / HTTP/1.1\r\nContent-Length: {n}\r\n\r\n");
        // Over the cap with no body sent at all: refused without waiting for it.
        assert_refused(head(MAX_BODY + 1).as_bytes(), ErrorCode::BodyTooLarge);
        assert_refused(head(usize::MAX).as_bytes(), ErrorCode::BodyTooLarge);
        let at = format!("{}{}", head(MAX_BODY), "x".repeat(MAX_BODY));
        assert_eq!(
            read(at.as_bytes()).expect("at the cap").body.len(),
            MAX_BODY
        );
        for bad in [
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: ten\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n",
        ] {
            assert_refused(bad.as_bytes(), ErrorCode::BadRequest);
        }
    }

    /// Catches a malformed or oversized head accepted, and a closed or failing connection
    /// answered as if it were a request.
    #[test]
    fn a_bad_head_is_refused_and_a_dead_peer_is_gone() {
        assert_refused(b"\x01\x02 nonsense\r\n\r\n", ErrorCode::BadRequest);
        let long = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_refused(long.as_bytes(), ErrorCode::BadRequest);
        // A head that never ends is cut at the cap too.
        let endless = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD));
        assert_refused(endless.as_bytes(), ErrorCode::BadRequest);
        let many = format!(
            "GET / HTTP/1.1\r\n{}\r\n",
            "a: b\r\n".repeat(MAX_HEADERS + 1)
        );
        assert_refused(many.as_bytes(), ErrorCode::BadRequest);
        // Closed before the head ends, and before the body ends.
        for cut in [
            &b"GET / HT"[..],
            b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nab",
        ] {
            assert!(matches!(read(cut), Err(ReadError::Gone(_))), "{cut:?}");
        }
        // A timeout in the head and in the body.
        for cut in [
            &b"GET / HT"[..],
            b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nab",
        ] {
            let r = read_request(&mut Drip(cut.to_vec(), 0));
            assert!(format!("{:?}", r.err()).contains("Gone(\"timed out"));
        }
    }

    /// Catches an answer that is not parseable HTTP: status line, JSON type, exact length and
    /// close.
    #[test]
    fn an_answer_carries_its_length_and_closes() {
        let mut out = Vec::new();
        write_response(&mut out, 202, "{\"a\":1}").expect("write");
        assert_eq!(
            String::from_utf8(out).expect("text"),
            "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\
             Connection: close\r\n\r\n{\"a\":1}"
        );
        for (status, text) in [
            (200, "OK"),
            (400, "Bad Request"),
            (404, "Not Found"),
            (405, "Method Not Allowed"),
            (409, "Conflict"),
            (413, "Content Too Large"),
            (503, "Service Unavailable"),
            (500, "Error"),
        ] {
            assert_eq!(reason(status), text);
        }
        // A writer that fails, in the write or in the flush, fails the answer.
        struct Broken(bool);
        impl Write for Broken {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    Ok(b.len())
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        assert!(write_response(&mut Broken(false), 200, "{}").is_err());
        assert!(write_response(&mut Broken(true), 200, "{}").is_err());
        assert!(
            Request {
                method: String::new(),
                path: String::new(),
                headers: vec![("a".into(), "1".into()), ("a".into(), "2".into())],
                body: Vec::new()
            }
            .repeated("a")
        );
    }
}

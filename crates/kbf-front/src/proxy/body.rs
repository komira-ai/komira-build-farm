//! The body the raw path hands from one side to the other.
//!
//! A [`Pass`] hands HTTP/2 frames on as they come and holds at most the frames peeked at
//! (to read the routing key, or to see that a read began) and the one it last handed
//! on. Nothing is collected: the demand of the side that reads it is what pulls the next
//! frame from the side that sends, so a slow consumer slows its producer.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body as _, Frame};
use kbf_types::Digest;
use tonic::body::Body;
use tonic::{Code, Status};

use super::owners::Owners;
use super::{Direction, Meter, Method};

/// `status` as the trailers that end a call: its `grpc-status`, `grpc-message` and
/// details, without the HTTP headers a trailers-only response also carries.
pub(super) fn trailers_of(status: &Status) -> tonic::codegen::http::HeaderMap {
    let all = status.clone().into_http::<Body>().into_parts().0.headers;
    let mut trailers = tonic::codegen::http::HeaderMap::new();
    for (name, value) in &all {
        if name.as_str().starts_with("grpc-") {
            trailers.append(name.clone(), value.clone());
        }
    }
    trailers
}

/// The longest resource name the raw path reads: far above any real one, so a client
/// cannot make the proxy hold a long first message to find its name.
pub(super) const MAX_NAME_BYTES: usize = 4096;

/// The most bytes of a unary request (`Read`, `QueryWriteStatus`) the proxy collects:
/// it keeps the request to replay it at the next owner.
pub(super) const MAX_UNARY_BYTES: usize = 64 << 10;

/// What happened to a response's first frames, before any byte reached the client.
pub(super) enum Probe {
    /// A data frame is ready.
    Data,
    /// The response ended, or carries trailers with an OK status, and no data.
    Done,
    /// The node failed before the first byte.
    Failed(Status),
}

/// Ends an upload whose primary is no longer the node it began at.
pub(super) struct Guard {
    pub owners: Owners,
    pub digest: Digest,
    pub owner: Arc<str>,
    /// The refusal, kept so that the call answers it rather than what the broken body
    /// made the node say.
    pub failed: Arc<Mutex<Option<Status>>>,
}

impl Guard {
    fn check(&self) -> Result<(), Status> {
        if self
            .owners
            .writer(&self.digest)
            .is_ok_and(|writer| writer.id() == &*self.owner)
        {
            return Ok(());
        }
        let refusal = Status::unavailable(format!(
            "the owner of {} changed from {} during the upload; start it again",
            self.digest, self.owner
        ));
        *self
            .failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refusal.clone());
        Err(refusal)
    }
}

/// A body that counts the wire bytes passing through it and the bytes it holds.
pub(super) struct Pass {
    queue: VecDeque<Frame<Bytes>>,
    inner: Body,
    meter: Arc<Meter>,
    method: Method,
    direction: Direction,
    /// The data bytes of the frame handed on last: held until the next one is asked for.
    last: u64,
    guard: Option<Guard>,
    done: bool,
}

impl Pass {
    pub fn new(inner: Body, meter: &Arc<Meter>, method: Method, direction: Direction) -> Self {
        Self {
            queue: VecDeque::new(),
            inner,
            meter: Arc::clone(meter),
            method,
            direction,
            last: 0,
            guard: None,
            done: false,
        }
    }

    /// A body that is `bytes` and ends: a request replayed to a node.
    pub fn replay(bytes: Bytes, meter: &Arc<Meter>, method: Method, direction: Direction) -> Self {
        let mut pass = Self::new(Body::empty(), meter, method, direction);
        meter.hold(bytes.len() as u64);
        pass.queue.push_back(Frame::data(bytes));
        pass
    }

    pub fn guarded(mut self, guard: Guard) -> Self {
        self.guard = Some(guard);
        self
    }

    /// The next frame of `inner`, counted as held.
    fn pull(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        if let Some(Err(refusal)) = self.guard.as_ref().map(Guard::check) {
            return Poll::Ready(Some(Err(refusal)));
        }
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            self.meter
                .hold(frame.data_ref().map_or(0, |d| d.len() as u64));
        }
        polled
    }

    /// Reads frames until the first message holds its whole `resource_name` (field 1 of
    /// every ByteStream request), and returns it. The frames read are kept and replayed
    /// first by the body.
    ///
    /// # Errors
    /// INVALID_ARGUMENT when the first message is compressed, does not begin with
    /// `resource_name` or ends before it; the client's own error if the request broke.
    pub async fn peek_name(&mut self) -> Result<String, Status> {
        let mut seen = Vec::new();
        loop {
            if let Some(name) = name_in(&seen)? {
                return Ok(name);
            }
            let Some(frame) = poll_fn(|cx| self.pull(cx)).await else {
                return Err(Status::invalid_argument(
                    "the request ended before the first message named a resource",
                ));
            };
            let frame = frame?;
            seen.extend_from_slice(frame.data_ref().map_or(&[][..], Bytes::as_ref));
            self.queue.push_back(frame);
        }
    }

    /// Reads the response up to its first data frame, or its end, and says whether the
    /// node failed before it.
    pub async fn probe(&mut self) -> Probe {
        let frame = match poll_fn(|cx| self.pull(cx)).await {
            Some(Ok(frame)) => frame,
            Some(Err(status)) => return Probe::Failed(status),
            None => return Probe::Done,
        };
        let failed = frame
            .trailers_ref()
            .and_then(Status::from_header_map)
            .filter(|status| status.code() != Code::Ok);
        if let Some(status) = failed {
            return Probe::Failed(status);
        }
        let data = frame.is_data();
        self.queue.push_back(frame);
        if data { Probe::Data } else { Probe::Done }
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        let queued: usize = self
            .queue
            .iter()
            .map(|f| f.data_ref().map_or(0, Bytes::len))
            .sum();
        self.meter.release(self.last + queued as u64);
    }
}

impl http_body::Body for Pass {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        let this = self.get_mut();
        this.meter.release(std::mem::take(&mut this.last));
        let polled = if let Some(frame) = this.queue.pop_front() {
            Poll::Ready(Some(Ok(frame)))
        } else if this.done {
            Poll::Ready(None)
        } else {
            this.pull(cx)
        };
        match polled {
            Poll::Ready(Some(Ok(frame))) => {
                this.last = frame.data_ref().map_or(0, |d| d.len() as u64);
                this.meter.add(this.method, this.direction, this.last);
                Poll::Ready(Some(Ok(frame)))
            }
            // A node that breaks mid-response ends the call with that status as the
            // trailers the client reads, not a reset it can only call a protocol error.
            Poll::Ready(Some(Err(status))) if this.direction == Direction::FromStorage => {
                this.done = true;
                Poll::Ready(Some(Ok(Frame::trailers(trailers_of(&status)))))
            }
            other => other,
        }
    }
}

/// The whole of a unary request body, at most [`MAX_UNARY_BYTES`].
///
/// # Errors
/// INVALID_ARGUMENT over the limit; the client's own error if the request broke.
pub(super) async fn collect(mut body: Body) -> Result<Bytes, Status> {
    let mut all = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = frame?;
        let data = frame.data_ref().map_or(&[][..], Bytes::as_ref);
        if all.len() + data.len() > MAX_UNARY_BYTES {
            return Err(Status::invalid_argument(format!(
                "a Read or QueryWriteStatus request is over {MAX_UNARY_BYTES} bytes"
            )));
        }
        all.extend_from_slice(data);
    }
    Ok(all.into())
}

/// The `resource_name` of the gRPC message that begins `buf` (the 5-byte prefix, then
/// protobuf field 1), `None` while more bytes are needed.
///
/// # Errors
/// INVALID_ARGUMENT: the message is compressed (its flag is set), its first field is not
/// `resource_name`, the name is over [`MAX_NAME_BYTES`] or not UTF-8, or the message is
/// shorter than its name.
pub(super) fn name_in(buf: &[u8]) -> Result<Option<String>, Status> {
    if buf.first().is_some_and(|flag| *flag != 0) {
        return Err(compressed("the message's compressed flag is set"));
    }
    let Some(&tag) = buf.get(5) else {
        return Ok(None);
    };
    if tag != 0x0a {
        return Err(Status::invalid_argument(
            "the first field of the first message must be resource_name",
        ));
    }
    let (mut len, mut at) = (0usize, 6usize);
    loop {
        let Some(&byte) = buf.get(at) else {
            return Ok(None);
        };
        len |= usize::from(byte & 0x7f) << (7 * (at - 6));
        at += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if at - 6 == 3 {
            return Err(too_long());
        }
    }
    if len > MAX_NAME_BYTES {
        return Err(too_long());
    }
    let end = at + len;
    // `buf` has the prefix's four length bytes: it is at least 6 long here.
    let message = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if end - 5 > message {
        return Err(Status::invalid_argument(
            "the first message is shorter than its resource_name",
        ));
    }
    let Some(name) = buf.get(at..end) else {
        return Ok(None);
    };
    String::from_utf8(name.to_vec())
        .map(Some)
        .map_err(|_| Status::invalid_argument("resource_name is not UTF-8"))
}

fn too_long() -> Status {
    Status::invalid_argument(format!("resource_name is over {MAX_NAME_BYTES} bytes"))
}

/// The refusal of a compressed ByteStream message: the proxy routes by the first
/// message's name, which a compressed message hides.
pub(super) fn compressed(why: &str) -> Status {
    Status::invalid_argument(format!(
        "gRPC message compression is not supported on ByteStream through the frontend \
         ({why}); send identity messages (REAPI compressed-blobs names are accepted)"
    ))
}

#[cfg(test)]
mod tests {
    use tonic::transport::Endpoint;

    use super::super::owners::Owner;
    use super::*;

    /// One gRPC message: prefix with `flag`, then field 1 holding `name`, then `more`.
    fn message(flag: u8, name: &str, more: &[u8]) -> Vec<u8> {
        let mut proto = vec![0x0a];
        let mut len = name.len();
        while len >= 0x80 {
            proto.push((len & 0x7f) as u8 | 0x80);
            len >>= 7;
        }
        proto.push(len as u8);
        proto.extend_from_slice(name.as_bytes());
        proto.extend_from_slice(more);
        let mut out = vec![flag];
        out.extend_from_slice(&(proto.len() as u32).to_be_bytes());
        out.extend(proto);
        out
    }

    /// Catches: a name read too early (a prefix of the name routes the call), a name
    /// that needs a longer varint than one byte misread, and bytes after the name read
    /// into it. Every cut of the message but the whole one needs more bytes.
    #[test]
    fn the_name_is_read_only_when_whole() {
        let long = "n".repeat(300);
        for name in ["main/blobs/ab/7", long.as_str()] {
            let m = message(0, name, &[0x10, 0x05, 0x1a, 0x01, 0xff]);
            let whole = 6 + if name.len() < 128 { 1 } else { 2 } + name.len();
            for cut in 0..whole {
                assert_eq!(name_in(&m[..cut]).unwrap(), None, "cut at {cut}");
            }
            assert_eq!(name_in(&m[..whole]).unwrap().as_deref(), Some(name));
            assert_eq!(name_in(&m).unwrap().as_deref(), Some(name));
        }
        // No name: the field is present and empty.
        assert_eq!(name_in(&message(0, "", &[])).unwrap().as_deref(), Some(""));
    }

    /// Catches: each refusal turned into a route (compressed hides the name; another
    /// first field has none; a long name holds the proxy; the length lying about the
    /// message; a name that is not text).
    #[test]
    fn what_cannot_be_routed_is_refused_naming_why() {
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        let mut short = message(0, "abcdef", &[]);
        short[4] = 3; // the message claims 3 bytes: the name runs past it
        let mut not_text = message(0, "ab", &[]);
        not_text[8] = 0xff;
        let mut endless = vec![0, 0, 0, 0, 20, 0x0a];
        endless.extend([0x80; 4]);
        let wrong_field = [0, 0, 0, 0, 3, 0x12, 0x01, b'a'];
        for (bytes, why) in [
            (message(1, "a", &[]), "compress"),
            (message(0, &long, &[]), "over 4096"),
            (short, "shorter"),
            (not_text, "UTF-8"),
            (endless, "over 4096"),
            (wrong_field.to_vec(), "must be resource_name"),
        ] {
            let e = name_in(&bytes).unwrap_err();
            assert_eq!(e.code(), Code::InvalidArgument);
            assert!(e.message().contains(why), "{why}: {}", e.message());
        }
        // The flag is read from the first byte alone.
        assert!(name_in(&[1]).is_err());
    }

    /// A body that yields these results, then ends.
    struct Script(VecDeque<Result<Frame<Bytes>, Status>>);

    impl http_body::Body for Script {
        type Data = Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn data(n: usize) -> Result<Frame<Bytes>, Status> {
        Ok(Frame::data(Bytes::from(vec![7; n])))
    }

    fn trailers(status: &Status) -> Result<Frame<Bytes>, Status> {
        Ok(Frame::trailers(trailers_of(status)))
    }

    fn pass(script: Vec<Result<Frame<Bytes>, Status>>, direction: Direction) -> (Pass, Arc<Meter>) {
        let meter = Arc::new(Meter::default());
        let body = Body::new(Script(script.into()));
        (Pass::new(body, &meter, Method::Read, direction), meter)
    }

    async fn next(pass: &mut Pass) -> Option<Result<Frame<Bytes>, Status>> {
        poll_fn(|cx| Pin::new(&mut *pass).poll_frame(cx)).await
    }

    /// Catches: a response that failed before its first byte read as a success (the
    /// next owner is never tried), a good one read as failed, and the frame looked at
    /// lost (the client would miss its first bytes). Each of the response's first frames
    /// is probed: an error, an early end, data, trailers of a failure, trailers of an OK.
    #[tokio::test]
    async fn a_probe_sees_failure_before_the_first_byte() {
        let lost = Status::data_loss("gone");
        let cases = [
            (vec![Err(Status::unavailable("reset"))], "failed"),
            (vec![], "done"),
            (vec![data(5), data(6)], "data"),
            (vec![trailers(&lost)], "failed"),
            (vec![trailers(&Status::ok(""))], "done"),
        ];
        for (script, want) in cases {
            let (mut p, meter) = pass(script, Direction::FromStorage);
            let got = match p.probe().await {
                Probe::Failed(_) => "failed",
                Probe::Done => "done",
                Probe::Data => "data",
            };
            assert_eq!(got, want);
            assert_eq!(
                meter.bytes(Method::Read, Direction::FromStorage),
                0,
                "not counted until handed on"
            );
            if got == "data" {
                // The probed frame comes first; nothing is lost behind it.
                let first = next(&mut p).await.unwrap().unwrap();
                assert_eq!(first.data_ref().unwrap().len(), 5);
                let second = next(&mut p).await.unwrap().unwrap();
                assert_eq!(second.data_ref().unwrap().len(), 6);
                assert_eq!(meter.bytes(Method::Read, Direction::FromStorage), 11);
                assert!(next(&mut p).await.is_none());
            }
            drop(p);
            assert_eq!(meter.held(), 0, "{want}: held bytes are released");
        }
    }

    /// Catches: a node that breaks mid-response ending the call as a protocol reset
    /// (the client could only call it an h2 error, and not resume), a break that is
    /// delivered twice, and the held gauge left up by a frame never handed on.
    #[tokio::test]
    async fn a_break_mid_response_becomes_the_trailers() {
        let script = vec![data(4), Err(Status::unavailable("reset")), data(9)];
        let (mut p, meter) = pass(script, Direction::FromStorage);
        assert_eq!(
            next(&mut p)
                .await
                .unwrap()
                .unwrap()
                .data_ref()
                .unwrap()
                .len(),
            4
        );
        let t = next(&mut p).await.unwrap().unwrap();
        let status = Status::from_header_map(t.trailers_ref().unwrap()).unwrap();
        assert_eq!(
            (status.code(), status.message()),
            (Code::Unavailable, "reset")
        );
        assert!(
            next(&mut p).await.is_none(),
            "the response ends after the trailers"
        );
        assert_eq!(meter.bytes(Method::Read, Direction::FromStorage), 4);
        assert_eq!(meter.held(), 0);
        // The client-facing direction keeps the error as an error.
        let (mut up, _) = pass(
            vec![Err(Status::cancelled("client left"))],
            Direction::ToStorage,
        );
        assert_eq!(
            next(&mut up).await.unwrap().unwrap_err().code(),
            Code::Cancelled
        );
    }

    /// Catches: a name searched for past the end of the request (a hang), or a client
    /// that breaks before naming anything routed, and bytes dropped while peeking.
    #[tokio::test]
    async fn peeking_ends_with_the_request() {
        let (mut p, _) = pass(vec![], Direction::ToStorage);
        let e = p.peek_name().await.unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
        let (mut p, _) = pass(vec![Err(Status::cancelled("reset"))], Direction::ToStorage);
        assert_eq!(p.peek_name().await.unwrap_err().code(), Code::Cancelled);
        // A name in two frames, then the rest of the message: all replayed, in order.
        let whole = message(0, "main/blobs/ab/7", &[1, 2, 3]);
        let (a, b) = whole.split_at(8);
        let frame = |bytes: &[u8]| Ok(Frame::data(Bytes::copy_from_slice(bytes)));
        let trailing = trailers(&Status::ok(""));
        let (mut p, meter) = pass(
            vec![frame(a), trailing, frame(b), frame(&[9])],
            Direction::ToStorage,
        );
        assert_eq!(p.peek_name().await.unwrap(), "main/blobs/ab/7");
        let mut replayed = Vec::new();
        while let Some(f) = next(&mut p).await {
            replayed.extend_from_slice(f.unwrap().data_ref().map_or(&[][..], Bytes::as_ref));
        }
        assert_eq!(replayed, [whole.as_slice(), &[9]].concat());
        assert_eq!(
            meter.bytes(Method::Read, Direction::ToStorage),
            replayed.len() as u64
        );
        assert_eq!(meter.held(), 0);
    }

    /// Catches: a unary request read without bound (a client could make the proxy hold
    /// whatever it sends), and a client that breaks mid-request treated as complete.
    #[tokio::test]
    async fn a_unary_request_is_collected_to_a_limit() {
        let ok = Body::new(Script(vec![data(10), data(20)].into()));
        assert_eq!(collect(ok).await.unwrap().len(), 30);
        let big = Body::new(Script(vec![data(MAX_UNARY_BYTES), data(1)].into()));
        assert_eq!(
            collect(big).await.unwrap_err().code(),
            Code::InvalidArgument
        );
        let broken = Body::new(Script(
            vec![data(10), Err(Status::cancelled("reset"))].into(),
        ));
        assert_eq!(collect(broken).await.unwrap_err().code(), Code::Cancelled);
        let with_trailers = Body::new(Script(vec![trailers(&Status::ok(""))].into()));
        assert!(collect(with_trailers).await.unwrap().is_empty());
    }

    /// Catches: an upload that goes on after its primary changed (the new primary would
    /// hold half a blob), and a refusal that is not kept for the call to answer.
    #[tokio::test]
    async fn a_guard_ends_the_upload_when_the_primary_changes() {
        let node = |id: &str| {
            Owner::new(
                id,
                Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            )
        };
        let owners = Owners::single(node("a"));
        let digest = kbf_segments::sha256(b"x");
        let failed = Arc::new(Mutex::new(None));
        let guard = Guard {
            owners: owners.clone(),
            digest,
            owner: Arc::from("a"),
            failed: Arc::clone(&failed),
        };
        let (p, _) = pass(vec![data(1), data(2), data(3)], Direction::ToStorage);
        let mut p = p.guarded(guard);
        assert!(next(&mut p).await.unwrap().is_ok());
        owners.replace(vec![node("b"), node("a")]);
        let refusal = next(&mut p).await.unwrap().unwrap_err();
        assert_eq!(refusal.code(), Code::Unavailable);
        assert!(
            refusal.message().contains("changed from a"),
            "{}",
            refusal.message()
        );
        assert_eq!(
            failed.lock().unwrap().take().unwrap().message(),
            refusal.message()
        );
        owners.replace(vec![]);
        assert!(
            next(&mut p).await.unwrap().is_err(),
            "no primary at all is a change too"
        );
    }
}

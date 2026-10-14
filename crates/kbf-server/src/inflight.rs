//! A bound on the gRPC message bytes the server's listeners hold for calls in flight.
//!
//! The cache services accept messages of up to [`kbf_front::MAX_MESSAGE_BYTES`] (a
//! gigabyte), and a connection may have [`kbf_front::MAX_CONCURRENT_STREAMS`] calls
//! open. tonic decodes a message into memory as the client sends it, before any
//! handler (and so before [`kbf_front::Cache::admit`]) runs, so without this layer a
//! client could make the server hold hundreds of gigabytes.
//!
//! [`InFlightLayer`] reads the length the first message of a call declares in its own
//! five-byte prefix, and takes that many bytes from a budget
//! ([`kbf_front::MESSAGE_BYTES_IN_FLIGHT`]) before it lets the first byte of the
//! message through. Until the budget has room the body is not read, so HTTP/2 flow
//! control holds the client back after one stream window. The share is held until the
//! call's response has been sent. A call that declares more than the whole budget takes
//! all of it and runs alone; the cache's own message limit refuses it if it is too big.
//!
//! The layer sits outside authentication but only a handler reads a body, so a call
//! that authentication refuses never takes a share. It also holds a batch read's
//! admission ([`kbf_front::ResponseHold`]) until the response body has been sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::body::Body;
use tonic::codegen::{Bytes, Service, http};

/// The most one call is charged: what a semaphore call takes at once.
const MOST_ONE_CALL: u64 = u32::MAX as u64;

/// What a gRPC message prefix is: a compression flag and a big-endian length.
const PREFIX_BYTES: usize = 5;

/// Shares one budget of message bytes between every call on every listener it is
/// applied to.
#[derive(Clone, Debug)]
pub struct InFlightLayer {
    budget: Arc<Semaphore>,
    total: u64,
}

impl InFlightLayer {
    /// A layer with a budget of `bytes` (at most what a semaphore holds).
    #[must_use]
    pub fn new(bytes: u64) -> Self {
        let total = bytes.min(Semaphore::MAX_PERMITS as u64).max(1);
        Self {
            // `total` is at most `MAX_PERMITS`, a `usize`.
            budget: Arc::new(Semaphore::new(total as usize)),
            total,
        }
    }

    /// The bytes taken from the budget now: what running calls hold plus whatever a
    /// waiting call has accrued toward its own share (a semaphore hands a waiter what
    /// is free as it waits).
    #[must_use]
    pub fn held(&self) -> u64 {
        self.total - self.budget.available_permits() as u64
    }
}

impl<S> tower_layer::Layer<S> for InFlightLayer {
    type Service = InFlight<S>;

    fn layer(&self, inner: S) -> InFlight<S> {
        InFlight {
            inner,
            layer: self.clone(),
        }
    }
}

/// The service [`InFlightLayer`] wraps around a server's routes.
#[derive(Clone, Debug)]
pub struct InFlight<S> {
    inner: S,
    layer: InFlightLayer,
}

/// The permit a call took, shared by its request body and the call's future so it is
/// freed when both are gone.
type Share = Arc<OnceLock<OwnedSemaphorePermit>>;

impl<S, ResB> Service<http::Request<Body>> for InFlight<S>
where
    S: Service<http::Request<Body>, Response = http::Response<ResB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResB: HttpBody<Data = Bytes> + Send + 'static,
    ResB::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        // `poll_ready` was called on `self.inner`, so that is the one that is called.
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let share = Share::default();
        let (parts, body) = request.into_parts();
        let gated = Gated {
            inner: body,
            state: Phase::First,
            budget: Arc::clone(&self.layer.budget),
            total: self.layer.total,
            share: Arc::clone(&share),
        };
        let request = http::Request::from_parts(parts, Body::new(gated));
        let call = inner.call(request);
        Box::pin(async move {
            let response = call.await?;
            let (mut parts, body) = response.into_parts();
            let hold = parts.extensions.remove::<kbf_front::ResponseHold>();
            let body = Body::new(Held {
                inner: Body::new(body),
                _share: share,
                _hold: hold,
            });
            Ok(http::Response::from_parts(parts, body))
        })
    }
}

/// The request body of one call, which takes the call's share before its first message.
struct Gated {
    inner: Body,
    state: Phase,
    budget: Arc<Semaphore>,
    total: u64,
    share: Share,
}

enum Phase {
    /// Nothing read yet.
    First,
    /// The first frame is read and waits for the budget.
    Waiting {
        frame: Option<Frame<Bytes>>,
        acquire: Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>,
    },
    /// The share is taken (or none is needed): frames pass through.
    Open,
}

/// How many bytes the message that starts `data` declares, or the most one message
/// may when `data` is too short to say.
fn declared(data: &[u8], most: u64) -> u64 {
    match data.get(..PREFIX_BYTES) {
        Some([_, a, b, c, d]) => u64::from(u32::from_be_bytes([*a, *b, *c, *d])),
        _ => most,
    }
}

impl HttpBody for Gated {
    type Data = Bytes;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                Phase::First => {
                    let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(frame))) => frame,
                        done @ Poll::Ready(_) => {
                            this.state = Phase::Open;
                            return done;
                        }
                    };
                    let Some(data) = frame.data_ref() else {
                        this.state = Phase::Open;
                        return Poll::Ready(Some(Ok(frame)));
                    };
                    let charge = declared(data, this.total)
                        .min(this.total)
                        .min(MOST_ONE_CALL);
                    if charge == 0 {
                        this.state = Phase::Open;
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    let budget = Arc::clone(&this.budget);
                    // `charge` is at most `u32::MAX`.
                    let n = u32::try_from(charge).unwrap_or(u32::MAX);
                    this.state = Phase::Waiting {
                        frame: Some(frame),
                        acquire: Box::pin(async move { budget.acquire_many_owned(n).await.ok() }),
                    };
                }
                Phase::Waiting { frame, acquire } => match acquire.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(permit) => {
                        let frame = frame.take();
                        if let Some(permit) = permit {
                            // Set once: this body is the only writer.
                            let _ = this.share.set(permit);
                        }
                        this.state = Phase::Open;
                        return Poll::Ready(frame.map(Ok));
                    }
                },
                Phase::Open => return Pin::new(&mut this.inner).poll_frame(cx),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, Phase::Open) && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A response body that keeps what its call took until the last byte has been sent or
/// the body is dropped.
struct Held {
    inner: Body,
    _share: Share,
    _hold: Option<kbf_front::ResponseHold>,
}

impl HttpBody for Held {
    type Data = Bytes;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use http_body_util::{BodyExt as _, Full};
    use kbf_front::Cache;
    use tower_layer::Layer as _;

    use super::*;

    /// A gRPC message prefix declaring `declared` bytes, then `present` of them.
    fn message(declared: u32, present: usize) -> Bytes {
        let mut out = vec![0u8];
        out.extend_from_slice(&declared.to_be_bytes());
        out.resize(PREFIX_BYTES + present, 0);
        Bytes::from(out)
    }

    fn request(declared: u32) -> http::Request<Body> {
        http::Request::new(Body::new(Full::new(message(declared, 16))))
    }

    /// A service that (when `read`) reads its request's first frame, counts itself
    /// started, then waits for a permit of `gate` before it answers.
    #[derive(Clone)]
    struct Probe {
        read: bool,
        started: Arc<AtomicUsize>,
        gate: Arc<Semaphore>,
        hold: Option<kbf_front::ResponseHold>,
    }

    impl Probe {
        fn new(read: bool) -> Self {
            Self {
                read,
                started: Arc::new(AtomicUsize::new(0)),
                gate: Arc::new(Semaphore::new(0)),
                hold: None,
            }
        }
    }

    impl Service<http::Request<Body>> for Probe {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<Body>) -> Self::Future {
            let this = self.clone();
            Box::pin(async move {
                let mut body = request.into_body();
                if this.read {
                    let frame = body.frame().await.expect("a frame").expect("ok");
                    assert!(frame.is_data(), "the first frame arrives whole");
                }
                this.started.fetch_add(1, Ordering::SeqCst);
                this.gate.acquire().await.expect("open").forget();
                // A body with data: tonic turns one that has already ended into an empty body,
                // dropping whatever wraps it.
                let mut response =
                    http::Response::new(Body::new(Full::new(Bytes::from_static(b"x"))));
                if let Some(hold) = this.hold {
                    response.extensions_mut().insert(hold);
                }
                Ok(response)
            })
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Catches: a layer that lets calls decode without regard to each other. Budget
    /// 1000, two calls declaring 600 each: the second must not read a byte (so its
    /// stream window holds the client back) until the first has answered, and must
    /// then proceed. Mutant: no budget wait (the second starts at once, `started` is 2
    /// before the gate opens).
    #[tokio::test]
    async fn a_second_call_waits_until_the_first_has_answered() {
        let layer = InFlightLayer::new(1000);
        let probe = Probe::new(true);
        let mut service = layer.layer(probe.clone());
        let first = tokio::spawn(service.call(request(600)));
        let second = tokio::spawn(service.call(request(600)));
        settle().await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 1);
        probe.gate.add_permits(1);
        let first = first.await.expect("task").expect("answer");
        settle().await;
        // The first's response body is still alive, so its share is too.
        assert_eq!(probe.started.load(Ordering::SeqCst), 1);
        drop(first);
        settle().await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 2);
        probe.gate.add_permits(1);
        drop(second.await.expect("task").expect("answer"));
        assert_eq!(layer.held(), 0);
    }

    /// Catches: a call declaring more than the whole budget waiting for bytes that can
    /// never be free (a hang), or being charged more than the budget. Mutant: charge
    /// the declared length without the cap (the call never starts).
    #[tokio::test]
    async fn a_call_over_the_whole_budget_runs_alone_and_takes_all_of_it() {
        let layer = InFlightLayer::new(100);
        let probe = Probe::new(true);
        let mut service = layer.layer(probe.clone());
        let call = tokio::spawn(service.call(request(1_000_000)));
        settle().await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 1);
        assert_eq!(layer.held(), 100);
        probe.gate.add_permits(1);
        drop(call.await.expect("task").expect("answer"));
        assert_eq!(layer.held(), 0);
    }

    /// Catches: a share taken when the call is accepted rather than when its body is
    /// read: a call that authentication refuses never reads its body, and must not
    /// take budget from calls that do.
    #[tokio::test]
    async fn a_call_that_never_reads_its_body_takes_nothing() {
        let layer = InFlightLayer::new(1000);
        let probe = Probe::new(false);
        let mut service = layer.layer(probe.clone());
        let call = tokio::spawn(service.call(request(900)));
        settle().await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 1);
        assert_eq!(layer.held(), 0);
        probe.gate.add_permits(1);
        drop(call.await.expect("task").expect("answer"));
    }

    /// Catches: a batch read's admission freed when its handler returns instead of
    /// when its response has been sent (the response and its encoding are copies of
    /// the data). The hold travels in the response's extensions and must stay counted
    /// while the response body is alive, and go with it. Mutant: leave the hold in the
    /// extensions (freed with the headers).
    #[tokio::test]
    async fn a_read_admission_lasts_as_long_as_the_response_body() {
        let cache = Cache::memory();
        let layer = InFlightLayer::new(1000);
        let mut probe = Probe::new(false);
        probe.hold = Some(cache.admit_read(10).await);
        assert_eq!(cache.read_bytes_admitted(), 10 * kbf_front::READ_COPIES);
        let mut service = layer.layer(probe.clone());
        let call = tokio::spawn(service.call(request(0)));
        probe.gate.add_permits(1);
        let response = call.await.expect("task").expect("answer");
        // The probe's own clones of the hold go; the body's is the one left.
        drop((probe, service));
        assert_eq!(cache.read_bytes_admitted(), 10 * kbf_front::READ_COPIES);
        drop(response);
        assert_eq!(cache.read_bytes_admitted(), 0);
    }
}

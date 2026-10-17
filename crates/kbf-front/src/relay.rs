//! The Execute relay (`docs/design/aio-handover.md`, section 5.2): what a frontend that
//! has handed its roles to a successor does with an `Execute` or `WaitExecution` that
//! still reaches it.
//!
//! [`Relay`] wraps the `Execution` service. While its farm has not handed over
//! ([`Handed::handed`]) every call goes to the wrapped service as if the relay were not
//! there. Once it has, a call is opened to the successor's REAPI listener over the channel
//! the relay is given (it dials nothing itself, as [`crate::Proxy`] does not), with the
//! caller's request metadata, and the successor's stream is sent back to the caller to its
//! end. What the successor says is the answer: its operation name is kept, and nothing
//! is answered from this process's scheduler.
//!
//! **The successor authorizes again.** The metadata of the call (the bearer token, the
//! request metadata header) is copied to the call the relay opens; what a transport hides
//! from the metadata (a client certificate of mTLS) is not, so the successor sees this
//! process's identity there. The deadline is not copied: the relay holds it itself (see
//! below), and a call it drops is cancelled at the successor.
//!
//! **A broken stream is not a lost one.** If the stream to the successor ends before the
//! operation is done (the successor restarted), the relay looks the operation up in the
//! state store by its action (the successor's operation name does not carry the operation
//! id; the unfinished row names the request, so this is done while the first operation
//! arrives) and follows it as [`Handed::look`] says, from the `done/` rows, until it ends:
//! the same follower a frontend runs for the streams it kept open. A stream that has
//! sent nothing yet, or whose operation could not be found, ends with the status it broke
//! with: nothing is known that could answer it.
//!
//! **Nothing is held outside the open call.** The relay has no queue and no parked
//! request. A call waits on the successor inside its own gRPC call; the caller's deadline
//! (`grpc-timeout`) ends a call the successor never starts.
//!
//! **Counted.** Every relayed call adds to `kbf_handover_relayed_total` and is in
//! `kbf_handover_relays_open` until its stream ends: a relay keeps this process alive for
//! as long as its action runs.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::stream;
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{
    ExecuteOperationMetadata, ExecuteRequest, WaitExecutionRequest, execution_stage,
};
use kbf_types::{Digest, OperationId};
use prost::Message;
use tonic::metadata::MetadataMap;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};

use crate::backend::Backend;
use crate::execution::{Closing, OperationStream, Progress, Stage, closing, operation};
use crate::{OpenStreams, wire};

/// How often a relay that fell back to the rows looks in the state store.
pub const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// The farm of a frontend that may hand its roles to a successor, as the relay asks it.
///
/// A trait because `kbf-front` does not know the scheduler's state store (the farm is
/// `kbf-server`'s); the tests of the relay script it.
pub trait Handed: Send + Sync + 'static {
    /// Whether this process has handed its roles over: from now on a call is the
    /// successor's.
    fn handed(&self) -> bool;

    /// The one unfinished operation of the state store that runs `action` (of `instance`,
    /// if it is known), if there is exactly one. A call that joined a twin has the
    /// twin's operation.
    ///
    /// # Errors
    /// The state store cannot be read.
    fn operation_of(
        &self,
        instance: Option<&str>,
        action: &Digest,
    ) -> Result<Option<OperationId>, String>;

    /// One look at `operation` in the state store: the stage to show now (executing,
    /// done, or lost after the follower's looks), or `None` when nothing changes.
    ///
    /// # Errors
    /// The state store cannot be read.
    fn look(&self, operation: OperationId) -> Result<Option<Stage>, String>;
}

/// The `Execution` service `E`, and the calls it forwards once its farm has handed over.
pub struct Relay<E> {
    inner: E,
    farm: Arc<dyn Handed>,
    successor: Channel,
    backend: Backend,
    closing: Closing,
    progress: Progress,
    follow_every: Duration,
}

impl<E> std::fmt::Debug for Relay<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay").finish_non_exhaustive()
    }
}

impl<E> Relay<E> {
    /// `inner`, with its calls forwarded over `successor` once `farm` has handed over;
    /// `backend` is what counts them. Streams are never closed and never re-sent until
    /// [`Relay::with_streams`] says so.
    #[must_use]
    pub fn new(inner: E, farm: Arc<dyn Handed>, successor: Channel, backend: Backend) -> Self {
        Self {
            inner,
            farm,
            successor,
            backend,
            closing: closing().1,
            progress: Progress::off(),
            follow_every: FOLLOW_EVERY,
        }
    }

    /// This relay, its streams ended when `streams.closing` closes and re-sent at every
    /// tick of `streams.progress` once they follow the state store (the successor sends
    /// its own while it is the one answering).
    #[must_use]
    pub fn with_streams(mut self, streams: OpenStreams) -> Self {
        self.closing = streams.closing;
        self.progress = streams.progress;
        self
    }

    /// This relay, looking in the state store every `every` once it follows it.
    #[must_use]
    pub const fn following_every(mut self, every: Duration) -> Self {
        self.follow_every = every;
        self
    }

    fn client(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.successor.clone())
            .max_decoding_message_size(crate::MAX_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_MESSAGE_BYTES)
    }

    /// Counts a call that is being relayed, until the guard drops.
    fn begin(&self) -> Open {
        let metrics = self.backend.metrics();
        metrics.handover_relayed.add(1);
        metrics.handover_relays_open.add_one();
        Open(self.backend.clone())
    }

    /// The caller's stream of a call opened to the successor.
    fn relayed(
        &self,
        live: Streaming<Operation>,
        instance: Option<String>,
        open: Open,
    ) -> OperationStream {
        let relayed = Relayed {
            leg: Leg::Live(live),
            instance,
            tracked: None,
            shown: Stage::Queued,
            farm: Arc::clone(&self.farm),
            closing: self.closing.clone(),
            progress: self.progress.clone(),
            every: self.follow_every,
            _open: open,
        };
        Box::pin(stream::unfold(Some(relayed), |state| async move {
            state?.next().await
        }))
    }
}

#[tonic::async_trait]
impl<E> Execution for Relay<E>
where
    E: Execution<ExecuteStream = OperationStream, WaitExecutionStream = OperationStream>,
{
    type ExecuteStream = OperationStream;
    type WaitExecutionStream = OperationStream;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        if !self.farm.handed() {
            return self.inner.execute(request).await;
        }
        let instance = request.get_ref().instance_name.clone();
        let (call, deadline) = forwarded(request);
        let open = self.begin();
        let live = opened(deadline, self.client().execute(call)).await?;
        Ok(Response::new(self.relayed(
            live.into_inner(),
            Some(instance),
            open,
        )))
    }

    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        if !self.farm.handed() {
            return self.inner.wait_execution(request).await;
        }
        let (call, deadline) = forwarded(request);
        let open = self.begin();
        let live = opened(deadline, self.client().wait_execution(call)).await?;
        Ok(Response::new(self.relayed(live.into_inner(), None, open)))
    }
}

/// One relayed call, counted: `kbf_handover_relays_open` is one higher until this drops,
/// however the stream ends.
struct Open(Backend);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.metrics().handover_relays_open.sub_one();
    }
}

/// The headers a transport sets for itself, and the deadline, which the relay keeps: the
/// call the relay opens gets its own.
const TRANSPORT_HEADERS: [&str; 6] = [
    "te",
    "content-type",
    "content-length",
    "host",
    "grpc-encoding",
    "grpc-timeout",
];

/// `request` as the call to open to the successor: the same message and the metadata of
/// the caller, less the transport's own headers. And the caller's deadline.
fn forwarded<T>(request: Request<T>) -> (Request<T>, Option<Duration>) {
    let (mut metadata, _extensions, message) = request.into_parts();
    let deadline = deadline(&metadata);
    for header in TRANSPORT_HEADERS {
        metadata.remove(header);
    }
    let mut call = Request::new(message);
    *call.metadata_mut() = metadata;
    (call, deadline)
}

/// The deadline in a call's `grpc-timeout` header: `None` when it is absent or not one
/// the gRPC wire format writes (at most eight digits and a unit of `H M S m u n`).
fn deadline(metadata: &MetadataMap) -> Option<Duration> {
    let value = metadata.get("grpc-timeout")?.to_str().ok()?;
    let (digits, unit) = value.split_at_checked(value.len().checked_sub(1)?)?;
    if digits.len() > 8 {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    match unit {
        "H" => Some(Duration::from_secs(n * 3600)),
        "M" => Some(Duration::from_secs(n * 60)),
        "S" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_millis(n)),
        "u" => Some(Duration::from_micros(n)),
        "n" => Some(Duration::from_nanos(n)),
        _ => None,
    }
}

/// The successor's answer to the call, or DEADLINE_EXCEEDED when the caller's `deadline`
/// passes first (the successor has not started the call: nothing is held for it).
async fn opened<R>(
    deadline: Option<Duration>,
    opening: impl Future<Output = Result<R, Status>>,
) -> Result<R, Status> {
    match deadline {
        Some(limit) => tokio::time::timeout(limit, opening).await.map_err(|_| {
            Status::deadline_exceeded("the successor did not start the call in time")
        })?,
        None => opening.await,
    }
}

/// The operation a relay can follow in the state store.
struct Tracked {
    name: String,
    action: Digest,
    operation: OperationId,
}

/// Where a relayed stream gets its operations.
enum Leg {
    /// The successor's stream.
    Live(Streaming<Operation>),
    /// The `done/` rows, after the successor's stream broke.
    Follow(Tracked),
}

/// One relayed stream, as [`Relay::relayed`] starts it.
struct Relayed {
    leg: Leg,
    /// The instance the call named: only an Execute knows it.
    instance: Option<String>,
    /// Set by the first operation the successor sent, if its action has an unfinished
    /// row in the state store.
    tracked: Option<Tracked>,
    /// The stage last sent, from which a followed stream changes.
    shown: Stage,
    farm: Arc<dyn Handed>,
    closing: Closing,
    progress: Progress,
    every: Duration,
    _open: Open,
}

fn shutting_down() -> Status {
    Status::unavailable("the server is shutting down; the operation ends here, Execute it again")
}

impl Relayed {
    /// The next item of the stream, and the stream if it goes on.
    async fn next(mut self) -> Option<(Result<Operation, Status>, Option<Self>)> {
        loop {
            match &mut self.leg {
                Leg::Live(live) => {
                    let got = tokio::select! {
                        biased;
                        () = self.closing.closed() => return Some((Err(shutting_down()), None)),
                        got = live.message() => got,
                    };
                    match got {
                        Ok(Some(operation)) => {
                            self.see(&operation);
                            let done = operation.done;
                            return Some((Ok(operation), (!done).then_some(self)));
                        }
                        broken => match self.tracked.take() {
                            Some(tracked) => self.leg = Leg::Follow(tracked),
                            None => return Some((Err(unfollowable(broken)), None)),
                        },
                    }
                }
                Leg::Follow(tracked) => {
                    let resend = tokio::select! {
                        biased;
                        () = self.closing.closed() => return Some((Err(shutting_down()), None)),
                        () = self.progress.tick() => true,
                        () = tokio::time::sleep(self.every) => false,
                    };
                    let stage = match self.farm.look(tracked.operation) {
                        Ok(stage) => stage.filter(|stage| *stage != self.shown),
                        Err(why) => {
                            let name = &tracked.name;
                            let why = format!("operation {name} could not be followed: {why}");
                            return Some((Err(Status::unavailable(why)), None));
                        }
                    };
                    let changed = stage.is_some();
                    if let Some(stage) = stage {
                        self.shown = stage;
                    }
                    if changed || resend {
                        let item = operation(&tracked.name, &tracked.action, &self.shown, false);
                        let done = matches!(self.shown, Stage::Done(_));
                        return Some((Ok(item), (!done).then_some(self)));
                    }
                }
            }
        }
    }

    /// Takes note of an operation the successor sent: its stage, and, once, the
    /// unfinished operation of the state store it is (found while it is unfinished).
    fn see(&mut self, operation: &Operation) {
        let Some(metadata) = operation
            .metadata
            .as_ref()
            .and_then(|any| ExecuteOperationMetadata::decode(any.value.as_slice()).ok())
        else {
            return;
        };
        let executing = metadata.stage == execution_stage::Value::Executing as i32;
        self.shown = if executing {
            Stage::Executing
        } else {
            Stage::Queued
        };
        if self.tracked.is_some() || operation.done {
            return;
        }
        let Ok(action) = wire::digest(metadata.action_digest.as_ref()) else {
            return;
        };
        match self.farm.operation_of(self.instance.as_deref(), &action) {
            Ok(Some(found)) => {
                self.tracked = Some(Tracked {
                    name: operation.name.clone(),
                    action,
                    operation: found,
                });
            }
            Ok(None) => tracing::debug!(%action, "relayed operation not in the state store"),
            Err(error) => tracing::warn!(%action, %error, "relayed operation not looked up"),
        }
    }
}

/// What a stream that broke with nothing to follow ends with: the status it broke with,
/// or UNAVAILABLE if it only ended.
fn unfollowable(broken: Result<Option<Operation>, Status>) -> Status {
    broken.err().unwrap_or_else(|| {
        Status::unavailable("the successor ended the stream before the operation was done")
    })
}

#[cfg(test)]
mod tests {
    use tonic::metadata::MetadataValue;

    use super::*;

    fn timeout(value: &str) -> MetadataMap {
        let mut metadata = MetadataMap::new();
        metadata.insert(
            "grpc-timeout",
            MetadataValue::try_from(value).expect("ascii"),
        );
        metadata
    }

    /// Catches: a deadline read in the wrong unit (a call that is cut a thousand times too
    /// early or late), a header the wire format does not write taken for one, and a call
    /// with no deadline given one.
    #[test]
    fn the_deadline_is_the_grpc_timeout_header() {
        for (value, want) in [
            ("2H", Duration::from_secs(7200)),
            ("3M", Duration::from_secs(180)),
            ("30S", Duration::from_secs(30)),
            ("250m", Duration::from_millis(250)),
            ("7u", Duration::from_micros(7)),
            ("9n", Duration::from_nanos(9)),
            ("99999999S", Duration::from_secs(99_999_999)),
        ] {
            assert_eq!(deadline(&timeout(value)), Some(want), "{value}");
        }
        for value in ["", "S", "12", "1x", "-1S", "123456789S", "1.5S", "5 S"] {
            assert_eq!(deadline(&timeout(value)), None, "{value:?}");
        }
        assert_eq!(deadline(&MetadataMap::new()), None);
    }

    /// Catches: the transport's own headers forwarded (a `content-type` or `te` of the
    /// first call on the second), the deadline forwarded too (the client would then cut the
    /// call with its own status as the relay's timer fires), and anything else dropped.
    #[test]
    fn a_forwarded_call_keeps_the_callers_metadata_but_not_the_transports() {
        let mut request = Request::new(7_u8);
        for (key, value) in [
            ("te", "trailers"),
            ("content-type", "application/grpc"),
            ("grpc-encoding", "gzip"),
            ("authorization", "Bearer t"),
            ("grpc-timeout", "5S"),
        ] {
            request
                .metadata_mut()
                .insert(key, MetadataValue::try_from(value).expect("ascii"));
        }
        request
            .metadata_mut()
            .insert_bin("x-blob-bin", MetadataValue::from_bytes(&[0, 1, 2]));
        let (call, limit) = forwarded(request);
        assert_eq!(*call.get_ref(), 7);
        assert_eq!(limit, Some(Duration::from_secs(5)));
        let headers = call.metadata().clone().into_headers();
        let mut keys: Vec<_> = headers.keys().map(|key| key.as_str().to_owned()).collect();
        keys.sort();
        assert_eq!(keys, ["authorization", "x-blob-bin"]);
    }
}

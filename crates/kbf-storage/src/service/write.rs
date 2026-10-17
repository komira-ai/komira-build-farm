//! Taking a `Write` into the store: streamed where the store can, forwarded to the
//! replica by the primary of a chain, whole in memory only where nothing else is
//! possible.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use kbf_map::MemberId;
use kbf_proto::google::bytestream::WriteRequest;
use kbf_types::Digest;
use tonic::Status;

use super::{STREAM_FROM_BYTES, StorageService};
use crate::replicate::{Plan, ReplicaLeg, Staging, View};
use crate::{PutStream, StorageError, StreamedPut};

/// What a streamed write takes from [`super::Config::write_bytes_in_flight`]: room for the
/// few chunks it holds (the writer's queue, the replica's, and the one in hand), not for
/// the blob.
const STREAM_WINDOW: u64 = 4 << 20;

/// The messages of a `Write` after the first: a call's request stream, or the chunks of a
/// blob an in-process caller holds ([`StorageService::in_process`]). One type for both so
/// a write is taken in by one body of code. Owned, so that a blob too large to hold can be
/// handed on to [`crate::Storage::put_streamed`] as it arrives.
pub(super) type Messages = BoxStream<'static, Result<WriteRequest, Status>>;

impl StorageService {
    /// The map a write routed by generation `requested` is judged by: `None` for a node
    /// outside a chain.
    ///
    /// # Errors
    /// STALE_MAP, or UNAVAILABLE for a caller ahead of a node that could not fetch the map
    /// in time ([`crate::replicate`]); UNAVAILABLE for a chain node with no map yet, or with a map
    /// that has not registered and so does not know which member it is.
    pub(super) async fn view(&self, requested: u64) -> Result<Option<View>, StorageError> {
        match &self.chain {
            Some(chain) => chain.view(requested, self.refetch_wait).await.map(Some),
            None => self.check_map(requested).await.map(|()| None),
        }
    }

    /// A whole `Write` once its caller is admitted: judges it by map generation `requested`,
    /// takes the first message and then the rest from `messages`, and stores the blob as
    /// this node's [`Plan`] says (forwarded to the replica when this node is the primary of
    /// a chain). The answer is the blob's size. The one body of code a `Write` call and an
    /// in-process write ([`StorageService::in_process`]) both run.
    ///
    /// # Errors
    /// What the checks and the store refuse, as the call's status.
    pub(super) async fn write_in(
        &self,
        requested: u64,
        leg_from: Option<MemberId>,
        unowned: bool,
        mut messages: Messages,
    ) -> Result<u64, Status> {
        let view = self.view(requested).await?;
        let first = next_or(&mut messages, "write stream sent no message").await?;
        let digest = super::wire::write_resource(&first.resource_name)?;
        let size = digest.size_bytes;
        let (plan, view) = match self.plan(view.as_ref(), &digest, leg_from) {
            // A copy the caller needs here though the map places the blob elsewhere: one
            // copy, outside the chain (no replica, no debt, no placement to confirm).
            Err(StorageError::Invalid(_)) if unowned => (Plan::Local, None),
            planned => (planned?, view),
        };
        if self.storage.find_missing(&[digest]).await?.is_empty() {
            return Ok(size);
        }
        self.write_blob(digest, first, messages, view.as_ref(), leg_from, plan)
            .await?;
        Ok(size)
    }

    /// What this node does with a write of `digest` under `view`.
    pub(super) fn plan(
        &self,
        view: Option<&View>,
        digest: &Digest,
        leg_from: Option<MemberId>,
    ) -> Result<Plan, StorageError> {
        match (&self.chain, view) {
            (Some(chain), Some(view)) => chain.plan(view, digest, leg_from),
            _ if leg_from.is_some() => Err(StorageError::Invalid(
                "a copy was sent to a node that is in no chain".into(),
            )),
            _ => Ok(Plan::Local),
        }
    }

    /// Stores the blob `digest`, of which `first` is the first message and `rest` the
    /// others, as `plan` says.
    pub(super) async fn write_blob(
        &self,
        digest: Digest,
        first: WriteRequest,
        rest: Messages,
        view: Option<&View>,
        leg_from: Option<MemberId>,
        plan: Plan,
    ) -> Result<(), Status> {
        let size = digest.size_bytes;
        let put = self.open_stream(size).await?;
        let (put, leg) = match (&plan, put, &self.chain, view) {
            (Plan::Primary { replica }, Some(put), Some(chain), Some(view)) => {
                (Some(put), Some(chain.open_leg(view, replica, size)?))
            }
            (Plan::Primary { .. }, None, _, _) => {
                return Err(Status::failed_precondition(
                    "this node's store cannot write a blob as it arrives, and a chain's primary must \
                     pass the blob on without holding it whole",
                ));
            }
            (_, put, _, _) => (put, None),
        };
        let Some(put) = put else {
            return self.write_buffered(digest, first, rest).await;
        };
        let room = size.min(STREAM_WINDOW);
        let room =
            u32::try_from(room).map_or(self.write_room_bytes, |r| r.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
        let (staged, lost) = receive(digest, put, first, rest, leg).await?;
        let mut owing = None;
        if let (Some(chain), Some(view)) = (&self.chain, view) {
            chain.confirm(view, &digest, leg_from, &plan)?;
            // The only way past a lost replica: the copy owed is on the disk before the
            // blob is present here, so a blob present here is replicated or owed. Until the
            // commit is over the debt is for a blob that may not be here yet, which the
            // repair worker must not mistake for an evicted one.
            if let (Plan::Primary { replica }, Some(why)) = (&plan, lost) {
                owing = Some(chain.degrade(view, replica, &digest, &why).await?);
            }
        }
        let committed = staged.commit().await;
        drop(owing);
        committed?;
        Ok(())
    }

    /// The store's way to write a blob of `size` bytes as it arrives, if it has one.
    async fn open_stream(&self, size: u64) -> Result<Option<Box<dyn PutStream>>, StorageError> {
        let storage = std::sync::Arc::clone(&self.storage);
        tokio::task::spawn_blocking(move || storage.stream(size))
            .await
            .map_err(|e| StorageError::Internal(format!("opening a blob stream: {e}")))
    }

    /// For a store that cannot write as bytes arrive, outside a chain: holds a blob of at
    /// most [`STREAM_FROM_BYTES`] whole until it is complete, then stores it with
    /// [`crate::Storage::put`], which hashes it; a larger one is never held, its messages
    /// go to [`crate::Storage::put_streamed`] as they arrive, which hashes them on the way
    /// and stores nothing unless they hash to the digest. Room for what is held (at most
    /// [`STREAM_FROM_BYTES`] of it) is taken first. Order is not checked message by
    /// message: the blob is checked against its digest as a whole.
    async fn write_buffered(
        &self,
        digest: Digest,
        first: WriteRequest,
        mut rest: Messages,
    ) -> Result<(), Status> {
        let size = digest.size_bytes;
        let held = size.min(STREAM_FROM_BYTES);
        let room =
            u32::try_from(held).map_or(self.write_room_bytes, |s| s.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
        if size > STREAM_FROM_BYTES {
            let fault = Arc::new(Mutex::new(None));
            let body = pieces(rest, first, size, Arc::clone(&fault));
            let claim = Box::pin(futures::future::ready(Some(digest)));
            let stored = self.storage.put_streamed(size, body, claim).await;
            // A fault of the sender's is INVALID_ARGUMENT, not the store's.
            if let Some(status) = fault.lock().unwrap_or_else(PoisonError::into_inner).take() {
                return Err(status);
            }
            stored?;
            return Ok(());
        }
        let mut received = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        let mut message = first;
        loop {
            if (received.len() + message.data.len()) as u64 > size {
                return Err(more_than(&digest));
            }
            received.extend_from_slice(&message.data);
            if message.finish_write {
                break;
            }
            message = next(&mut rest).await?;
        }
        let stored = self.storage.put(received.into()).await?;
        if stored != digest {
            return Err(StorageError::DigestMismatch {
                claimed: digest,
                actual: stored,
            }
            .into());
        }
        Ok(())
    }
}

fn more_than(digest: &Digest) -> Status {
    Status::invalid_argument(format!(
        "more than the {} bytes {digest} names",
        digest.size_bytes
    ))
}

/// The next message of a write that has not finished.
async fn next(stream: &mut Messages) -> Result<WriteRequest, Status> {
    next_or(stream, "write stream ended without finish_write").await
}

/// The next message, or INVALID_ARGUMENT `missing` if the stream has ended.
async fn next_or(stream: &mut Messages, missing: &'static str) -> Result<WriteRequest, Status> {
    stream
        .next()
        .await
        .transpose()?
        .ok_or_else(|| Status::invalid_argument(missing))
}

/// Streams the blob `digest` into `put`, and on to `leg` if there is one, a message at
/// a time. The blob is checked against its digest as it passes, and the replica's
/// answer awaited: the result is a blob written and verified here, not yet present on this
/// node ([`StreamedPut::commit`]), and why the replica could not be reached if it
/// could not (it holds the blob if that is `None`).
async fn receive(
    digest: Digest,
    put: Box<dyn PutStream>,
    first: WriteRequest,
    mut rest: Messages,
    mut leg: Option<ReplicaLeg>,
) -> Result<(Box<dyn StreamedPut>, Option<StorageError>), Status> {
    let mut staging = Staging::start(put, digest);
    let mut received = 0;
    let mut message = first;
    loop {
        received += message.data.len() as u64;
        if received > digest.size_bytes {
            return Err(more_than(&digest));
        }
        let finish = message.finish_write;
        let data = Bytes::from(std::mem::take(&mut message.data));
        staging.push(data.clone()).await?;
        if let Some(leg) = &mut leg {
            leg.send(WriteRequest {
                data: data.to_vec(),
                ..message
            })
            .await?;
        }
        if finish {
            break;
        }
        message = next(&mut rest).await?;
    }
    let (staged, replica) = tokio::join!(staging.finish(), async {
        match leg {
            Some(leg) => leg.finish().await,
            None => Ok(None),
        }
    });
    Ok((staged?, replica?))
}

/// The bytes of a `Write` of a blob of `size` bytes, message by message as they arrive,
/// starting with `first`: the stream [`crate::Storage::put_streamed`] takes. A message
/// stream that is wrong (more bytes than `size`, fewer at `finish_write`, ended without
/// it, or broken) fails the stream with the sender's fault noted in `fault`.
fn pieces(
    stream: Messages,
    first: WriteRequest,
    size: u64,
    fault: Arc<Mutex<Option<Status>>>,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    futures::stream::unfold(
        Incoming {
            stream,
            first: Some(first),
            got: 0,
            size,
            done: false,
            fault,
        },
        Incoming::step,
    )
    .boxed()
}

/// The state of [`pieces`].
struct Incoming {
    stream: Messages,
    first: Option<WriteRequest>,
    got: u64,
    size: u64,
    done: bool,
    fault: Arc<Mutex<Option<Status>>>,
}

impl Incoming {
    /// The next message's bytes, or `None` after the last.
    async fn step(mut self) -> Option<(std::io::Result<Bytes>, Self)> {
        loop {
            if self.done {
                return None;
            }
            let message = match self.first.take() {
                Some(message) => message,
                None => match self.stream.next().await {
                    Some(Ok(message)) => message,
                    None => {
                        let status =
                            Status::invalid_argument("write stream ended without finish_write");
                        return Some(self.refuse(status));
                    }
                    Some(Err(status)) => return Some(self.refuse(status)),
                },
            };
            self.got += message.data.len() as u64;
            if self.got > self.size {
                let status = Status::invalid_argument(format!(
                    "more than the {} bytes the resource names",
                    self.size
                ));
                return Some(self.refuse(status));
            }
            if message.finish_write {
                self.done = true;
                if self.got < self.size {
                    let status = Status::invalid_argument(format!(
                        "{} bytes, fewer than the {} the resource names",
                        self.got, self.size
                    ));
                    return Some(self.refuse(status));
                }
            }
            if !message.data.is_empty() {
                return Some((Ok(Bytes::from(message.data)), self));
            }
        }
    }

    /// Ends the stream in an error, noting `status` as the sender's fault.
    fn refuse(mut self, status: Status) -> (std::io::Result<Bytes>, Self) {
        let error = std::io::Error::other(status.message().to_owned());
        *self.fault.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
        self.done = true;
        (Err(error), self)
    }
}

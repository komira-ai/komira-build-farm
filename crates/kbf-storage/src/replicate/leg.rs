//! The call from a primary to its replica: the same ByteStream `Write` a client makes,
//! fed message by message as the primary receives them, so the blob passes through and
//! is never held whole.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::MemberId;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{WriteRequest, WriteResponse};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use crate::StorageError;
use crate::service::{MAP_GENERATION_HEADER, MAX_MESSAGE_BYTES, lift};

/// The metadata entry that marks a `Write` as a copy another storage node sends: its
/// value is the sender's member id. The receiver stores the blob and forwards nothing.
pub const LEG_HEADER: &str = "kbf-chain-leg";

/// Messages the call holds ahead of the replica: with the one the sender is putting
/// down, the most a leg holds of a blob, whatever its size.
const QUEUE: usize = 2;

/// A replica that did not take a message, or did not commit, in time.
#[derive(Debug, thiserror::Error)]
#[error("the replica did not {what} within {after:?}")]
struct Stalled {
    what: &'static str,
    after: Duration,
}

/// The call to the replica, ended if the leg is dropped.
struct Call(JoinHandle<Result<Response<WriteResponse>, Status>>);

impl Drop for Call {
    /// A write abandoned (the client left, or another part of it failed) ends the call to
    /// the replica, whose half-received blob is dropped uncommitted.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One blob on its way to the replica.
pub(crate) struct Leg {
    tx: mpsc::Sender<WriteRequest>,
    call: Call,
    /// The blob's size: an answer for this many bytes is the replica's copy.
    size: u64,
    /// The replica answered, before the blob was complete, that it holds all of it
    /// already: nothing more is sent.
    held: bool,
    stall: Duration,
    commit: Duration,
}

impl Leg {
    /// Starts the `Write` of a blob of `size` bytes to `channel`, tagged with the map
    /// generation the sender routed by and its own member id.
    pub(crate) fn open(
        channel: Channel,
        generation: u64,
        sender: MemberId,
        size: u64,
        stall: Duration,
        commit: Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let call = tokio::spawn(async move {
            let mut request = Request::new(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)));
            let metadata = request.metadata_mut();
            metadata.insert(MAP_GENERATION_HEADER, MetadataValue::from(generation));
            metadata.insert(LEG_HEADER, MetadataValue::from(sender.0));
            ByteStreamClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES)
                .write(request)
                .await
        });
        Self {
            tx,
            call: Call(call),
            size,
            held: false,
            stall,
            commit,
        }
    }

    /// Hands the next message to the call, waiting while the replica is behind. A replica
    /// that has answered that it holds the whole blob already is sent nothing more.
    ///
    /// # Errors
    /// Why the call ended, if it did (the replica refused or could not be reached, or
    /// answered for other than the blob's size); [`StorageError::Unavailable`] if it took
    /// no message for the stall bound.
    pub(crate) async fn send(&mut self, message: WriteRequest) -> Result<(), StorageError> {
        if self.held {
            return Ok(());
        }
        match tokio::time::timeout(self.stall, self.tx.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => self.ended().await,
            Err(_) => Err(StorageError::Unavailable(Arc::new(Stalled {
                what: "take a message",
                after: self.stall,
            }))),
        }
    }

    /// Ends the request stream and waits for the replica's answer: it has verified the
    /// blob against its digest and made it durable, or this says why not.
    ///
    /// # Errors
    /// Why the call failed; [`StorageError::Unavailable`] if the replica did not commit
    /// for the commit bound; [`StorageError::Internal`] if it acknowledged other than
    /// `size` bytes.
    pub(crate) async fn finish(self) -> Result<(), StorageError> {
        let Self {
            tx,
            mut call,
            size,
            held,
            commit,
            ..
        } = self;
        drop(tx);
        if held {
            return Ok(());
        }
        let answer = tokio::time::timeout(commit, &mut call.0)
            .await
            .map_err(|_| {
                StorageError::Unavailable(Arc::new(Stalled {
                    what: "commit",
                    after: commit,
                }))
            })?;
        match answer {
            Ok(Ok(response)) if u64::try_from(response.get_ref().committed_size) == Ok(size) => {
                Ok(())
            }
            Ok(Err(status)) => Err(lift(&status, None)),
            // An acknowledgement of other than the blob's size, or a call that panicked.
            other => Err(StorageError::Internal(format!(
                "the replica's answer to a {size}-byte write: {:?}",
                other.map(|r| r.map(|response| response.into_inner().committed_size))
            ))),
        }
    }

    /// What a call that ended before the blob was complete means: nothing to fear if the
    /// replica answered that it holds the whole blob (it had it already; the leg is done and
    /// the sender goes on staging its own copy), otherwise why the call failed.
    async fn ended(&mut self) -> Result<(), StorageError> {
        match (&mut self.call.0).await {
            Ok(Ok(response))
                if u64::try_from(response.get_ref().committed_size) == Ok(self.size) =>
            {
                self.held = true;
                Ok(())
            }
            Ok(Err(status)) => Err(lift(&status, None)),
            other => Err(StorageError::Internal(format!(
                "the replica answered before the blob was complete: {:?}",
                other.map(|r| r.map(|response| response.into_inner().committed_size))
            ))),
        }
    }
}

/// The replica's part in one write: a leg that may be lost without the write being.
///
/// A leg is lost when the replica cannot be reached (including a stall, and a replica the
/// map names down): the write goes on to the primary's own store, and whoever commits it
/// owes the copy ([`crate::replicate::Chain::degrade`]). A replica that answers with a
/// refusal is not lost; its refusal is the write's.
pub(crate) struct ReplicaLeg {
    leg: Option<Leg>,
    lost: Option<StorageError>,
}

/// Whether `error` says the replica could not be reached, rather than that it refused.
fn not_reached(error: &StorageError) -> bool {
    matches!(error, StorageError::Unavailable(_))
}

impl ReplicaLeg {
    pub(crate) fn open(
        channel: Channel,
        generation: u64,
        sender: MemberId,
        size: u64,
        stall: Duration,
        commit: Duration,
    ) -> Self {
        Self {
            leg: Some(Leg::open(channel, generation, sender, size, stall, commit)),
            lost: None,
        }
    }

    /// A replica that is not called: lost from the start, for `why`.
    pub(crate) fn lost(why: StorageError) -> Self {
        Self {
            leg: None,
            lost: Some(why),
        }
    }

    /// Hands the next message on, unless the replica is lost.
    ///
    /// # Errors
    /// The replica's refusal.
    pub(crate) async fn send(&mut self, message: WriteRequest) -> Result<(), StorageError> {
        let Some(leg) = &mut self.leg else {
            return Ok(());
        };
        match leg.send(message).await {
            Err(why) if not_reached(&why) => {
                self.leg = None;
                self.lost = Some(why);
                Ok(())
            }
            other => other,
        }
    }

    /// Ends the call and waits for the replica: `None` if it holds the blob, `Some` why it
    /// could not be reached.
    ///
    /// # Errors
    /// The replica's refusal.
    pub(crate) async fn finish(self) -> Result<Option<StorageError>, StorageError> {
        match self.leg {
            None => Ok(self.lost),
            Some(leg) => match leg.finish().await {
                Ok(()) => Ok(None),
                Err(why) if not_reached(&why) => Ok(Some(why)),
                Err(refusal) => Err(refusal),
            },
        }
    }
}

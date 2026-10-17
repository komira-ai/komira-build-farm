//! Fetching one offer into the inbox: every component through a [`Source`], counted
//! against the offered size and hashed as it is written, checked against the offered
//! digest before the directory becomes visible.
//!
//! The checks here stop a frontend that misbehaves from filling the disk or from leaving
//! a half release where the helper reads. They are not trust: the helper verifies the
//! build attestation of whatever is in the inbox.

use std::io;
use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_threads::disk::FreeSpace;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::inbox::{Inbox, Staging, tarball_name};
use crate::source::{FetchError, Source};
use crate::types::{Component, FREE_DISK_FACTOR, Offer};

/// How long a fetch waits before it tries again: `first`, doubling up to `max`.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

/// Why a fetch stopped short of a visible directory.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The offer cannot be completed, and why. Trying again would not change that.
    Refused(String),
    /// A newer offer arrived.
    Abandoned,
}

/// What a fetch needs from the node.
pub(crate) struct Job<'a> {
    pub source: &'a dyn Source,
    pub inbox: &'a Inbox,
    pub space: &'a (dyn FreeSpace + Send + Sync),
    /// The update directory, whose filesystem must have room.
    pub dir: &'a Path,
    pub backoff: Backoff,
}

fn refused(what: &str, e: &io::Error) -> Stop {
    Stop::Refused(format!("{what}: {e}"))
}

/// Brings `offer` into the inbox, unless it is there. Ends early when `abandoned`
/// completes, leaving nothing behind.
pub(crate) async fn stage(
    job: &Job<'_>,
    offer: &Offer,
    abandoned: BoxFuture<'_, ()>,
) -> Result<(), Stop> {
    if job.inbox.has(offer) {
        return Ok(());
    }
    let free = job
        .space
        .space(job.dir)
        .map_err(|e| refused("cannot tell how much disk is free", &e))?
        .available;
    let needed = offer.total_size().saturating_mul(FREE_DISK_FACTOR);
    if free < needed {
        return Err(Stop::Refused(format!(
            "{free} bytes of disk are free; fetching {} bytes needs {FREE_DISK_FACTOR} times that, \
             {needed}",
            offer.total_size()
        )));
    }
    let staging = job
        .inbox
        .begin(offer)
        .map_err(|e| refused("cannot start a directory in the inbox", &e))?;
    // Biased: a fetch that is superseded makes no more progress, whatever else is ready.
    let filled = tokio::select! {
        biased;
        () = abandoned => Err(Stop::Abandoned),
        done = fill(job, offer, &staging) => done,
    };
    let result = filled.and_then(|()| {
        staging
            .finish(offer)
            .map_err(|e| refused("cannot complete the inbox directory", &e))
    });
    if result.is_err() {
        staging.discard();
    }
    result
}

async fn fill(job: &Job<'_>, offer: &Offer, staging: &Staging) -> Result<(), Stop> {
    for component in &offer.components {
        let file = tarball_name(offer, component);
        let mut delay = job.backoff.first;
        loop {
            match attempt(job.source, staging, component, &file).await {
                Ok(()) => break,
                Err(Attempt::Refused(why)) => return Err(Stop::Refused(why)),
                Err(Attempt::Retry(why)) => {
                    tracing::warn!(component = %component.name, %why, ?delay, "fetch failed; trying again");
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(job.backoff.max);
                }
            }
        }
    }
    Ok(())
}

/// How one try at a component ended, unless it succeeded.
enum Attempt {
    /// A failure of the connection or the service.
    Retry(String),
    /// A failure no new try fixes.
    Refused(String),
}

impl From<FetchError> for Attempt {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Transport(why) => Self::Retry(why),
            FetchError::Missing(_) => Self::Refused(e.to_string()),
        }
    }
}

impl From<io::Error> for Attempt {
    fn from(e: io::Error) -> Self {
        Self::Refused(format!("the inbox cannot be written: {e}"))
    }
}

/// One try: the blob into `file`, from its first byte.
async fn attempt(
    source: &dyn Source,
    staging: &Staging,
    component: &Component,
    file: &str,
) -> Result<(), Attempt> {
    let mut chunks = source.fetch(&component.sha256).await?;
    let mut out = staging.create(file).await?;
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        count = count.saturating_add(chunk.len() as u64);
        // Not one byte more than offered is written, or read once it is seen.
        if count > component.size {
            return Err(Attempt::Refused(format!(
                "{}: the frontend served more than the {} bytes offered",
                component.name, component.size
            )));
        }
        hasher.update(&chunk);
        out.write_all(&chunk).await?;
    }
    if count != component.size {
        return Err(Attempt::Refused(format!(
            "{}: the frontend served {count} of the {} bytes offered",
            component.name, component.size
        )));
    }
    let digest = hex::encode(hasher.finalize());
    if digest != component.sha256 {
        return Err(Attempt::Refused(format!(
            "{}: the bytes served hash to {digest}, not the {} offered",
            component.name, component.sha256
        )));
    }
    out.sync_all().await?;
    Ok(())
}

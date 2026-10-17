//! The frontend that is serving, when another asks to take over (see [`super`]).

use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::AbortHandle;
use kbf_state::{GENERATION, Store, StoreError};
use tonic::Status;

use super::wire::{Link, Message, PROTOCOL};
use super::{HandoverError, Sockets, Stop};
use crate::drain::Drain;
use crate::farm::Farm;

/// How often a holder looks again for the work that keeps it from standing down.
const QUIET_LOOK: Duration = Duration::from_millis(10);

/// What the holder tells a daemon whose stream it ends.
const STANDING_DOWN: &str = "this frontend handed its work to a new process; connect again";

/// What a holder serves, which a successor must be started with to take it over.
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    pub reapi: SocketAddr,
    pub worker: SocketAddr,
    pub api: Option<SocketAddr>,
    pub state: PathBuf,
}

/// Everything a holder stops to let go, in the order it stops it.
pub(crate) struct Stand {
    pub drain: Arc<Drain>,
    pub farm: Arc<Farm>,
    /// Stops the worker listener.
    pub worker: Stop,
    /// Stops the operator API listener.
    pub api: Stop,
    /// Ends the task that expires the state store's rows.
    pub sweeper: AbortHandle,
    pub store: Arc<dyn Store>,
    /// The longest to wait for the farm to be [quiet](Farm::quiet).
    pub wait: Duration,
}

impl Stand {
    /// Stands down: see the module docs of [`super`], step 3.
    ///
    /// # Errors
    /// The state store cannot give up its writer role. The farm is fenced and its
    /// listeners are stopped by then, so the drain is closed: the process only has its
    /// open streams to end.
    pub async fn run(&self) -> Result<(), StoreError> {
        tracing::info!("standing down for a successor");
        self.drain.begin();
        let _ = tokio::time::timeout(self.wait, self.until_quiet()).await;
        self.farm.fence();
        self.sweeper.abort();
        self.worker.fire();
        self.farm
            .end_worker_streams(&Status::unavailable(STANDING_DOWN));
        self.api.fire();
        let demoted = self.store.demote();
        if demoted.is_err() {
            self.drain.close();
        }
        demoted
    }

    async fn until_quiet(&self) {
        while !self.farm.quiet() {
            tokio::time::sleep(QUIET_LOOK).await;
        }
    }
}

/// The control socket of a serving frontend, and what it needs to hand over.
pub(crate) struct Holder {
    listener: tokio::net::UnixListener,
    /// The only user whose processes may ask (the one this runs as).
    uid: u32,
    facts: Facts,
    /// Second handles on the three listeners: what an offer carries.
    sockets: Sockets,
    stand: Stand,
}

impl Holder {
    /// Binds the control socket at `path`, mode 0600, replacing a socket file left by a
    /// frontend that died (the caller holds the state store, so none is serving on it).
    ///
    /// # Errors
    /// The socket cannot be made.
    pub fn bind(
        path: &Path,
        uid: u32,
        facts: Facts,
        sockets: Sockets,
        stand: Stand,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: tokio::net::UnixListener::from_std(listener)?,
            uid,
            facts,
            sockets,
            stand,
        })
    }

    /// Serves the control socket until a successor has taken over, then answers the
    /// callers whose streams it kept open (`every` is how often it looks for their
    /// results). A failure is logged: the frontend goes on serving, unable to hand over.
    pub async fn run(self, farm: Arc<Farm>, every: Duration) {
        let handed_over = self.hand_over().await;
        // The handles on the listeners go, and with them the control socket: the
        // successor's are the only ones left, and a successor that dies can be started
        // again on the same addresses.
        drop(self);
        match handed_over {
            Ok(()) => follow(&farm, every).await,
            Err(error) => tracing::error!(%error, "the handover control socket failed"),
        }
    }

    /// Takes connections one at a time until one of them takes over.
    ///
    /// # Errors
    /// The control socket fails, or the state store cannot be given up.
    async fn hand_over(&self) -> Result<(), HandoverError> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let trusted = stream.peer_cred().is_ok_and(|peer| peer.uid() == self.uid);
            if !trusted {
                tracing::warn!("a handover was asked for by another user: refused");
                continue;
            }
            let mut link = Link::new(stream);
            let ready = self.negotiate(&mut link).await;
            if matches!(ready, Ok(true)) {
                return self.release(link).await;
            }
            tracing::info!(?ready, "a handover was asked for and did not happen");
        }
    }

    /// The conversation up to `ready`: whether the successor is ready to take over. Until
    /// it is, nothing about this frontend has changed.
    async fn negotiate(&self, link: &mut Link) -> Result<bool, HandoverError> {
        let hello = link.hear().await?;
        if let Some(why) = self.refusal(&hello) {
            link.say(&Message::Refused(why)).await?;
            return Ok(false);
        }
        let offer = Message::Offer {
            generation: self.generation()?,
            api: self.sockets.api.is_some(),
        };
        let api = self.sockets.api.as_ref().map(AsFd::as_fd);
        let sockets: Vec<BorrowedFd<'_>> = [
            Some(self.sockets.reapi.as_fd()),
            Some(self.sockets.worker.as_fd()),
            api,
        ]
        .into_iter()
        .flatten()
        .collect();
        link.say_with(&offer, &sockets).await?;
        Ok(link.hear().await? == Message::Ready)
    }

    /// Why this frontend will not hand over to the process that sent `hello`, if it will
    /// not.
    fn refusal(&self, hello: &Message) -> Option<String> {
        let Message::Hello {
            version,
            reapi,
            worker,
            api,
            state,
        } = hello
        else {
            return Some("the first message must be hello".to_owned());
        };
        let facts = &self.facts;
        [
            (
                *version != PROTOCOL,
                format!("this frontend speaks handover protocol {PROTOCOL}, not {version}"),
            ),
            (
                (*reapi, *worker, *api) != (facts.reapi, facts.worker, facts.api),
                "the successor's listen flags are not this frontend's: a handover keeps the \
                 listeners, so they must be the same"
                    .to_owned(),
            ),
            (
                *state != facts.state,
                "the successor's --state-store is not this frontend's".to_owned(),
            ),
        ]
        .into_iter()
        .find_map(|(refused, why)| refused.then_some(why))
    }

    /// The generation the state store is at, which the successor must find.
    fn generation(&self) -> Result<u64, StoreError> {
        let row = self.stand.store.get(GENERATION)?;
        let bytes = row.and_then(|row| <[u8; 8]>::try_from(row.value.as_slice()).ok());
        Ok(bytes.map_or(0, u64::from_be_bytes))
    }

    /// The successor is ready: stands down, and says so. From here this frontend does not
    /// go back, whatever the successor does.
    async fn release(&self, mut link: Link) -> Result<(), HandoverError> {
        let stood = self.stand.run().await;
        tracing::info!(stood = stood.is_ok(), "handed over to the successor");
        let said = match &stood {
            Ok(()) => Message::Released,
            Err(error) => Message::Failed(error.to_string()),
        };
        // The successor may be gone already; the rows are what it leaves behind.
        let _ = link.say(&said).await;
        Ok(stood?)
    }
}

/// Answers the callers this frontend kept open from the state store, every `every`, until
/// none is left waiting.
pub(crate) async fn follow(farm: &Farm, every: Duration) {
    let mut look = tokio::time::interval(every);
    loop {
        look.tick().await;
        match farm.follow() {
            Ok(0) => return,
            Ok(_waiting) => {}
            Err(error) => tracing::warn!(%error, "the successor's results could not be read"),
        }
    }
}

#[cfg(test)]
mod tests;

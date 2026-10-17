//! The frontend that starts while another serves, when it asks to take over (see [`super`]).

use std::os::fd::OwnedFd;
use std::path::Path;

use tokio::net::UnixStream;

use super::wire::{Link, Message};
use super::{HandoverError, Sockets};

impl Sockets {
    /// The sockets of an offer: REAPI, worker, and the operator API if `api`.
    fn of(sockets: Vec<OwnedFd>, api: bool) -> Result<Self, HandoverError> {
        let got = sockets.len();
        let mut sockets = sockets.into_iter();
        let (reapi, worker, operator, more) = (
            sockets.next(),
            sockets.next(),
            sockets.next(),
            sockets.next(),
        );
        match (reapi, worker, operator, more, api) {
            (Some(reapi), Some(worker), None, None, false) => Ok(Self {
                reapi,
                worker,
                api: None,
            }),
            (Some(reapi), Some(worker), Some(api), None, true) => Ok(Self {
                reapi,
                worker,
                api: Some(api),
            }),
            _ => Err(HandoverError::Sockets {
                got,
                want: 2 + usize::from(api),
            }),
        }
    }
}

/// What the serving frontend offered, besides the sockets.
pub(crate) struct Offer {
    /// The control connection, to say `ready` on and to hear `released` on.
    link: Link,
    /// The generation the state store must be found at.
    pub generation: u64,
}

/// Asks the frontend serving on `socket` to hand over, saying what this process is
/// started with (`hello`). `None` if nothing serves there (no socket file, or one nobody
/// listens on): this is the only frontend and starts as any start does.
///
/// # Errors
/// The socket cannot be used, or the frontend refuses or answers wrongly.
pub(crate) async fn knock(
    socket: &Path,
    hello: &Message,
) -> Result<Option<(Offer, Sockets)>, HandoverError> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mut link = Link::new(stream);
    link.say(hello).await?;
    match link.hear_with().await? {
        (Message::Offer { generation, api }, sockets) => {
            let sockets = Sockets::of(sockets, api)?;
            Ok(Some((Offer { link, generation }, sockets)))
        }
        (Message::Refused(why), _) => Err(HandoverError::Refused(why)),
        (other, _) => Err(HandoverError::Unexpected(other.line())),
    }
}

impl Offer {
    /// Says this process holds the sockets and accepts the offer, and waits for the
    /// frontend serving to stand down.
    ///
    /// # Errors
    /// The frontend fails to stand down, or the connection fails.
    pub async fn ready(mut self) -> Result<(), HandoverError> {
        self.link.say(&Message::Ready).await?;
        match self.link.hear().await? {
            Message::Released => Ok(()),
            Message::Failed(why) => Err(HandoverError::Failed(why)),
            other => Err(HandoverError::Unexpected(other.line())),
        }
    }
}

#[cfg(test)]
mod tests;

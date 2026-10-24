//! The real [`SafeToStop`]: a call to the frontend's loopback operator API,
//! `GET /v1/members/{id}/safe-to-stop` (`docs/api.md`), through [`OperatorApi`].
//!
//! The host's alias is the id (a node's id; the API accepts a member's number as well). The
//! token and the proxy's secret are handled as [`crate::operator`] and [`crate::secret`] say.
//!
//! What the answer means:
//!
//! - 200: the [`Verdict`]; an answer that is no verdict is an error, never "safe".
//! - 404: [`Stop::Unknown`].
//! - 401, 403, 415: the API refused the call (a wrong token, or a server behind the proxy's
//!   secret that was not sent); an error that says so, which the roll retries until its bound and then fails at.
//! - 409: the server cannot tell whether the node runs a storage member; an error.
//! - 5xx, a refused or timed-out connection: [`PortError::unreachable`].
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::inventory::Host;
use crate::operator::{CHECK, OperatorApi, WHAT};
use crate::ports::{PortError, SafeToStop, Stop, Verdict};

/// The safe-to-stop port over HTTP.
pub struct HttpSafeToStop {
    api: OperatorApi,
}

impl HttpSafeToStop {
    /// An adapter for the API at `addr`; see [`OperatorApi::new`] for the files.
    pub fn new(addr: SocketAddr, token_file: PathBuf) -> Result<HttpSafeToStop, String> {
        Ok(HttpSafeToStop {
            api: OperatorApi::new(addr, token_file)?,
        })
    }

    /// An adapter over a client that was made (and its files checked) already.
    pub fn from_api(api: OperatorApi) -> HttpSafeToStop {
        HttpSafeToStop { api }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> HttpSafeToStop {
        self.api = self.api.with_timeout(timeout);
        self
    }
}

impl SafeToStop for HttpSafeToStop {
    fn check(&self, host: &Host) -> Result<Stop, PortError> {
        let reply = self
            .api
            .get(&format!("/v1/members/{}/safe-to-stop", host.alias))?;
        match reply.status {
            200 => serde_json::from_slice::<Verdict>(&reply.body)
                .map(Stop::Answer)
                .map_err(|_| {
                    PortError::other(format!("{WHAT}'s answer is not a safe-to-stop verdict"))
                }),
            404 => Ok(Stop::Unknown),
            409 => Err(PortError::other(format!(
                "{WHAT} cannot tell whether the host runs a storage member (409)"
            ))),
            _ => Err(reply.failure(WHAT, CHECK)),
        }
    }
}

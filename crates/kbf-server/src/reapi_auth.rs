//! Bearer-token authentication of REAPI calls, with the token file of
//! [`crate::principal`]: a layer over every route of the REAPI listener.
//!
//! [`guard`] wraps the whole router, its fallback included, so every method of
//! every service on it, and any path it does not serve, needs the header; a service
//! added to the router later is covered without a change here. A call is served only
//! when it carries exactly one `authorization` header whose value is `Bearer <token>`
//! (the scheme in any case) and the token's digest is an entry of the file as
//! [`TokenStore::current`] gives it now. Anything else is UNAUTHENTICATED:
//!
//! - no header, two headers, another scheme, or a token no entry admits: the message
//!   says how to configure Buck2 and Bazel to send the header;
//! - the file is missing, breaks a file rule or does not parse (fail closed, see
//!   [`crate::principal`]): every call, with a message that says so and names no
//!   path. The server logs the file's error at ERROR when it changes.
//!
//! A served call carries a [`kbf_front::Caller`] in its request extensions: the
//! principal's name and default QoS. Execute submits at that QoS and logs the name.
//! The token, its digest and the header are never logged or echoed.
//!
//! The check runs in the server, behind the TLS front: the front passes the header
//! through and checks nothing, and the peer address is not trusted (behind a proxy on
//! the same host every peer is loopback).

use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use kbf_front::Caller;
use tonic::Status;
use tonic::service::Routes;

use crate::principal::{TokenStore, TokenStoreError};

/// What a refused caller is told to do.
pub const HOW_TO_SEND: &str = "every call to this server needs `authorization: Bearer \
    <token>` with a token from its operator. Buck2: `http_headers = Authorization: Bearer \
    <token>` under [buck2_re_client], then `buck2 kill` (a running buck2 daemon keeps its \
    old headers). Bazel: --remote_header=Authorization=Bearer <token>";

/// The message of every call refused while the token file is unusable.
pub const FILE_UNUSABLE: &str = "the server's REAPI token file is unusable, so every call \
    is refused until an operator fixes it; the server's log says why";

/// `routes` with every call checked against `tokens` (see the module docs), or
/// `routes` as they are when there is no token file.
#[must_use]
pub fn guard(routes: Routes, tokens: Option<Arc<TokenStore>>) -> Routes {
    let Some(tokens) = tokens else {
        return routes;
    };
    let gate = Arc::new(Gate {
        tokens,
        logged: Mutex::new(None),
    });
    Routes::from(
        routes
            .into_axum_router()
            .layer(from_fn_with_state(gate, check)),
    )
}

struct Gate {
    tokens: Arc<TokenStore>,
    /// The file error last logged, so that one error is logged once, not per call.
    logged: Mutex<Option<Arc<TokenStoreError>>>,
}

impl Gate {
    /// The caller `request` authenticates as, or the refusal.
    fn admit(&self, request: &Request) -> Result<Caller, Status> {
        let principals = match self.tokens.current() {
            Ok(principals) => {
                *self.logged.lock().unwrap_or_else(PoisonError::into_inner) = None;
                principals
            }
            Err(e) => {
                let mut logged = self.logged.lock().unwrap_or_else(PoisonError::into_inner);
                if !logged.as_ref().is_some_and(|l| Arc::ptr_eq(l, &e)) {
                    tracing::error!(
                        error = %e,
                        "--reapi-token-file is unusable: every REAPI call is refused"
                    );
                    *logged = Some(e);
                }
                return Err(Status::unauthenticated(FILE_UNUSABLE));
            }
        };
        let mut headers = request.headers().get_all(AUTHORIZATION).iter();
        let (Some(value), None) = (headers.next(), headers.next()) else {
            return Err(Status::unauthenticated(HOW_TO_SEND));
        };
        let principal = principals
            .admit(value.as_bytes())
            .ok_or_else(|| Status::unauthenticated(HOW_TO_SEND))?;
        Ok(Caller {
            principal: Arc::from(principal.name()),
            qos: principal.qos().clone(),
        })
    }
}

async fn check(State(gate): State<Arc<Gate>>, mut request: Request, next: Next) -> Response {
    match gate.admit(&request) {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(status) => status.into_http(),
    }
}

//! `kbf.map.v1.Membership`: how a storage node registers with a frontend, and the
//! frontend's timer that expires members (`docs/design/architecture-v2.md`, section 6).
//!
//! **Why two unary calls and not the worker stream's `Hello`.** `Hello`/`Welcome` belongs
//! to a daemon: its node id is bound to its certificate and its session is scheduler
//! state. A storage node has no session; membership lives in the state store, so nothing
//! is kept per connection and a node that reconnects to another frontend loses nothing.
//! `Register` is the `Hello` (and says `NOT_FOUND`-then-register-again on a lapse) and
//! `Heartbeat` keeps the row alive. The map comes back in `RegisterResponse`; the push to
//! a connected node is the next change (it can ride `Heartbeat`'s answer).
//!
//! **Who may call.** Under mutual TLS ([`Admission::Certified`]) the caller's certificate
//! must name the role `storage`; without TLS every call is admitted, as on the storage
//! listener.

use std::sync::Arc;

use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_server::{Membership, MembershipServer};
use kbf_storage::service::{Admission, PeerRole, role_of};
use tonic::{Request, Response, Status};

use crate::members::{Members, MembershipError, Progress, Registrant};
use crate::storage_health::{Debts, Reported, Writes};

/// The service over `members`, admitting callers as `admission` says.
pub struct MembershipService {
    members: Arc<Members>,
    admission: Admission,
}

impl MembershipService {
    /// The service over `members`.
    #[must_use]
    pub fn new(members: Arc<Members>, admission: Admission) -> Self {
        Self { members, admission }
    }

    /// The service as a tonic server, ready to add to a listener.
    #[must_use]
    pub fn server(self) -> MembershipServer<Self> {
        MembershipServer::new(self)
    }

    fn admit<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let certs = kbf_tls::peer_certs(request);
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        self.admit_leaf(leaf.map(AsRef::as_ref))
    }

    /// Admits a caller whose verified chain starts with `leaf`.
    ///
    /// # Errors
    /// Under mutual TLS, UNAUTHENTICATED without a certificate; PERMISSION_DENIED for any
    /// role but `storage` (see [`role_of`]).
    pub fn admit_leaf(&self, leaf: Option<&[u8]>) -> Result<(), Status> {
        if self.admission == Admission::Open {
            return Ok(());
        }
        let der = leaf.ok_or_else(|| Status::unauthenticated("no client certificate"))?;
        match role_of(der)? {
            PeerRole::Storage => Ok(()),
            other => Err(Status::permission_denied(format!(
                "role {} may not register as a storage node",
                other.name()
            ))),
        }
    }
}

/// The gRPC answer for a membership failure.
fn status(e: MembershipError) -> Status {
    let message = e.to_string();
    match e {
        MembershipError::Invalid(_) => Status::invalid_argument(message),
        MembershipError::Protocol { .. } | MembershipError::Superseded { .. } => {
            Status::failed_precondition(message)
        }
        MembershipError::NotRegistered(_) => Status::not_found(message),
        MembershipError::Contended => Status::aborted(message),
        MembershipError::Store(_) => Status::unavailable(message),
        MembershipError::NotSettable(_)
        | MembershipError::NoMap
        | MembershipError::Corrupt(_)
        | MembershipError::Map(_) => Status::internal(message),
    }
}

/// Runs `call` on a blocking thread: the store's calls block (its trait says so).
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, MembershipError> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|e| Status::internal(format!("membership task: {e}")))?
        .map_err(status)
}

#[tonic::async_trait]
impl Membership for MembershipService {
    async fn register(
        &self,
        request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        self.admit(&request)?;
        let req = request.into_inner();
        if pb::Role::try_from(req.role) != Ok(pb::Role::Storage) {
            return Err(Status::invalid_argument(
                "role must be ROLE_STORAGE: only storage nodes register",
            ));
        }
        let who = Registrant {
            instance_id: req.instance_id,
            addr: req.addr,
            zone: req.zone,
            weight: req.weight,
            protocol: req.protocol,
            progress: Progress::from_wire(req.catching_up),
        };
        let members = Arc::clone(&self.members);
        let joined = blocking(move || members.register(&who)).await?;
        let beat = self.members.config().beat();
        Ok(Response::new(pb::RegisterResponse {
            member_id: joined.id.0,
            boot: joined.boot,
            heartbeat_interval_ms: u64::try_from(beat.as_millis()).unwrap_or(u64::MAX),
            map: Some(pb::Map::from(&joined.map)),
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        self.admit(&request)?;
        let req = request.into_inner();
        let members = Arc::clone(&self.members);
        let id = kbf_map::MemberId(req.member_id);
        let have = req.have_generation;
        let progress = Progress::from_wire(req.catching_up);
        let said = Reported {
            debts: Debts::from_wire(req.debts_known, req.repair_debts),
            writes: Writes::from_wire(req.writes_known, req.open_writes),
            generation: have,
        };
        let (map_generation, map) = blocking(move || {
            let generation = members.heartbeat(id, req.boot, progress, said)?;
            // Read only for a node that is behind: most beats carry no map.
            let newer = if generation > have {
                members.current_map()?.filter(|map| map.generation() > have)
            } else {
                None
            };
            Ok((generation, newer))
        })
        .await?;
        Ok(Response::new(pb::HeartbeatResponse {
            map_generation,
            map: map.as_ref().map(pb::Map::from),
        }))
    }
}

/// Expires members on the store's clock: every beat, [`Members::refresh`], then frees
/// every expired row of the store ([`Members::reap`]: the farm's session and result rows
/// as well). Never
/// completes; a failure is logged and tried again at the next beat.
pub async fn sweep(members: Arc<Members>) {
    let mut tick = tokio::time::interval(members.config().beat());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let refreshing = Arc::clone(&members);
        if let Err(e) = blocking(move || refreshing.refresh()).await {
            tracing::warn!("membership refresh: {}", e.message());
        }
        let reaping = Arc::clone(&members);
        if let Err(e) = blocking(move || reaping.reap()).await {
            tracing::warn!("state store reap: {}", e.message());
        }
    }
}

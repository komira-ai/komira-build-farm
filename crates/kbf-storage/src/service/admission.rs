//! Who may call which endpoint (`docs/design/architecture-v2.md`, section 9: mTLS on
//! every hop with the role in the certificate; storage accepts only expected roles per
//! endpoint).
//!
//! **What is real.** The per-endpoint table of expected roles ([`Endpoint::allowed`]),
//! the check against the caller's verified client certificate, and the refusals:
//! UNAUTHENTICATED for no certificate, PERMISSION_DENIED for a certificate with no role,
//! two, an unknown one or one the endpoint does not take.
//!
//! **How a certificate names its role** is `kbf-peer`'s: one URI subjectAltName
//! `urn:kbf:role:<name>` ([`ROLE_URI_PREFIX`]), read by `kbf_peer::role_of`; [`role_of`]
//! here is it with the refusal as a gRPC status, and `kbf-server`'s worker listener asks
//! it too.
//!
//! **Names** are `kbf-peer`'s as well ([`names_of`]): the DNS and IP subjectAltNames, which
//! `kbf-server`'s `Membership` service checks against a storage node's instance id and
//! address at `Register` and `Heartbeat`. The storage endpoints here check the role only.
//! Not here, and planned in section 9 and phase P7: the deny list and node-name checks
//! on these endpoints (the worker listener's `Membership` calls apply its deny list and
//! its node-name rule already), and the signed storage-access grant.
//!
//! Plain text ([`Admission::Open`]) has no identity to read, so it admits every call but a
//! copy from another storage node ([`Endpoint::WriteLeg`]), which only a certified storage
//! caller may send: for
//! tests and trials on one machine, as the worker listener's `Peers::Unauthenticated`.

use kbf_peer::RoleError;
pub use kbf_peer::{PeerNames, PeerRole, ROLE_URI_PREFIX, host_of};
use tonic::{Request, Status};

/// The endpoints with their own expectation of who calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `FindMissingBlobs`.
    FindMissing,
    /// ByteStream `Read`.
    Read,
    /// ByteStream `Write`.
    Write,
    /// ByteStream `Write` of a copy another storage node sends (the `kbf-chain-leg`
    /// entry): a primary to its replica, or a repair. Only a storage node makes one; a
    /// call that names itself a copy and is from another role would be a write that is
    /// never forwarded to a second owner.
    WriteLeg,
    /// ByteStream `QueryWriteStatus`: whether a `Write` of the blob has landed, so it
    /// is the roles of the `Write` it asks about.
    QueryWriteStatus,
    /// `ActionStore.Get`: a client's `GetActionResult`.
    ActionGet,
    /// `ActionStore.Prepare`: the first half of an action-cache write.
    ActionPrepare,
    /// `ActionStore.Commit`: the second half.
    ActionCommit,
    /// The `Segments` upload and locate calls.
    Segments,
}

impl Endpoint {
    /// Every endpoint.
    pub const ALL: [Self; 9] = [
        Self::FindMissing,
        Self::Read,
        Self::Write,
        Self::WriteLeg,
        Self::QueryWriteStatus,
        Self::ActionGet,
        Self::ActionPrepare,
        Self::ActionCommit,
        Self::Segments,
    ];

    /// The roles that call this endpoint (section 7: a client's calls come through a
    /// frontend; a daemon fetches inputs and uploads outputs directly; storage nodes
    /// call each other to replicate). A daemon never reads the action cache, and only a
    /// daemon uploads segments.
    #[must_use]
    pub const fn allowed(self) -> &'static [PeerRole] {
        use PeerRole::{Daemon, Frontend, Storage};
        match self {
            Self::FindMissing | Self::Read | Self::Write | Self::QueryWriteStatus => {
                &[Frontend, Storage, Daemon]
            }
            Self::WriteLeg => &[Storage],
            Self::ActionGet => &[Frontend],
            Self::ActionPrepare | Self::ActionCommit => &[Frontend, Daemon],
            Self::Segments => &[Daemon],
        }
    }
}

/// How a storage service learns who is calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Plain text: no identity, every call admitted but [`Endpoint::WriteLeg`], which needs
    /// a caller shown to be a storage node.
    Open,
    /// Mutual TLS: the verified client certificate must name a role the endpoint takes.
    Certified,
}

impl Admission {
    /// Admits the call `request` is to `endpoint`, or says why not.
    ///
    /// # Errors
    /// See [`Admission::check_leaf`].
    pub fn check<T>(&self, request: &Request<T>, endpoint: Endpoint) -> Result<(), Status> {
        let certs = kbf_tls::peer_certs(request);
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        self.check_leaf(leaf.map(AsRef::as_ref), endpoint)
    }

    /// Whether the caller of `request` may mark a write as an unowned copy
    /// ([`crate::Storage::put_unowned`]): only the frontend's identity may, since such a
    /// write is never forwarded to a second owner and owes no repair. Plain text has no
    /// identity and takes the rule of every other call but a copy: admitted.
    #[must_use]
    pub fn permits_unowned<T>(&self, request: &Request<T>) -> bool {
        let certs = kbf_tls::peer_certs(request);
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        self.permits_unowned_leaf(leaf.map(AsRef::as_ref))
    }

    /// [`Admission::permits_unowned`] for a caller whose verified chain starts with `leaf`.
    #[must_use]
    pub fn permits_unowned_leaf(&self, leaf: Option<&[u8]>) -> bool {
        match self {
            Self::Open => true,
            Self::Certified => leaf
                .and_then(|der| role_of(der).ok())
                .is_some_and(|role| role == PeerRole::Frontend),
        }
    }

    /// [`Admission::check`] for a caller whose verified chain starts with `leaf`.
    ///
    /// # Errors
    /// UNAUTHENTICATED for [`Endpoint::WriteLeg`] under [`Admission::Open`]. Under
    /// [`Admission::Certified`], UNAUTHENTICATED if there is no certificate or it
    /// does not parse; PERMISSION_DENIED if it names no role, several, one that is not
    /// a role, or a role `endpoint` does not take.
    pub fn check_leaf(&self, leaf: Option<&[u8]>, endpoint: Endpoint) -> Result<(), Status> {
        if *self == Self::Open {
            return if endpoint == Endpoint::WriteLeg {
                Err(Status::unauthenticated(
                    "a copy from another storage node needs a certified storage caller",
                ))
            } else {
                Ok(())
            };
        }
        let der = leaf.ok_or_else(|| Status::unauthenticated("no client certificate"))?;
        let role = role_of(der)?;
        if endpoint.allowed().contains(&role) {
            return Ok(());
        }
        let refused =
            Status::permission_denied(format!("role {} may not call {endpoint:?}", role.name()));
        tracing::warn!(role = role.name(), ?endpoint, "storage call refused");
        Err(refused)
    }
}

/// The role a DER certificate names (`kbf_peer::role_of`), a refusal as the status every
/// listener that takes a client certificate answers it.
///
/// # Errors
/// UNAUTHENTICATED: the bytes are not a certificate. PERMISSION_DENIED: it names no
/// role or several, or a name that is no role.
pub fn role_of(der: &[u8]) -> Result<PeerRole, Status> {
    kbf_peer::role_of(der).map_err(|e| refusal(&e))
}

/// The DNS and IP names a DER certificate carries (`kbf_peer::names_of`), a refusal as
/// the status every listener that takes a client certificate answers it.
///
/// # Errors
/// UNAUTHENTICATED: the bytes are not a certificate.
pub fn names_of(der: &[u8]) -> Result<PeerNames, Status> {
    kbf_peer::names_of(der).map_err(|e| refusal(&e))
}

/// The status a certificate that gives no identity is answered with.
fn refusal(e: &RoleError) -> Status {
    match e {
        RoleError::NotACertificate(_) => Status::unauthenticated(e.to_string()),
        RoleError::Count(_) | RoleError::Unknown(_) => Status::permission_denied(e.to_string()),
    }
}

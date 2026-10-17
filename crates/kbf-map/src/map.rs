//! The map, its members and its owner functions.

use std::collections::BTreeSet;
use std::fmt;

use kbf_types::Digest;

use crate::deny::DenyEntry;
use crate::hrw;

/// A member's identity, assigned by the state store at registration (no node picks its
/// own). Ordered, so that equal scores break the same way everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberId(pub u64);

impl fmt::Display for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m{}", self.0)
    }
}

/// Where a member is in its life; the table in the crate documentation says what each
/// state owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberState {
    /// Admitted; a write owner at once, read last.
    Joining,
    /// Rebuilding its index or receiving copies; as `Joining`.
    CatchingUp,
    /// Caught up: a full owner.
    Active,
    /// Leaving: takes no new writes, still read in rank order until its successor has
    /// the data.
    Draining,
    /// Lease lapsed, kept in the map for the dwell.
    Down,
}

impl MemberState {
    /// The state as an operator reads it: the lower-case, underscored name `GET
    /// /v1/storage` shows. Exhaustive: a new state fails to compile here instead of
    /// reaching operators unnamed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Joining => "joining",
            Self::CatchingUp => "catching_up",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Down => "down",
        }
    }

    /// Whether the member can be a write owner.
    #[must_use]
    pub const fn takes_writes(self) -> bool {
        !matches!(self, Self::Draining)
    }

    /// Whether the member is among the settled owners: it holds the data placed before
    /// any join or drain in progress.
    const fn is_settled(self) -> bool {
        matches!(self, Self::Active | Self::Draining | Self::Down)
    }
}

/// How owners are chosen from the scores. One rule exists; zone-aware placement is a
/// later addition (`architecture-v2.md` section 6); the enum is exhaustive, so a new rule
/// fails to compile where the wire conversion and the owner functions match on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The top `r` members by weighted rendezvous score.
    ///
    /// This value fixes the whole computation: the SipHash-2-4 keys, the byte layout of
    /// the hashed key (digest hash, size, member id), the reading of the hash as `u`, the
    /// score `weight / neg_ln(u)` and `neg_ln` itself, down to the last bit. Any change to
    /// one of them is a NEW rule value (and a new wire value), never an edit of this one,
    /// because it remaps every blob in the farm.
    Rendezvous,
}

/// One storage node as the map holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The member's id.
    pub id: MemberId,
    /// The address callers dial (non-empty).
    pub addr: String,
    /// The zone label; empty when the node has none.
    pub zone: String,
    /// Its share of the digests, relative to the others (at least 1).
    pub weight: u32,
    /// Its state.
    pub state: MemberState,
}

/// Why a map was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// Generation 0 means "no map seen" on the wire.
    #[error("generation 0 is reserved for \"no map seen\"")]
    ZeroGeneration,
    /// A replication factor of 0 places nothing.
    #[error("replication factor 0")]
    ZeroReplication,
    /// More owners wanted than members.
    #[error("replication factor {r} exceeds the {members} members")]
    ReplicationExceedsMembers {
        /// The factor asked for.
        r: u8,
        /// The members in the map.
        members: usize,
    },
    /// Two members share an id.
    #[error("member id {0} appears twice")]
    DuplicateMember(MemberId),
    /// A member with weight 0 would own nothing.
    #[error("member {0} has weight 0")]
    ZeroWeight(MemberId),
    /// A member with no address cannot be dialled.
    #[error("member {0} has no address")]
    EmptyAddr(MemberId),
    /// The same deny entry twice, after canonical spelling.
    #[error("deny entry \"{0}\" appears twice")]
    DuplicateDenied(DenyEntry),
}

/// The storage map of one generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    generation: u64,
    members: Vec<Member>,
    rule: Rule,
    r: u8,
    min_protocol: u32,
    denied: Vec<DenyEntry>,
}

impl Map {
    /// Builds a map, refusing one that could not place a digest.
    ///
    /// `denied` lists what the frontend refuses: certificate serials, public key hashes and
    /// node ids (not member ids, which are assigned at registration, after the certificate
    /// is checked). Entries are canonical already ([`DenyEntry`]), so a repeat of one is
    /// refused whatever its original spelling.
    pub fn new(
        generation: u64,
        members: Vec<Member>,
        rule: Rule,
        r: u8,
        min_protocol: u32,
        denied: Vec<DenyEntry>,
    ) -> Result<Self, MapError> {
        if generation == 0 {
            return Err(MapError::ZeroGeneration);
        }
        if r == 0 {
            return Err(MapError::ZeroReplication);
        }
        let mut ids = BTreeSet::new();
        for m in &members {
            if !ids.insert(m.id) {
                return Err(MapError::DuplicateMember(m.id));
            }
            if m.weight == 0 {
                return Err(MapError::ZeroWeight(m.id));
            }
            if m.addr.is_empty() {
                return Err(MapError::EmptyAddr(m.id));
            }
        }
        if usize::from(r) > members.len() {
            return Err(MapError::ReplicationExceedsMembers {
                r,
                members: members.len(),
            });
        }
        let mut seen = BTreeSet::new();
        if let Some(twice) = denied.iter().find(|entry| !seen.insert(*entry)) {
            return Err(MapError::DuplicateDenied(twice.clone()));
        }
        Ok(Self {
            generation,
            members,
            rule,
            r,
            min_protocol,
            denied,
        })
    }

    /// The generation; later maps have larger numbers.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The members, in the order the map was built with.
    #[must_use]
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// The placement rule.
    #[must_use]
    pub fn rule(&self) -> Rule {
        self.rule
    }

    /// The replication factor.
    #[must_use]
    pub fn r(&self) -> u8 {
        self.r
    }

    /// The lowest protocol version a member may speak.
    #[must_use]
    pub fn min_protocol(&self) -> u32 {
        self.min_protocol
    }

    /// What the frontend refuses.
    #[must_use]
    pub fn denied(&self) -> &[DenyEntry] {
        &self.denied
    }

    /// The member with this id.
    #[must_use]
    pub fn member(&self, id: MemberId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// Every member, best score first for this digest; equal scores go to the lower id.
    fn ranked(&self, digest: &Digest) -> Vec<&Member> {
        let Rule::Rendezvous = self.rule;
        let mut scored: Vec<(f64, &Member)> = self
            .members
            .iter()
            .map(|m| (hrw::score(digest, m.id, m.weight), m))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        scored.into_iter().map(|(_, m)| m).collect()
    }

    /// Who a write of this digest goes to, primary (rank 1) first.
    ///
    /// The owners are the top `r` by score among the members whose state takes writes,
    /// listed settled members first (best score first), then the joiners (`Joining`,
    /// `CatchingUp`). A joiner takes writes at once but is never rank 1 while a settled
    /// writer exists, because rank 1 is the head of the chain, the one flusher and the
    /// `FindMissing` target, and a joiner holds nothing yet: if the top `r` are all
    /// joiners, the best settled writer takes the place of the lowest-scored joiner. When
    /// no settled writer exists at all (a fresh farm), the joiners are the owners and the
    /// best-scored one is primary; answering nobody would stall the first writes. With
    /// `r` = 1 the only slot is rank 1, so a joiner owns nothing while a settled writer
    /// exists; its slice reaches it by the catch-up copy.
    ///
    /// A `Down` member keeps its slot, so the caller checks [`Member::state`] and records
    /// a repair; with `r` = 1 and the only owner `Down` the caller refuses the write with
    /// `UNAVAILABLE`, as there is no second copy to make. A `Draining` member never
    /// appears. Fewer than `r` come back only when too few members take writes.
    #[must_use]
    pub fn write_owners(&self, digest: &Digest) -> Vec<MemberId> {
        self.write_members(digest)
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn write_members(&self, digest: &Digest) -> Vec<&Member> {
        let writers: Vec<&Member> = self
            .ranked(digest)
            .into_iter()
            .filter(|m| m.state.takes_writes())
            .collect();
        let mut owners: Vec<&Member> = writers.iter().take(usize::from(self.r)).copied().collect();
        if !owners.iter().any(|m| m.state.is_settled())
            && let Some(settled) = writers.iter().find(|m| m.state.is_settled())
        {
            owners.pop();
            owners.push(settled);
        }
        // Stable: settled members first, each group in score order.
        owners.sort_by_key(|m| !m.state.is_settled());
        owners
    }

    /// The order to try members when reading this digest in this generation: the top `r`
    /// settled owners in rank order, then the write owners not yet listed (the members
    /// that may not hold it yet, `Joining` and `CatchingUp` among them, last). `Down`
    /// members are left out: with every owner `Down` the list is empty and the caller
    /// treats the blob as unavailable (`UNAVAILABLE`), not as missing.
    #[must_use]
    pub fn read_order(&self, digest: &Digest) -> Vec<MemberId> {
        self.read_members(digest)
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn read_members(&self, digest: &Digest) -> Vec<&Member> {
        let r = usize::from(self.r);
        let settled: Vec<&Member> = self
            .ranked(digest)
            .into_iter()
            .filter(|m| m.state.is_settled())
            .take(r)
            .collect();
        let mut order: Vec<&Member> = Vec::new();
        for m in settled
            .into_iter()
            .chain(self.write_members(digest))
            .filter(|m| m.state != MemberState::Down)
        {
            if !order.iter().any(|seen| seen.id == m.id) {
                order.push(m);
            }
        }
        order
    }

    /// [`Map::read_order`], then the previous generation's read order without the members
    /// already listed. The members come back as the map that holds them gives them: one
    /// that left between the generations is the `previous` map's `Member`, with the
    /// address it had there.
    #[must_use]
    pub fn read_order_with_previous<'a>(
        &'a self,
        previous: Option<&'a Map>,
        digest: &Digest,
    ) -> Vec<&'a Member> {
        let mut order = self.read_members(digest);
        for m in previous.map_or_else(Vec::new, |p| p.read_members(digest)) {
            if !order.iter().any(|seen| seen.id == m.id) {
                order.push(m);
            }
        }
        order
    }
}

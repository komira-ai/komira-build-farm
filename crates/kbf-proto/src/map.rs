//! `kbf.map.v1` and the pure `kbf_map` types, both ways.
//!
//! Decoding is strict: an unspecified or unknown enum value, a replication factor that is
//! not a `u8`, a denied entry that is not `kind value`, or a map `kbf_map::Map::new` refuses is an error, never a default.

use kbf_map::{DenyEntry, DenyEntryError, Map, MapError, Member, MemberId, MemberState, Rule};

use crate::kbf::map::v1 as pb;

/// Why a wire map was not read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapDecodeError {
    /// A member state this build does not name (or `UNSPECIFIED`).
    #[error("unknown member state {0}")]
    UnknownState(i32),
    /// A rule this build does not name (or `UNSPECIFIED`).
    #[error("unknown placement rule {0}")]
    UnknownRule(i32),
    /// A replication factor above 255.
    #[error("replication factor {0} is above 255")]
    Replication(u32),
    /// A denied entry that is not `kind value` (see `kbf_map::DenyEntry`).
    #[error("denied entry {entry:?}: {source}")]
    Denied {
        /// The string as it came.
        entry: String,
        /// What is wrong with it.
        source: DenyEntryError,
    },
    /// The map is well formed on the wire and refused by the domain.
    #[error("invalid map: {0}")]
    Invalid(#[from] MapError),
}

impl From<MemberState> for pb::MemberState {
    fn from(s: MemberState) -> Self {
        match s {
            MemberState::Joining => Self::Joining,
            MemberState::CatchingUp => Self::CatchingUp,
            MemberState::Active => Self::Active,
            MemberState::Draining => Self::Draining,
            MemberState::Down => Self::Down,
        }
    }
}

fn state_from_wire(v: i32) -> Result<MemberState, MapDecodeError> {
    match pb::MemberState::try_from(v) {
        Ok(pb::MemberState::Joining) => Ok(MemberState::Joining),
        Ok(pb::MemberState::CatchingUp) => Ok(MemberState::CatchingUp),
        Ok(pb::MemberState::Active) => Ok(MemberState::Active),
        Ok(pb::MemberState::Draining) => Ok(MemberState::Draining),
        Ok(pb::MemberState::Down) => Ok(MemberState::Down),
        Ok(pb::MemberState::Unspecified) | Err(_) => Err(MapDecodeError::UnknownState(v)),
    }
}

impl From<&Member> for pb::Member {
    fn from(m: &Member) -> Self {
        Self {
            id: m.id.0,
            addr: m.addr.clone(),
            zone: m.zone.clone(),
            weight: m.weight,
            state: pb::MemberState::from(m.state).into(),
        }
    }
}

impl TryFrom<pb::Member> for Member {
    type Error = MapDecodeError;

    fn try_from(m: pb::Member) -> Result<Self, MapDecodeError> {
        Ok(Self {
            id: MemberId(m.id),
            addr: m.addr,
            zone: m.zone,
            weight: m.weight,
            state: state_from_wire(m.state)?,
        })
    }
}

impl From<&Map> for pb::Map {
    fn from(m: &Map) -> Self {
        let rule = match m.rule() {
            Rule::Rendezvous => pb::Rule::Rendezvous,
        };
        Self {
            generation: m.generation(),
            members: m.members().iter().map(pb::Member::from).collect(),
            rule: rule.into(),
            replication: u32::from(m.r()),
            min_protocol: m.min_protocol(),
            denied: m.denied().iter().map(ToString::to_string).collect(),
        }
    }
}

impl TryFrom<pb::Map> for Map {
    type Error = MapDecodeError;

    fn try_from(m: pb::Map) -> Result<Self, MapDecodeError> {
        let rule = match pb::Rule::try_from(m.rule) {
            Ok(pb::Rule::Rendezvous) => Rule::Rendezvous,
            Ok(pb::Rule::Unspecified) | Err(_) => return Err(MapDecodeError::UnknownRule(m.rule)),
        };
        let r =
            u8::try_from(m.replication).map_err(|_| MapDecodeError::Replication(m.replication))?;
        let members = m
            .members
            .into_iter()
            .map(Member::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let denied = m
            .denied
            .into_iter()
            .map(|entry| {
                entry
                    .parse::<DenyEntry>()
                    .map_err(|source| MapDecodeError::Denied { entry, source })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Map::new(
            m.generation,
            members,
            rule,
            r,
            m.min_protocol,
            denied,
        )?)
    }
}

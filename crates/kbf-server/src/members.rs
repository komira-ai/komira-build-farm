//! Membership in the state store (`docs/design/architecture-v2.md`, sections 6 and 8.4):
//! who the storage nodes are, whether they are alive, and the map built from them.
//! Only a frontend runs this; storage nodes reach it through [`crate::members_rpc`].
//!
//! **Keys.**
//!
//! | key | value | lives |
//! |---|---|---|
//! | `instance/<instance id>` | the member id | for ever: a node that returns keeps its id |
//! | `member/<id>` | the member (address, zone, weight, admin state) and its boot counter | `ttl` after the last heartbeat |
//! | `down/<id>` | nothing | `dwell` from the moment the member went `Down` |
//! | `map` | the current `kbf.map.v1.Map` | for ever; a change is one compare-and-set |
//! | `meta/members_formed` | nothing | for ever: the cluster has had an `Active` member |
//!
//! and the counters `member-id` and `boot/<id>`. The store assigns both numbers, so no
//! node picks its identity; `(id, boot)` is the prefix of the node's object keys.
//!
//! **Life of a member.** `register` writes `member/<id>` as `Active` when the node says
//! its index is whole ([`Progress::CaughtUp`]: a first start of a cluster, or a restart whose
//! meta log already has its rebuild) and as `CatchingUp` when it is rebuilding. A heartbeat
//! rewrites the row to restart its time to live. When the store's clock expires the row
//! (never a frontend's), the next [`Members::refresh`] marks the member `Down` in the map
//! and writes `down/<id>` with the dwell as its time to live in the same transaction;
//! when that expires too, the next refresh drops the member from the map. A member that
//! registers again meanwhile (a restart: same id, larger boot counter) is `Active` again
//! if its index is whole, `CatchingUp` if not, and its `down/<id>` mark is deleted. The
//! last member never leaves: a map needs one member, so it stays `Down` in the map.
//!
//! **The map** is a function of the rows: [`Members::refresh`] reads `map`, `member/` and
//! the `down/` marks, builds the next member list, and if it differs commits generation
//! `g + 1` with the compare-and-set on `map`'s version and a check on every row it read,
//! so a row that changed meanwhile fails the commit rather than being decided on stale
//! reads. A conflict is read again; two frontends racing give one winner and one retry.
//! Anyone may call it; the frontend does so after every change and on a timer.
//!
//! **Promotion.** A node's own request moves it in two ways only. A registration saying the
//! index is whole ([`Progress::CaughtUp`]) writes `Active`, so a rolling restart leaves
//! every member `Active`; a registration saying it is rebuilding ([`Progress::CatchingUp`])
//! writes `CatchingUp`, and its first heartbeat saying the rebuild is done makes it `Active`,
//! in the same compare-and-set as the beat, so a state set meanwhile (`Draining`) is never
//! overwritten. Whatever makes a member `Active` also writes `meta/members_formed` in the
//! same transaction. `Joining` is set by hand ([`Members::set_state`], the hook the join
//! handoff and the operator call); a heartbeat changes no other state: a `Joining` member
//! waits for the handoff, and a member that is `Draining`, `Active` or `Down` stays so
//! whatever it reports.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_map::{Map, MapError, Member, MemberId, MemberState, Rule};
use kbf_mapsync::MapSource;
use kbf_proto::kbf::map::v1 as pb;
use kbf_state::{Cond, Store, StoreError, Txn, Version, Versioned};
use prost::Message as _;
use tokio::sync::watch;

const MAP_KEY: &str = "map";
const MEMBERS: &str = "member/";
const ID_COUNTER: &str = "member-id";
/// Written with every `Active` member and never deleted: the cluster has had an
/// `Active` member.
const FORMED: &str = "meta/members_formed";
/// Reads and commits tried before a call gives up on writers it keeps racing.
const ATTEMPTS: usize = 16;

/// The newest protocol version this build speaks, and the lowest a registration may
/// name (the map's `min_protocol`).
pub const PROTOCOL: u32 = 1;

/// How long rows live and how many owners a digest has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MembershipConfig {
    /// A member row lives this long after its last heartbeat; then the member is `Down`.
    pub ttl: Duration,
    /// A `Down` member stays in the map this long before it leaves.
    pub dwell: Duration,
    /// The replication factor of the map (clamped to the member count).
    pub replication: u8,
}

impl Default for MembershipConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(15),
            dwell: Duration::from_secs(300),
            replication: 2,
        }
    }
}

impl MembershipConfig {
    /// How often a node heartbeats and a frontend refreshes: a third of the time to
    /// live, so two beats can be lost before the row expires.
    #[must_use]
    pub fn beat(&self) -> Duration {
        self.ttl / 3
    }
}

/// Whether a node is still rebuilding its index, as it says when it registers and at
/// every heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Rebuilding: it enters the map as `CatchingUp`, and stays so.
    CatchingUp,
    /// Its index is whole: a `CatchingUp` member becomes `Active`.
    CaughtUp,
}

impl Progress {
    /// The wire's `catching_up` field.
    #[must_use]
    pub const fn from_wire(catching_up: bool) -> Self {
        if catching_up {
            Self::CatchingUp
        } else {
            Self::CaughtUp
        }
    }
}

/// What a node says about itself when it registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registrant {
    /// Stable across restarts: the key of its member id.
    pub instance_id: String,
    /// The address callers dial.
    pub addr: String,
    /// The zone label; may be empty.
    pub zone: String,
    /// Its relative capacity, at least 1.
    pub weight: u32,
    /// The newest protocol version it speaks.
    pub protocol: u32,
    /// Whether it is rebuilding its index.
    pub progress: Progress,
}

/// What a registration returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// The member id the store assigned (the same one for the same instance).
    pub id: MemberId,
    /// The boot counter: larger at every registration of this member.
    pub boot: u64,
    /// The map after this registration.
    pub map: Map,
}

/// Why a membership call failed.
#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    /// The state store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The registration is malformed.
    #[error("invalid registration: {0}")]
    Invalid(&'static str),
    /// The node speaks a protocol older than the map's minimum.
    #[error("protocol {got} is below the minimum {min}")]
    Protocol {
        /// What the node speaks.
        got: u32,
        /// What the map requires.
        min: u32,
    },
    /// The member has no row: it expired, or never registered. Register again.
    #[error("member {0} has no live row: register again")]
    NotRegistered(MemberId),
    /// A newer registration of the same member exists.
    #[error("member {id} boot {boot} was superseded by boot {current}")]
    Superseded {
        /// The member.
        id: MemberId,
        /// The boot counter the caller holds.
        boot: u64,
        /// The one on the row.
        current: u64,
    },
    /// Only `Joining`, `CatchingUp`, `Active` and `Draining` are set by hand; `Down` is
    /// the store's verdict.
    #[error("a member cannot be set {0:?}")]
    NotSettable(MemberState),
    /// Another writer kept winning the compare-and-set.
    #[error("gave up after {ATTEMPTS} attempts against other writers")]
    Contended,
    /// No member is live, so there is no map to answer with.
    #[error("no member is live")]
    NoMap,
    /// A stored value does not decode.
    #[error("stored membership is unreadable: {0}")]
    Corrupt(String),
    /// The members the rows give do not make a map.
    #[error("the members do not make a map: {0}")]
    Map(#[from] MapError),
}

/// A stored member row: the member as the map will hold it, and its boot counter.
#[derive(Clone, PartialEq, prost::Message)]
struct Row {
    #[prost(message, optional, tag = "1")]
    member: Option<pb::Member>,
    #[prost(uint64, tag = "2")]
    boot: u64,
}

/// A decoded row and the version it was read at.
struct Live {
    member: Member,
    boot: u64,
    version: Version,
}

fn decode_row(stored: &Versioned) -> Result<Live, MembershipError> {
    let row = Row::decode(stored.value.as_slice())
        .map_err(|e| MembershipError::Corrupt(format!("member row: {e}")))?;
    let member = Member::try_from(row.member.unwrap_or_default())
        .map_err(|e| MembershipError::Corrupt(format!("member row: {e}")))?;
    Ok(Live {
        member,
        boot: row.boot,
        version: stored.version,
    })
}

fn encode_row(member: &Member, boot: u64) -> Vec<u8> {
    Row {
        member: Some(pb::Member::from(member)),
        boot,
    }
    .encode_to_vec()
}

fn decode_map(stored: &Versioned) -> Result<Map, MembershipError> {
    let wire = pb::Map::decode(stored.value.as_slice())
        .map_err(|e| MembershipError::Corrupt(format!("map: {e}")))?;
    Map::try_from(wire).map_err(|e| MembershipError::Corrupt(format!("map: {e}")))
}

fn member_key(id: MemberId) -> String {
    format!("{MEMBERS}{}", id.0)
}

fn down_key(id: MemberId) -> String {
    format!("down/{}", id.0)
}

fn boot_counter(id: MemberId) -> String {
    format!("boot/{}", id.0)
}

fn instance_key(instance: &str) -> String {
    format!("instance/{instance}")
}

/// What one refresh decided: the next map, who just went down, and what it read.
struct Plan {
    map: Map,
    newly_down: Vec<MemberId>,
    guards: Vec<(MemberId, Cond)>,
}

/// The next map from the current one, the live rows and the live `down/` marks; `None`
/// when it would not change (or there is no member to put in one). Pure.
fn plan(
    current: Option<&Map>,
    rows: &BTreeMap<MemberId, Live>,
    marks: &BTreeSet<MemberId>,
    config: &MembershipConfig,
) -> Result<Option<Plan>, MapError> {
    let mut members: BTreeMap<MemberId, Member> = rows
        .iter()
        .map(|(id, live)| (*id, live.member.clone()))
        .collect();
    let mut guards: Vec<(MemberId, Cond)> = rows
        .iter()
        .map(|(id, live)| (*id, Cond::Version(live.version)))
        .collect();
    let mut newly_down = Vec::new();
    for old in current.map_or(&[][..], Map::members) {
        if members.contains_key(&old.id) {
            continue;
        }
        guards.push((old.id, Cond::Absent));
        if old.state != MemberState::Down {
            newly_down.push(old.id);
            let down = Member {
                state: MemberState::Down,
                ..old.clone()
            };
            members.insert(old.id, down);
        } else if marks.contains(&old.id) {
            members.insert(old.id, old.clone());
        }
    }
    if members.is_empty() {
        return Ok(None);
    }
    let members: Vec<Member> = members.into_values().collect();
    let count = u8::try_from(members.len()).unwrap_or(u8::MAX);
    let r = config.replication.min(count);
    if current.is_some_and(|c| c.members() == members.as_slice() && c.r() == r) {
        return Ok(None);
    }
    let map = Map::new(
        current.map_or(1, |c| c.generation() + 1),
        members,
        Rule::Rendezvous,
        r,
        current.map_or(PROTOCOL, Map::min_protocol),
        current.map_or_else(Vec::new, |c| c.denied().to_vec()),
    )?;
    Ok(Some(Plan {
        map,
        newly_down,
        guards,
    }))
}

/// How a row is rewritten in place.
#[derive(Clone, Copy)]
enum Touch {
    /// A heartbeat of this boot, and what it reports: a whole time to live more.
    Beat(u64, Progress),
    /// An administrator's state change: the time to live it had.
    State(MemberState),
}

/// The maps a process has seen: the newest, and the generation it replaced.
#[derive(Default)]
struct Seen {
    latest: Option<Arc<Map>>,
    previous: Option<Arc<Map>>,
}

/// Membership over a state store.
pub struct Members {
    store: Arc<dyn Store>,
    config: MembershipConfig,
    /// The newest map this process has seen, whoever bumped it, and the one before it.
    seen: Mutex<Seen>,
    /// Its generation (0 before any): [`MapSource::changes`].
    generation: watch::Sender<u64>,
}

impl Members {
    /// Membership kept in `store`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, config: MembershipConfig) -> Self {
        Self {
            store,
            config,
            seen: Mutex::default(),
            generation: watch::Sender::new(0),
        }
    }

    /// Notes `map` as seen: if it is newer than the last, keeps it and wakes the
    /// [`MapSource::changes`] receivers.
    fn publish(&self, map: &Map) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen
            .latest
            .as_ref()
            .is_none_or(|l| l.generation() < map.generation())
        {
            seen.previous = seen.latest.replace(Arc::new(map.clone()));
            self.generation.send_replace(map.generation());
        }
    }

    /// The newest map this process has seen and the one it replaced, which a call routed
    /// by the newer finds a blob written before the change in. `None` before any map.
    /// Reads no store: [`Members::current_map`] does, and publishes what it finds.
    #[must_use]
    pub fn view(&self) -> Option<(Arc<Map>, Option<Arc<Map>>)> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let latest = seen.latest.clone()?;
        Some((latest, seen.previous.clone()))
    }

    /// The map the push protocol reads: its current value (`None` until the first member
    /// registers) and a receiver of its generation, which changes on every bump this
    /// process commits or observes (a bump by another frontend is seen at the next
    /// [`Members::refresh`], which the frontend runs every beat). Seeded from the store,
    /// so a restarted frontend has the map its members left.
    ///
    /// # Errors
    /// The store failing.
    pub fn map_source(self: &Arc<Self>) -> Result<Arc<dyn MapSource>, MembershipError> {
        self.current_map()?;
        Ok(Arc::new(MemberMap {
            members: Arc::clone(self),
        }))
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &MembershipConfig {
        &self.config
    }

    /// Registers a storage node: assigns its id (the same for the same instance) and a
    /// larger boot counter, writes its row as `Active` (`CatchingUp` if it is rebuilding
    /// its index, see the module docs), and rebuilds the map.
    ///
    /// # Errors
    /// [`MembershipError::Invalid`] or [`MembershipError::Protocol`] for a bad request;
    /// [`MembershipError::Superseded`] if a newer registration of the instance won;
    /// the store failing; [`MembershipError::Contended`].
    pub fn register(&self, who: &Registrant) -> Result<Registration, MembershipError> {
        if who.instance_id.is_empty() || who.addr.is_empty() || who.weight == 0 {
            return Err(MembershipError::Invalid(
                "an instance id, an address and a weight of at least 1 are required",
            ));
        }
        if who.protocol < PROTOCOL {
            return Err(MembershipError::Protocol {
                got: who.protocol,
                min: PROTOCOL,
            });
        }
        let (id, boot) = self.assign(&who.instance_id)?;
        let member = Member {
            id,
            addr: who.addr.clone(),
            zone: who.zone.clone(),
            weight: who.weight,
            state: match who.progress {
                Progress::CatchingUp => MemberState::CatchingUp,
                Progress::CaughtUp => MemberState::Active,
            },
        };
        self.write_row(&member, boot)?;
        let map = self.refresh()?.ok_or(MembershipError::NoMap)?;
        Ok(Registration { id, boot, map })
    }

    /// The id of `instance` (assigned on first sight) and a new boot counter for it.
    fn assign(&self, instance: &str) -> Result<(MemberId, u64), MembershipError> {
        let key = instance_key(instance);
        for _ in 0..ATTEMPTS {
            let (id, txn) = match self.store.get(&key)? {
                Some(known) => {
                    let id = MemberId(decode_id(&known.value)?);
                    let txn = Txn::new().check(&key, Cond::Version(known.version));
                    (id, txn.next(boot_counter(id)))
                }
                None => {
                    // A counter has no precondition, so it cannot conflict.
                    let taken = self.store.txn(Txn::new().next(ID_COUNTER))?;
                    let id = MemberId(taken.map_err(|_| MembershipError::Contended)?.counters[0]);
                    let txn = Txn::new().put(&key, id.0.to_be_bytes(), Cond::Absent);
                    (id, txn.next(boot_counter(id)))
                }
            };
            if let Ok(commit) = self.store.txn(txn)? {
                return Ok((id, commit.counters[0]));
            }
        }
        Err(MembershipError::Contended)
    }

    /// Stores `member`'s row with a whole time to live, unless a newer boot is there, and
    /// clears its `down/` mark. An `Active` row also writes `meta/members_formed`, in the
    /// same transaction.
    fn write_row(&self, member: &Member, boot: u64) -> Result<(), MembershipError> {
        let key = member_key(member.id);
        for _ in 0..ATTEMPTS {
            let cond = match self.store.get(&key)? {
                Some(stored) => {
                    let current = decode_row(&stored)?.boot;
                    if current > boot {
                        return Err(MembershipError::Superseded {
                            id: member.id,
                            boot,
                            current,
                        });
                    }
                    Cond::Version(stored.version)
                }
                None => Cond::Absent,
            };
            let mut txn = Txn::new();
            if member.state == MemberState::Active {
                txn = txn.put(FORMED, Vec::new(), Cond::Any);
            }
            let txn = txn
                .put_ttl(&key, encode_row(member, boot), self.config.ttl, cond)
                .delete(down_key(member.id), Cond::Any);
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
        Err(MembershipError::Contended)
    }

    /// Restarts the time to live of `id`'s row and, if the member is `CatchingUp` and
    /// `progress` says it is done, makes it `Active` and rebuilds the map. Returns the
    /// generation of the current map.
    ///
    /// # Errors
    /// [`MembershipError::NotRegistered`] if the row expired (register again);
    /// [`MembershipError::Superseded`] if `boot` is not the row's; the store failing.
    pub fn heartbeat(
        &self,
        id: MemberId,
        boot: u64,
        progress: Progress,
    ) -> Result<u64, MembershipError> {
        if self.touch(id, Touch::Beat(boot, progress))? {
            return Ok(self.refresh()?.map_or(0, |map| map.generation()));
        }
        Ok(self.current_map()?.map_or(0, |map| map.generation()))
    }

    /// Sets a live member's state (the promotion hook: `Joining`, `CatchingUp`, `Active`,
    /// `Draining`) and rebuilds the map. The row keeps the time to live it had.
    ///
    /// # Errors
    /// [`MembershipError::NotSettable`] for `Down`; [`MembershipError::NotRegistered`] if
    /// the member has no live row; the store failing.
    pub fn set_state(&self, id: MemberId, state: MemberState) -> Result<Map, MembershipError> {
        if state == MemberState::Down {
            return Err(MembershipError::NotSettable(state));
        }
        self.touch(id, Touch::State(state))?;
        self.refresh()?.ok_or(MembershipError::NoMap)
    }

    /// Rewrites `id`'s row as `how` says; whether its state changed.
    fn touch(&self, id: MemberId, how: Touch) -> Result<bool, MembershipError> {
        let key = member_key(id);
        for _ in 0..ATTEMPTS {
            let stored = self
                .store
                .get(&key)?
                .ok_or(MembershipError::NotRegistered(id))?;
            let mut live = decode_row(&stored)?;
            let before = live.member.state;
            let ttl = match how {
                Touch::Beat(boot, _) if boot != live.boot => {
                    return Err(MembershipError::Superseded {
                        id,
                        boot,
                        current: live.boot,
                    });
                }
                Touch::Beat(_, progress) => {
                    if before == MemberState::CatchingUp && progress == Progress::CaughtUp {
                        live.member.state = MemberState::Active;
                    }
                    self.config.ttl
                }
                Touch::State(state) => {
                    live.member.state = state;
                    stored
                        .expires_at
                        .map_or(self.config.ttl, |at| at.since(self.store.now()))
                }
            };
            let mut put = Txn::new().put_ttl(
                &key,
                encode_row(&live.member, live.boot),
                ttl,
                Cond::Version(stored.version),
            );
            if live.member.state == MemberState::Active && before != MemberState::Active {
                put = put.put(FORMED, Vec::new(), Cond::Any);
            }
            if self.store.txn(put)?.is_ok() {
                return Ok(live.member.state != before);
            }
        }
        Err(MembershipError::Contended)
    }

    /// The current map, or `None` before the first member registered. This is what a
    /// map push (`Welcome.map`, `MapUpdate`) and a `STALE_MAP` answer read.
    ///
    /// # Errors
    /// The store failing; the stored map not decoding.
    pub fn current_map(&self) -> Result<Option<Map>, MembershipError> {
        let stored = self.store.get(MAP_KEY)?;
        let map = stored.as_ref().map(decode_map).transpose()?;
        map.iter().for_each(|m| self.publish(m));
        Ok(map)
    }

    /// Frees the rows the store's clock has expired (member, dwell and session rows
    /// among them) and returns how many. Reads hide an expired row whether or not this
    /// has run; it only keeps the file from growing.
    ///
    /// # Errors
    /// The store failing.
    pub fn reap(&self) -> Result<usize, MembershipError> {
        Ok(self.store.reap_expired()?)
    }

    /// Brings the map up to date with the rows: members that expired go `Down`, `Down`
    /// members whose dwell ran out leave, new and changed rows enter. Returns the map
    /// after, and bumps the generation by one compare-and-set only if something changed.
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Contended`].
    pub fn refresh(&self) -> Result<Option<Map>, MembershipError> {
        for _ in 0..ATTEMPTS {
            let stored = self.store.get(MAP_KEY)?;
            let current = stored.as_ref().map(decode_map).transpose()?;
            let rows = self.read_rows()?;
            let marks = self.read_marks(current.as_ref())?;
            let Some(plan) = plan(current.as_ref(), &rows, &marks, &self.config)? else {
                current.iter().for_each(|m| self.publish(m));
                return Ok(current);
            };
            let cond = stored.map_or(Cond::Absent, |s| Cond::Version(s.version));
            let wire = pb::Map::from(&plan.map).encode_to_vec();
            let mut txn = Txn::new().put(MAP_KEY, wire, cond);
            for id in &plan.newly_down {
                txn = txn.put_ttl(down_key(*id), Vec::new(), self.config.dwell, Cond::Any);
            }
            for (id, cond) in &plan.guards {
                txn = txn.check(member_key(*id), *cond);
            }
            if self.store.txn(txn)?.is_ok() {
                self.publish(&plan.map);
                return Ok(Some(plan.map));
            }
        }
        Err(MembershipError::Contended)
    }

    fn read_rows(&self) -> Result<BTreeMap<MemberId, Live>, MembershipError> {
        let mut rows = BTreeMap::new();
        for (_, stored) in self.store.scan_prefix(MEMBERS)? {
            let live = decode_row(&stored)?;
            rows.insert(live.member.id, live);
        }
        Ok(rows)
    }

    /// The `Down` members of `current` whose dwell has not run out.
    fn read_marks(&self, current: Option<&Map>) -> Result<BTreeSet<MemberId>, MembershipError> {
        let mut marks = BTreeSet::new();
        for m in current.map_or(&[][..], Map::members) {
            if m.state == MemberState::Down && self.store.get(&down_key(m.id))?.is_some() {
                marks.insert(m.id);
            }
        }
        Ok(marks)
    }
}

fn decode_id(value: &[u8]) -> Result<u64, MembershipError> {
    <[u8; 8]>::try_from(value)
        .map(u64::from_be_bytes)
        .map_err(|_| MembershipError::Corrupt("instance entry is not a member id".to_owned()))
}

/// [`Members`] as a [`MapSource`].
struct MemberMap {
    members: Arc<Members>,
}

impl MapSource for MemberMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.members.view().map(|(latest, _)| latest)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.members.generation.subscribe()
    }
}

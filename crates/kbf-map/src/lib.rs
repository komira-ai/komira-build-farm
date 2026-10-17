//! The storage map and rendezvous hashing over it (`docs/design/architecture-v2.md`,
//! sections 6 and 8): which members own a digest, who to write to, and in what order to
//! read.
//!
//! A [`Map`] is a generation number, the members with their address, zone, weight and
//! [`MemberState`], a placement [`Rule`], the replication factor `r`, the lowest protocol
//! version a member may speak, and the deny entries ([`DenyEntry`]: a certificate serial, a public key hash or a node id). [`Map::new`] refuses a map that could
//! not place a digest (see [`MapError`]), so every `Map` is usable.
//!
//! Owners are the top `r` members by weighted rendezvous score ([`hrw`] says how the
//! score is made and why it never changes). A member's score for a digest is independent
//! of the other members, so adding or removing one moves only the digests it wins.
//!
//! # Who owns what, by state
//!
//! The write owners of a digest are the top `r` members, by score, among those that take
//! writes, listed settled members first: a joiner takes writes at once but is never rank 1
//! (the chain head, the flusher, the `FindMissing` target) while a settled writer exists
//! (when none exists, a fresh farm, the best-scored joiner is primary: [`Map::write_owners`]).
//! Reads try the *settled* owners (the top `r` among members that hold data) first, in rank order, then the write owners not yet listed, which are the members that
//! may not have the data yet. A member in state `Down` is never listed for reading.
//!
//! | state        | takes writes                                | held data, read in rank order | read |
//! |--------------|---------------------------------------------|-------------------------------|------|
//! | `Joining`    | yes, at once (not at r = 1), never rank 1   | no                            | last |
//! | `CatchingUp` | yes, never rank 1                           | no                            | last |
//! | `Active`     | yes                                         | yes                           | rank order |
//! | `Draining`   | no: its successor takes its slot at once    | yes                           | rank order, before the successor |
//! | `Down`       | yes: it keeps its slot for the dwell; the caller skips it and records a repair | yes | never listed |
//!
//! Draining is the reverse of joining: join adds a write owner that is read last until
//! it has caught up; drain removes a write owner that is read first until its successor
//! has. `Down` keeps its slot so that a flapping member remaps nothing; when the dwell
//! ends the member leaves the map and its slice moves (a new generation).
//!
//! `Down` is also how an operator takes a running member out of the write path for a planned
//! restart (`kbf-server`'s `Members::plan_down`): there is no further state on the wire, so a
//! reader built before it reads such a member like any `Down` one.
//!
//! Across generations, [`Map::read_order_with_previous`] lists this generation's read
//! order, then the previous generation's members not yet listed: a blob written before a
//! join or a drain is found where it was.
//!
//! This is a pure crate: no I/O, async runtime, clock or randomness. The layering test in
//! `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod deny;
mod hrw;
mod map;

pub use deny::{DenyEntry, DenyEntryError, normalize_serial};
pub use map::{Map, MapError, Member, MemberId, MemberState, Rule};

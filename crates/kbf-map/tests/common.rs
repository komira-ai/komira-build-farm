//! Shared fixtures of the placement and validation tests.
#![allow(dead_code)]

use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

pub fn member(id: u64, weight: u32, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("10.0.0.{id}:7000"),
        zone: format!("z{}", id % 3),
        weight,
        state,
    }
}

/// A valid map of generation `generation`, the given replication factor and members.
pub fn map(generation: u64, r: u8, members: Vec<Member>) -> Map {
    Map::new(generation, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a valid map")
}

/// `n` digests spread like real ones: the SHA-256 of a counter, with a size.
pub fn digests(n: u64) -> Vec<Digest> {
    (0..n)
        .map(|i| {
            let hash = Sha256::digest(i.to_le_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, i % 7919)
        })
        .collect()
}

pub fn ids(of: &[u64]) -> Vec<MemberId> {
    of.iter().map(|&i| MemberId(i)).collect()
}

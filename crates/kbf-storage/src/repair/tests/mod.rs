//! The repair journal over [`FaultFs`]: a crash at every operation with every torn
//! write, an error at every operation, and damage at every byte.

mod journal;
mod repairs;

use kbf_map::MemberId;
use kbf_types::{Digest, DigestFunction};

use super::RepairRecord;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n) * 100)
}

fn record(n: u8) -> RepairRecord {
    RepairRecord {
        digest: digest(n),
        replica: MemberId(u64::from(n) + 10),
        generation: u64::from(n) + 1000,
    }
}

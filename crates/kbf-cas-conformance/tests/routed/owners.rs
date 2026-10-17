//! A farm of several members, each a [`Fake`], all of them owners of every blob, with a
//! read-repair hook that writes down what it is told.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_route::{MapView, Nodes, ReadRepair, RepairLimit, Router};
use kbf_storage::{Storage, StorageError};
use kbf_transfer::{RoutedCas, TransferConfig};
use kbf_types::{Digest, DigestFunction};

use super::support::{Fake, Fixed};

/// What the hook was told: the blob, who answered, who did not have it.
pub type Told = (Digest, u64, Vec<u64>);

/// A repair hook that writes down what it is told.
#[derive(Default)]
pub struct Hook(Mutex<Vec<Told>>);

impl ReadRepair for Hook {
    fn found_elsewhere(&self, digest: &Digest, found_at: MemberId, missing_at: &[MemberId]) {
        let missing = missing_at.iter().map(|m| m.0).collect();
        self.0
            .lock()
            .expect("lock")
            .push((*digest, found_at.0, missing));
    }
}

impl Hook {
    pub fn told(&self) -> Vec<Told> {
        self.0.lock().expect("lock").clone()
    }
}

/// Member `id` is `fakes[id]`.
struct Members(BTreeMap<u64, Arc<Fake>>);

impl Nodes for Members {
    fn node(&self, member: &Member, _: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(self.0[&member.id.0].clone())
    }
}

/// A data plane over `count` members, every one an owner of every blob, with a hook that
/// is never rate-limited.
pub struct Owners {
    pub cas: Arc<RoutedCas>,
    pub hook: Arc<Hook>,
    router: Router,
    fakes: BTreeMap<u64, Arc<Fake>>,
}

impl Owners {
    /// Owners that each serve `copies[i]`, the `i`th in the read order of `digest`, under
    /// any digest asked for.
    pub fn serving(digest: &kbf_proto::reapi::Digest, copies: &[&[u8]]) -> Self {
        let count = u64::try_from(copies.len()).expect("a few owners");
        let fakes: BTreeMap<u64, Arc<Fake>> = (1..=count).map(|id| (id, Fake::new(b""))).collect();
        let members = (1..=count)
            .map(|id| Member {
                id: MemberId(id),
                addr: format!("node-{id:02}:7000"),
                zone: String::new(),
                weight: 1,
                state: MemberState::Active,
            })
            .collect();
        let r = u8::try_from(count).expect("a few owners");
        let map = Map::new(1, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map");
        let view = MapView {
            map: Arc::new(map),
            previous: None,
        };
        let hook = Arc::new(Hook::default());
        let many = NonZeroU32::new(1000).expect("not zero");
        let limit = RepairLimit {
            burst: many,
            per_second: many,
        };
        let router = Router::new(Arc::new(Fixed(view)), Arc::new(Members(fakes.clone())))
            .with_read_repair(hook.clone(), limit);
        let cas = Arc::new(RoutedCas::new(router.clone(), &TransferConfig::default()));
        let owners = Self {
            cas,
            hook,
            router,
            fakes,
        };
        for (fake, copy) in owners.in_order(digest).iter().zip(copies) {
            fake.set_data(copy);
        }
        owners
    }

    /// The ids of the owners of `digest`, in the order a read asks them.
    pub fn order(&self, digest: &kbf_proto::reapi::Digest) -> Vec<u64> {
        let placed = self.router.place(&parsed(digest)).expect("a map");
        placed.read.iter().map(|m| m.id.0).collect()
    }

    /// The owners of `digest`, in the order a read asks them.
    pub fn in_order(&self, digest: &kbf_proto::reapi::Digest) -> Vec<Arc<Fake>> {
        self.order(digest)
            .iter()
            .map(|id| Arc::clone(&self.fakes[id]))
            .collect()
    }

    /// How many reads each owner of `digest` was asked, in read order.
    pub fn reads(&self, digest: &kbf_proto::reapi::Digest) -> Vec<usize> {
        self.in_order(digest)
            .iter()
            .map(|fake| fake.reads.load(Ordering::SeqCst))
            .collect()
    }
}

/// `digest` as storage names it.
pub fn parsed(digest: &kbf_proto::reapi::Digest) -> Digest {
    Digest::parse(DigestFunction::Sha256, &kbf_transfer::label(digest)).expect("parse")
}

//! What the tests of the routes share: runs of a shape, and waiting for the rows the
//! history writer puts in the index after the chunk.

use std::time::Duration;

use kbf_history::records::InvocationHeader;
use kbf_history::{IndexRoot, InvKey, StatusRow};
use kbf_invocation::{ActionShape, Event, InvocationId, Owner, Target, TargetKind, Text};
use kbf_state::Store as _;
use prost::Message as _;

use crate::pages::digest;
use crate::world::{World, ran};

/// A shape of key `key` and configuration `cfg`; a test when `test`, with `target` as
/// the label the server read from its paths.
pub fn shape(key: u64, cfg: &str, test: bool, target: Option<&str>) -> ActionShape {
    ActionShape {
        key,
        cfg: (cfg != "-").then(|| cfg.to_owned()),
        target: target.map(|label| Target {
            kind: TargetKind::Target,
            label: Text::sent(label),
        }),
        test,
    }
}

/// The events of action `n` of `id` on `instance` ([`ran`]), with the shape the server read
/// from its Command set on the row before the run ends.
pub fn run(id: &str, instance: &str, shape: ActionShape, n: u8, exit_code: i32) -> Vec<Event> {
    let mut events = ran(id, instance, n, u64::from(n), exit_code);
    let command = Event::Command {
        id: InvocationId::new(id),
        owner: Owner::default(),
        action: digest(u64::from(n)),
        command: digest(99),
        shape: Some(Box::new(shape)),
    };
    events.insert(1, command);
    events
}

/// Waits until `ready` holds, polling the stores the writer puts rows in.
pub async fn until(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

/// Waits until the index holds `shapes` catalog rows and `statuses` status rows.
pub async fn indexed(world: &World, shapes: usize, statuses: usize) {
    until("the shape and status rows", || {
        world.rows("ui/shape/").len() == shapes && world.rows("ui/status/").len() == statuses
    })
    .await;
}

/// The status row of the test (`instance`, `shape`, `cfg`), if the index holds one.
pub fn status(world: &World, instance: &str, shape: u64, cfg: &str) -> Option<StatusRow> {
    let key = IndexRoot::default().status(instance, shape, cfg);
    let row = world.state.fake.get(&key).expect("a read")?;
    Some(StatusRow::decode(row.value.as_slice()).expect("a status row"))
}

/// The header row of `inv`, if the index holds one.
pub fn header(world: &World, inv: &str) -> Option<InvocationHeader> {
    let key = IndexRoot::default().invid(&InvKey::from_record_id(inv));
    let row = world.state.fake.get(&key).expect("a read")?;
    Some(InvocationHeader::decode(row.value.as_slice()).expect("a header"))
}

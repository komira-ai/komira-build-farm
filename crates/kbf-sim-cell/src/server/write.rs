//! The direct write path of the simulated server: an upload is PUT to the object
//! store, indexed with `PutBlob` once the store has it, and only then acknowledged,
//! with a crash hook at each [`StorePoint`] and a deadline on every PUT.

use kbf_meta::{Applied, BlobAnswer, Command, Location, ObjectId, StoreId};
use kbf_sim::{NodeId, Output};
use kbf_store::Fault;
use kbf_types::FarmTime;

use super::{Cut, PUT_TIMEOUT, Server, T_PUT, Up};
use crate::Msg;
use crate::hook::{Down, StorePoint};

impl Server {
    /// The direct write path: `Upload`, then the store's answer. Any other message is
    /// handed back.
    pub(super) fn write_path(&mut self, now: FarmTime, from: &NodeId, msg: Msg) -> Option<Msg> {
        self.up.as_ref()?;
        match msg {
            Msg::Upload { digest } => {
                self.seen.uploaded.insert(digest);
                if self.stage(now, from, digest) {
                    return None;
                }
                let up = self.up.as_mut()?;
                let seq = up
                    .next_object
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let object = ObjectId::new(up.writer, seq);
                up.uploads.insert(object, (from.clone(), digest));
                let tag = T_PUT + self.puts_sent;
                self.puts_sent += 1;
                up.deadlines.insert(tag, object);
                let put = Msg::Put { object, digest };
                self.out.push(Output::Send {
                    to: self.store.clone(),
                    msg: put,
                });
                self.timer(PUT_TIMEOUT, tag);
                self.reach(now, StorePoint::PutSent);
            }
            Msg::PutFailed { object, digest } => {
                let up = self.up.as_mut()?;
                if let Some((client, _)) = up.uploads.remove(&object) {
                    self.seen.put_failed.insert(object);
                    let msg = Msg::UploadFailed { digest };
                    self.out.push(Output::Send { to: client, msg });
                }
            }
            Msg::PutDone { object, digest } => {
                let (client, _) = self.up.as_mut()?.uploads.remove(&object)?;
                if self.reach(now, StorePoint::BeforeCommit) {
                    return None;
                }
                let planted = self.arm_in_commit();
                let location = Location {
                    store: StoreId::new(0),
                    object,
                    offset: 0,
                };
                let put = Command::PutBlob { digest, location };
                let up = self.up.as_mut()?;
                let applied = up.commit(&self.out, put);
                // The planted crash fired in this commit if the commit failed with it
                // still pending; one that this commit did not reach is no cut of it.
                let failed = up.failed_at.is_some();
                if let (Some((point, down)), true, Some(_)) = (planted, failed, self.armed) {
                    self.seen.crashed_at.push((now, point));
                    let unsynced = self.disk.unsynced();
                    self.seen.cuts.push(Cut {
                        object,
                        digest,
                        down,
                        unsynced,
                        survived: None,
                    });
                }
                if let Some(Applied::Blob(Ok(_))) = applied {
                    if self.reach(now, StorePoint::BeforeReply) {
                        return None;
                    }
                    self.seen.acked.push((now, digest, object));
                    let msg = Msg::Uploaded { digest };
                    self.out.push(Output::Send { to: client, msg });
                }
            }
            other => return Some(other),
        }
        None
    }

    /// A PUT's deadline has passed: if the store has not answered it, the upload is
    /// answered `UNAVAILABLE` and a later answer is ignored. (A deadline set by an
    /// earlier start finds nothing.)
    pub(super) fn put_deadline(&mut self, now: FarmTime, tag: u64) {
        if self.flush_deadline(now, tag) {
            return;
        }
        let Some(up) = self.up.as_mut() else { return };
        let Some(object) = up.deadlines.remove(&tag) else {
            return;
        };
        if let Some((client, digest)) = up.uploads.remove(&object) {
            self.seen.timed_out.insert(object);
            let msg = Msg::UploadFailed { digest };
            self.out.push(Output::Send { to: client, msg });
        }
    }

    /// The write path reaches `point`: whether an armed crash fires there.
    fn reach(&mut self, now: FarmTime, point: StorePoint) -> bool {
        let Some((point, how)) = self.hooks.reached(point) else {
            return false;
        };
        self.seen.crashed_at.push((now, point));
        self.dying = Some((self.out.len(), how));
        true
    }

    /// A crash armed part-way through the next `PutBlob` commit is planted in the disk,
    /// at the commit's `after`-th filesystem operation: the point and how the server
    /// goes down there. It is recorded once it fires.
    fn arm_in_commit(&mut self) -> Option<(StorePoint, Down)> {
        let armed = self.hooks.peek().find(|(p, _, _)| p.in_commit().is_some());
        let (point, _, _) = armed?;
        let (point, how) = self.hooks.reached(point)?;
        let after = point.in_commit().unwrap_or_default();
        self.disk.fail_at(self.disk.ops() + after, Fault::Crash);
        self.armed = Some(how);
        Some((point, how))
    }
}

/// At a start (`up`, before it serves): whether the index the log was opened to has
/// each cut commit's blob in the cut's object (the commit was durable) or not (it was
/// lost whole).
pub(super) fn read_back_cuts(up: &Up, cuts: &mut [Cut]) {
    for cut in cuts.iter_mut().filter(|c| c.survived.is_none()) {
        let digest = cut.digest;
        let answer = up.query(move |m| m.blob(&digest));
        let held =
            matches!(answer, BlobAnswer::Present { location: at, .. } if at.object == cut.object);
        cut.survived = Some(held);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kbf_sim::{Event, NodeInput};
    use kbf_types::{Digest, StateMachine};

    use super::*;
    use crate::hook::Down;
    use crate::server::{ServerPlan, T_START};

    fn input(at: u64, event: Event<Msg>) -> NodeInput<Msg> {
        NodeInput {
            now: FarmTime::from_millis(at),
            entropy: 9,
            event,
        }
    }

    /// `Msg` has no `PartialEq` (its digests do, its node names do): compare text.
    fn debug<T: std::fmt::Debug>(v: &[T]) -> String {
        format!("{v:?}")
    }

    fn sends(s: &mut Server) -> Vec<Msg> {
        s.take_outputs()
            .into_iter()
            .filter_map(|o| match o {
                Output::Send { msg, .. } => Some(msg),
                Output::Timer { .. } => None,
            })
            .collect()
    }

    fn message(at: u64, from: &str, msg: Msg) -> NodeInput<Msg> {
        input(
            at,
            Event::Message {
                from: NodeId::from(from),
                msg,
            },
        )
    }

    fn present(s: &Server, d: &Digest) -> bool {
        let meta = s.meta().expect("up");
        matches!(meta.blob(d), kbf_meta::BlobAnswer::Present { .. })
    }

    /// Catches a PUT that is never given up (a hung store holding every upload until it
    /// returns), a deadline that does not answer the client, a late answer indexed
    /// after the client was told `UNAVAILABLE`, a refused PUT indexed or acknowledged,
    /// and a deadline from an earlier start that times out a later PUT.
    #[test]
    fn a_put_refused_or_past_its_deadline_is_unavailable_and_indexes_nothing() {
        let d = crate::input_of(&crate::action(1));
        let plan = ServerPlan {
            faults: Vec::new(),
            down_for: Duration::from_secs(3),
            tier: None,
        };
        let mut s = Server::new(plan, NodeId::from("store"));
        s.apply(input(0, Event::Start));
        s.take_outputs();
        s.apply(message(10, "client-0", Msg::Upload { digest: d }));
        // The first start's writer epoch is 1; its first object is 0.
        let object = ObjectId::new(kbf_meta::Epoch::new(1), 0);
        let put = [
            Output::Send {
                to: NodeId::from("store"),
                msg: Msg::Put { object, digest: d },
            },
            Output::Timer {
                after: PUT_TIMEOUT,
                tag: T_PUT,
            },
        ];
        assert_eq!(debug(&s.take_outputs()), debug(&put));
        let failed = [Msg::UploadFailed { digest: d }];
        s.apply(input(30_010, Event::Timer { tag: T_PUT }));
        assert_eq!(debug(&sends(&mut s)), debug(&failed));
        s.apply(message(30_020, "store", Msg::PutDone { object, digest: d }));
        assert!(sends(&mut s).is_empty());
        assert!(!present(&s, &d));
        // A refused PUT.
        s.apply(message(30_030, "client-0", Msg::Upload { digest: d }));
        let object = ObjectId::of(object.writer(), object.seq() + 1);
        sends(&mut s);
        s.apply(message(
            30_040,
            "store",
            Msg::PutFailed { object, digest: d },
        ));
        assert_eq!(debug(&sends(&mut s)), debug(&failed));
        assert!(!present(&s, &d));
        // After a restart, the first start's second deadline finds nothing.
        s.down(Down::Kill);
        s.apply(input(40_000, Event::Timer { tag: T_START }));
        s.take_outputs();
        s.apply(message(40_010, "client-0", Msg::Upload { digest: d }));
        sends(&mut s);
        s.apply(input(60_030, Event::Timer { tag: T_PUT + 1 }));
        assert!(
            sends(&mut s).is_empty(),
            "an earlier start's deadline answered"
        );
        assert_eq!(s.puts_sent, 3);
    }
}

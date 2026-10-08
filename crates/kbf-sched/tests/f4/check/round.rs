//! One placement round, checked against the reference first fit and verdict.

use super::*;

impl Checker {
    /// One placement round, checked against the reference (I1, I6 to I8, I10, I11, L2).
    pub(super) fn round(&mut self, effects: &[Effect]) {
        let now = self.now;
        let (mut live, mut cordoned) = (Vec::new(), Vec::new());
        for (name, w) in &self.workers {
            if w.alive(now) {
                if self.cordons.contains_key(name) {
                    cordoned.push(name);
                } else {
                    live.push(name);
                }
            }
        }
        let caps = |names: &[&WorkerId]| -> Vec<(&NodeCaps, Resources)> {
            names
                .iter()
                .map(|n| (&self.workers[*n].caps, self.workers[*n].capacity))
                .collect()
        };
        let (live_caps, cordoned_caps) = (caps(&live), caps(&cordoned));
        let mut matching: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut verdicts: BTreeMap<(usize, Resources), Verdict> = BTreeMap::new();
        let mut no_room: BTreeSet<(usize, Resources)> = BTreeSet::new();
        let mut extra: BTreeMap<usize, Resources> = BTreeMap::new();
        let mut grants: Vec<(OperationId, WorkerId)> = Vec::new();
        let mut notes: Vec<(OperationId, Verdict)> = Vec::new();
        let mut room_short = false;
        for &(_, id) in &self.queue {
            let op = &self.ops[usize::try_from(id.0).expect("ids fit")];
            let class = (op.needs, op.request.resources);
            let needs = &self.needs[op.needs];
            let mut placed = false;
            if grants.len() < PLACEMENT_ROUND && !no_room.contains(&class) {
                let m = matching.entry(op.needs).or_insert_with(|| {
                    (0..live.len())
                        .filter(|&i| needs.matches(&self.workers[live[i]].caps))
                        .collect()
                });
                let fit = m.iter().copied().find(|i| {
                    let w = &self.workers[live[*i]];
                    let booked = add(w.booked, extra.get(i).copied().unwrap_or_default());
                    fits(sub(w.capacity, booked), op.request.resources)
                });
                match fit {
                    Some(i) => {
                        let e = extra.entry(i).or_default();
                        *e = add(*e, op.request.resources);
                        grants.push((id, live[i].clone()));
                        placed = true;
                    }
                    None => {
                        no_room.insert(class);
                    }
                }
            }
            let verdict = if placed {
                Verdict::Servable
            } else {
                *verdicts.entry(class).or_insert_with(|| {
                    verdict(&live_caps, &cordoned_caps, needs, op.request.resources)
                })
            };
            room_short |= !placed && verdict == Verdict::Servable;
            if op.wait.is_some() || verdict != Verdict::Servable {
                notes.push((id, verdict));
            }
        }
        if room_short {
            self.hit("servable work waits for room");
        }
        if grants.len() == PLACEMENT_ROUND {
            self.hit("full round (PLACEMENT_ROUND grants)");
        }

        // Split what the round emitted.
        let mut told: BTreeMap<OperationId, Option<String>> = BTreeMap::new();
        let mut refused: BTreeMap<OperationId, String> = BTreeMap::new();
        let mut granted: Vec<&LeaseGrant> = Vec::new();
        for effect in effects {
            match effect {
                Effect::Waiting(w) => {
                    if told.insert(w.operation, w.reason.clone()).is_some() {
                        self.violated("I10", format!("{}: told twice in one round", w.operation));
                    }
                }
                Effect::Commit(ControlRecord::Refusal(r)) => {
                    refused.insert(r.operation, r.reason.clone());
                }
                Effect::Commit(ControlRecord::Lease(g)) => granted.push(g),
                other => self.violated("I2", format!("a placement round emitted {other:?}")),
            }
        }

        // Grants: the explicit invariants first, then the reference first fit.
        let mut booked_now: BTreeMap<WorkerId, Resources> = BTreeMap::new();
        for g in &granted {
            if self.next_seq != g.lease.seq || g.lease.term != TERM {
                self.violated(
                    "I1",
                    format!("{} granted; next lease is seq {}", g.lease, self.next_seq),
                );
            }
            self.next_seq += 1;
            let Some(w) = self.workers.get(&g.worker) else {
                self.violated("I7", format!("{g:?} to an unregistered worker"));
            };
            let op = self.op(g.operation);
            let booked = booked_now.entry(g.worker.clone()).or_insert(w.booked);
            *booked = add(*booked, op.request.resources);
            if !fits(w.capacity, *booked) {
                self.violated(
                    "I6",
                    format!("{g:?} books {booked:?} on capacity {:?}", w.capacity),
                );
            }
            if !w.alive(now) || !self.needs[op.needs].matches(&w.caps) {
                self.violated(
                    "I7",
                    format!("{g:?}: worker silent or its caps do not satisfy the platform"),
                );
            }
            if self.cordons.contains_key(&g.worker) {
                self.violated("I8", format!("{g:?} to a cordoned worker"));
            }
        }
        let got: Vec<(OperationId, WorkerId)> = granted
            .iter()
            .map(|g| (g.operation, g.worker.clone()))
            .collect();
        if got != grants {
            self.violated(
                "I11",
                format!("placement differs from the reference first fit\n  expected {grants:?}\n  got      {got:?}"),
            );
        }

        // Verdicts: who is told what, and who is refused.
        let wait_ms = self.wait_ms;
        for (id, v) in notes {
            let reason = told.remove(&id);
            let wait = self.op(id).wait;
            if v == Verdict::Servable {
                if reason != Some(None) {
                    self.violated(
                        "I10",
                        format!("{id} is servable again but its callers got {reason:?}"),
                    );
                }
                let op = self.op_mut(id);
                op.wait = None;
                op.told = None;
                self.hit("servable again");
                continue;
            }
            let since = match wait {
                Some(w) if w.verdict.refusable() && v.refusable() => w.since,
                _ => now,
            };
            let changed = wait.is_none_or(|w| w.verdict != v);
            if wait.is_some_and(|w| w.verdict == Verdict::Cordoned) && v.refusable() {
                self.hit("a wait for a cordon became refusable");
            }
            let text = match reason {
                Some(Some(text)) if v.states(&text) => Some(text),
                None if !changed => None,
                other => {
                    self.violated("I10", format!("{id} waits ({v:?}); callers told {other:?}"))
                }
            };
            if v == Verdict::Cordoned {
                self.hit("waits for a cordon");
            }
            let op = self.op_mut(id);
            op.wait = Some(Wait { since, verdict: v });
            if let Some(text) = text {
                op.told = Some(text);
            }
            if v.refusable() && now >= since + wait_ms {
                let reason = format!(
                    "{} (waited {} s for a worker that can run it)",
                    op.told.as_deref().unwrap_or_default(),
                    wait_ms / 1_000
                );
                let qos = op.request.qos.clone();
                self.queue.remove(&(Reverse(qos), id));
                if refused.remove(&id).as_ref() != Some(&reason) {
                    self.violated(
                        "I10/L2",
                        format!("{id} due for refusal with {reason:?}; not refused so"),
                    );
                }
            }
        }
        if let Some((id, reason)) = told.into_iter().next() {
            self.violated(
                "I10",
                format!("{id} told {reason:?} with no verdict to tell"),
            );
        }
        if let Some((id, reason)) = refused.into_iter().next() {
            self.violated(
                "I10/L2",
                format!("{id} refused early or without a stated reason: {reason:?}"),
            );
        }

        for (g, (id, _)) in granted.iter().zip(&grants) {
            let resources = self.op(*id).request.resources;
            let w = self.workers.get_mut(&g.worker).expect("checked");
            w.booked = add(w.booked, resources);
            w.leases.insert(g.lease);
            self.touched_workers.insert(g.worker.clone());
            let qos = self.op(*id).request.qos.clone();
            self.queue.remove(&(Reverse(qos), *id));
            self.op_mut(*id).holding = Some(g.lease);
            self.leases.insert(
                g.lease,
                Lease {
                    op: *id,
                    worker: g.worker.clone(),
                    committed: false,
                    running: false,
                    sent: None,
                },
            );
            self.hit("granted");
        }
    }
}

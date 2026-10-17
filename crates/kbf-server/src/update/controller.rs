//! The controller: reads the `update/*` rows and the farm, moves every host in a step with
//! [`step`], starts hosts with [`pick`], and gives the orders (`docs/design/split-mode-updates.md`
//! sections 2.1, 2.6, 5.3 and 6.1). One call, [`Controller::tick`], is one look; the caller
//! makes it every heartbeat. The controller keeps no state a restart would lose: the rows are
//! the state, and what it holds in memory (which orders to give again, which alerts it has
//! sent) only saves work.
//!
//! **Write ahead.** A host's move is written to `update/host/<id>` with a compare-and-set
//! before the orders it calls for are given, so a controller that dies between the two
//! resumes from the row: its first tick gives the orders of every row it finds again
//! ([`super::HostRecord::entry_commands`]), and each is repeatable ([`super::Transport`]).
//! **The one exception is a host that leaves its step**: a give-back, a hold, and the move
//! that finishes. The row says the host is out of a step only once the orders that undo the
//! step (undo the mark, uncordon, confirm a healthy binary) have been given, because the row
//! of a host that is out of a step is not read again for them: written first, a crash would
//! leave the node cordoned and marked with no row to say so. If one of those orders fails
//! the row is not written, and the next look decides the same move again. The slot and the
//! alert follow the row; an alert is lost if the controller dies between the two. A held
//! run's alert is sent again by the first look of the next controller, a give-back's is not.
//!
//! **Never overwrite.** Every row write names the version it read. When another controller
//! moved the row the write is refused ([`Tick::conflict`]), the tick ends without giving
//! that move's orders, and the next one reads again. The places of `update/slot` are claimed
//! and released by re-reading and trying again, never by writing the value read earlier; a
//! place freed because its host is in no step is freed by the claim the look saw
//! ([`Rows::release_claim`]), so a claim made since stands. The slot is read before the rows,
//! so a place it shows is never judged by rows older than it.
//!
//! **A controller that was taken over.** A store handle another writer has fenced out fails
//! its next write, but some orders follow no write: the replay of the rows a restart found,
//! an order that failed and is given again, and the re-assert of a row that did not change.
//! Each is preceded by [`Rows::check_writer`], a transaction that changes nothing and fails
//! once the handle is fenced, so a controller that was taken over gives no order.
//!
//! **Orders.** An activation order is drawn from the store's counter ([`Rows::mint_order`])
//! when the pure step asks for one, so two controllers over one store never draw the same
//! number, and the number is the row's from then on. The epoch it carries is the one the
//! controller was given, and a row that holds an order of a later epoch stops the
//! controller ([`ControllerError::EpochBehind`]): see [`Controller::new`].
//!
//! **A run's commit stays with its hosts.** A host is stepped toward the commit of the run
//! that started it ([`HostRow::commit`]). When a higher generation is offered (a rollback), a
//! host that has done nothing irreversible (`Selected` to `Gated`) is given back, so the
//! release being rolled back is not activated on it ([`super::GiveBack::Superseded`]); a host
//! past the activate order finishes the step it began. The new generation is carried out by
//! hosts started after it, and walks the finished ones back.
//!
//! **Time.** `now` is stored in rows (`since`, `set_at`), so its origin must outlive
//! the process: milliseconds on a clock that a restart does not reset, such as the wall
//! clock. A clock that reads earlier than a stored time stops the controller
//! ([`ControllerError::ClockBehind`]): every limit is `now` less a stored time, and against
//! a clock that is behind it would never pass.
//!
//! **What is not here.** The farm: the traits [`Transport`] and [`FactsSource`] are all the
//! controller knows of it, and nothing calls it yet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kbf_map::MemberId;
use kbf_state::Store;
use kbf_types::{FarmTime, WorkerId};

use super::codec::{Desired, HostRow};
use super::command::{Alert, Command, SlotOp};
use super::facts::{Facts, OrderId};
use super::moves::{acts_on_the_farm, decide, next_row, split};
use super::node_step::{HostPhase, Transition};
use super::pick::{Pick, pick};
use super::policy::Policy;
use super::ports::{FactsSource, Transport, TransportError};
use super::report::{CommandError, ControllerError, Moved, Tick};
use super::rows::{RowError, Rows, SLOT, Stored, host_key};
use super::slot::Slot;

/// Whether the rest of the tick goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

/// One tick's working state: the world as read, kept up to date with the controller's own
/// writes.
struct Pass {
    now: FarmTime,
    desired: Desired,
    policy: Policy,
    facts: Facts,
    /// Every host's row. After a write the version is unknown (`None`), which a second write
    /// would be refused on: a host is moved at most once in a tick.
    rows: BTreeMap<WorkerId, Stored<HostRow>>,
    touched: BTreeSet<WorkerId>,
    report: Tick,
}

impl Pass {
    fn member_of(&self, host: &WorkerId) -> Option<MemberId> {
        self.facts.host(host).and_then(|h| h.member)
    }

    /// Records a row the controller wrote.
    fn wrote(&mut self, host: &WorkerId, from: HostPhase, row: HostRow) {
        if from != row.record.phase {
            self.report.moved.push(Moved {
                host: host.clone(),
                from,
                to: row.record.phase.clone(),
            });
        }
        if let Some(facts) = self.facts.hosts.iter_mut().find(|h| h.id == *host) {
            facts.record = row.record.clone();
        }
        self.rows.insert(
            host.clone(),
            Stored {
                value: row,
                version: None,
            },
        );
        self.touched.insert(host.clone());
    }
}

/// The controller of a rolling update.
pub struct Controller {
    rows: Rows,
    epoch: u64,
    transport: Box<dyn Transport>,
    source: Box<dyn FactsSource>,
    /// Whether the first tick has given the orders of the rows it found.
    resumed: bool,
    /// Hosts an order did not reach: their row's orders are given again.
    redo: BTreeSet<WorkerId>,
    /// The alerts sent for a host's current phase.
    told: BTreeMap<WorkerId, Vec<Alert>>,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controller")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl Controller {
    /// A controller over `store`. `epoch` is the store's epoch: **it must be greater than
    /// every epoch an earlier controller of this store, or of a store this one was restored
    /// over, drew orders under.** Order ids are `(epoch, number)` and the number comes from a
    /// counter that a restore sets back, so the epoch is what keeps a restored store from
    /// issuing an id a host's helper has already seen. The SQLite store moves its epoch past
    /// both files' on a restore (`kbf_state::sqlite::SqliteStore::epoch`); a store that is
    /// never restored may keep one epoch, because the counter then goes on where it was.
    /// Every look checks the rows against it: an order in the slot or in a host's row of a
    /// later epoch stops the controller. That catches an epoch that is too low for the rows
    /// as they stand; it cannot know of a restore that dropped the rows which carried the
    /// higher epoch, so the epoch must come from a store that moves it on a restore.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        epoch: u64,
        transport: Box<dyn Transport>,
        source: Box<dyn FactsSource>,
    ) -> Self {
        Self {
            rows: Rows::new(store),
            epoch,
            transport,
            source,
            resumed: false,
            redo: BTreeSet::new(),
            told: BTreeMap::new(),
        }
    }

    /// One look: reads the rows and the farm; gives again the orders of the rows a restart
    /// found (the first tick) and of hosts an order did not reach; frees places nobody holds;
    /// moves every host in a step; and, unless the run is paused, starts the hosts the
    /// picker names.
    ///
    /// Nothing is done without a run (`update/desired`).
    ///
    /// # Errors
    /// [`ControllerError`]: a row or the farm could not be read, a row cannot be written, or
    /// the policy cannot be kept. Moves written before the error stand.
    pub fn tick(&mut self, now: FarmTime) -> Result<Tick, ControllerError> {
        let Some(mut pass) = self.read(now)? else {
            return Ok(Tick::default());
        };
        let again = if self.resumed {
            std::mem::take(&mut self.redo)
        } else {
            self.redo.clear();
            pass.rows
                .iter()
                .filter(|(_, row)| row.value.record.phase.holds_place())
                .map(|(id, _)| id.clone())
                .collect()
        };
        if let Err(error) = self.replay(&mut pass, &again) {
            // Give them again at the next tick; every order is repeatable.
            self.redo.extend(again);
            return Err(error);
        }
        self.resumed = true;
        self.free_unheld_places(&mut pass)?;
        let moving: Vec<WorkerId> = pass
            .rows
            .iter()
            .filter(|(id, row)| {
                row.value.record.phase.holds_place() && pass.facts.host(id).is_some()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in moving {
            if self.advance(&mut pass, &id)? == Flow::Stop {
                return Ok(pass.report);
            }
        }
        if !pass.desired.paused && pass.report.missing.is_empty() {
            self.start(&mut pass)?;
        }
        Ok(pass.report)
    }

    /// Reads the rows and the farm into one pass; `None` without a run.
    fn read(&mut self, now: FarmTime) -> Result<Option<Pass>, ControllerError> {
        let Some(desired) = self.rows.desired()?.value else {
            return Ok(None);
        };
        let policy = self.rows.policy()?;
        policy.validate()?;
        // The slot before the rows: a place is claimed after its host's row is written, so a
        // holder the slot shows has a row at least as new as the slot. Read the other way
        // round, a claim made between the two reads would show as a place held by a host
        // that the older rows say is in no step, and [`Controller::free_unheld_places`]
        // would free it.
        let slot = self.rows.slot()?.value;
        let mut rows = self.rows.hosts()?;
        self.check(now, &desired, &slot, &rows)?;
        let view = self.source.look(now)?;
        let missing: Vec<WorkerId> = rows
            .iter()
            .filter(|(id, row)| {
                row.value.record.phase.holds_place() && !view.hosts.iter().any(|h| h.id == **id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for host in &view.hosts {
            rows.entry(host.id.clone()).or_insert_with(|| Stored {
                value: HostRow::unstarted(),
                version: None,
            });
        }
        let wait_since = rows
            .values()
            .map(|row| &row.value.record)
            .filter(|r| {
                r.generation == desired.generation
                    && matches!(r.phase, HostPhase::RejoiningWork | HostPhase::Done)
            })
            .map(|r| r.since)
            .fold(desired.set_at, FarmTime::max);
        let mut hosts = view.hosts;
        for host in &mut hosts {
            host.record = rows[&host.id].value.record.clone();
        }
        let facts = Facts {
            target: desired.commit.clone(),
            generation: desired.generation,
            // Drawn from the store when a move needs one ([`Controller::advance`]).
            next_order: OrderId::new(self.epoch, u64::MAX),
            hosts,
            members: view.members,
            slot,
            queued_classes: view.queued_classes,
            wait_since,
        };
        Ok(Some(Pass {
            now,
            desired,
            policy,
            facts,
            rows,
            touched: BTreeSet::new(),
            report: Tick {
                missing,
                ..Tick::default()
            },
        }))
    }

    /// Refuses a clock that is behind a time the rows hold, and an epoch below the epoch of
    /// an order they hold: the two things the rows depend on that nothing else checks.
    fn check(
        &self,
        now: FarmTime,
        desired: &Desired,
        slot: &Slot,
        rows: &BTreeMap<WorkerId, Stored<HostRow>>,
    ) -> Result<(), ControllerError> {
        let latest = rows
            .values()
            .map(|row| row.value.record.since)
            .fold(desired.set_at, FarmTime::max);
        if now < latest {
            return Err(ControllerError::ClockBehind {
                now_ms: now.as_millis(),
                latest_ms: latest.as_millis(),
            });
        }
        let in_slot = slot
            .holders()
            .map(|(_, holder)| (SLOT.to_owned(), holder.order.epoch));
        let in_rows = rows
            .iter()
            .map(|(id, row)| (host_key(id), row.value.record.order.epoch));
        match in_slot
            .chain(in_rows)
            .find(|(_, found)| *found > self.epoch)
        {
            Some((row, found)) => Err(ControllerError::EpochBehind {
                row,
                found,
                epoch: self.epoch,
            }),
            None => Ok(()),
        }
    }

    /// Gives again the orders of the phase each of `hosts` is in, and the alert of a held
    /// run, which a controller that died before sending it never did. A host the farm does
    /// not show yet keeps its turn for the look that shows it.
    fn replay(
        &mut self,
        pass: &mut Pass,
        hosts: &BTreeSet<WorkerId>,
    ) -> Result<(), ControllerError> {
        let (shown, missing): (Vec<&WorkerId>, Vec<&WorkerId>) = hosts
            .iter()
            .partition(|id| pass.facts.host(id).is_some() && pass.rows.contains_key(*id));
        self.redo.extend(missing.into_iter().cloned());
        if shown.is_empty() {
            return Ok(());
        }
        // These orders follow no write of this look, so nothing else would stop a controller
        // that another has taken over from giving them.
        self.rows.check_writer()?;
        for id in shown {
            let row = pass.rows[id].value.clone();
            let mut commands = row.record.entry_commands();
            if let HostPhase::Held { reason } = row.record.phase {
                commands.push(Command::Alert(Alert::Held(reason)));
            }
            self.run(pass, id, &commands, &row)?;
        }
        Ok(())
    }

    /// Frees the places whose holder is no longer in a step: a controller that died between
    /// the row that finished a host and the release of its place. Each is freed by the claim
    /// the look saw, so a place the host claimed again since stands.
    fn free_unheld_places(&self, pass: &mut Pass) -> Result<(), ControllerError> {
        let stale: Vec<(WorkerId, OrderId)> = pass
            .facts
            .slot
            .holders()
            .map(|(_, holder)| (holder.host.clone(), holder.order))
            .filter(|(host, _)| {
                !pass
                    .rows
                    .get(host)
                    .is_some_and(|row| row.value.record.phase.holds_place())
            })
            .collect();
        for (host, order) in stale {
            pass.facts.slot = self.rows.release_claim(&host, order)?;
        }
        Ok(())
    }

    /// The next move of a host in a step.
    fn advance(&mut self, pass: &mut Pass, id: &WorkerId) -> Result<Flow, ControllerError> {
        let stored = pass.rows[id].clone();
        let mut facts = pass.facts.clone();
        facts.target = stored.value.commit.clone();
        let mut moved = decide(&facts, id, &pass.policy, pass.now)?;
        if moved.order.is_some() {
            // The step asked for an order id: draw one, and ask again with it.
            let order = self.rows.mint_order(self.epoch)?;
            facts.next_order = order;
            moved = decide(&facts, id, &pass.policy, pass.now)?;
            if moved.order != Some(order) {
                return Err(ControllerError::Invariant(format!(
                    "{id}: the step named {:?}, not the order drawn for it",
                    moved.order
                )));
            }
        }
        self.apply(pass, id, &stored, &moved)
    }

    /// Starts the hosts the picker names, each as a move to `Selected`.
    fn start(&mut self, pass: &mut Pass) -> Result<(), ControllerError> {
        let hosts = match pick(&pass.facts, &pass.policy, pass.now) {
            Pick::Wait { why } => {
                pass.report.waiting = Some(why);
                return Ok(());
            }
            Pick::Hosts(hosts) => hosts,
        };
        for picked in hosts {
            if pass.touched.contains(&picked.host) {
                continue;
            }
            let stored = pass.rows[&picked.host].clone();
            if self.apply(pass, &picked.host, &stored, &Transition::select())? == Flow::Stop {
                return Ok(());
            }
            pass.report.started.push(picked.host);
        }
        Ok(())
    }

    /// Writes the move to the host's row, and gives its orders around the write: see the
    /// module docs.
    fn apply(
        &mut self,
        pass: &mut Pass,
        id: &WorkerId,
        stored: &Stored<HostRow>,
        moved: &Transition,
    ) -> Result<Flow, ControllerError> {
        let row = next_row(&stored.value, moved, pass.now, &pass.desired);
        let changed = row != stored.value;
        let (before, after) = split(&moved.commands, changed && !moved.next.in_step());
        // An order that no write of this look precedes (the orders that undo a step, and the
        // re-asserts of a row that did not change) would not be stopped by the store's fence:
        // a controller that another has taken over would give them.
        if !before.is_empty() || (!changed && after.iter().any(acts_on_the_farm)) {
            self.rows.check_writer()?;
        }
        if !before.is_empty() && !self.run(pass, id, &before, &row)? {
            return Ok(Flow::Go);
        }
        if changed {
            match self.rows.put_host(id, stored, &row) {
                Ok(()) => {}
                Err(RowError::Conflict(_)) => {
                    pass.report.conflict = true;
                    return Ok(Flow::Stop);
                }
                Err(other) => return Err(other.into()),
            }
            if row.record.phase != stored.value.record.phase {
                self.told.remove(id);
            }
            pass.wrote(id, stored.value.record.phase.clone(), row.clone());
        }
        self.run(pass, id, &after, &row)?;
        Ok(Flow::Go)
    }

    /// Gives `commands` in order. Returns false at the first the farm did not take; the host
    /// is then given its row's orders again at the next tick.
    fn run(
        &mut self,
        pass: &mut Pass,
        id: &WorkerId,
        commands: &[Command],
        row: &HostRow,
    ) -> Result<bool, ControllerError> {
        for command in commands {
            let given = match command {
                Command::CasSlot(SlotOp::Claim) => self.claim(pass, id).map(|()| Ok(()))?,
                Command::CasSlot(SlotOp::Release) => {
                    pass.facts.slot = self.rows.release(id)?;
                    Ok(())
                }
                Command::Cordon => self.transport.cordon(id),
                Command::Uncordon => self.transport.uncordon(id),
                Command::MarkDown => match pass.member_of(id) {
                    Some(member) => self.transport.mark_down(id, member),
                    None => Err(no_member(id)),
                },
                Command::Undown => match pass.member_of(id) {
                    Some(member) => self.transport.undown(id, member),
                    None => Err(no_member(id)),
                },
                Command::Activate { order } => self.transport.activate(id, *order, &row.commit),
                Command::Confirm { order } => self.transport.confirm(id, *order),
                Command::Alert(alert) => self.tell(id, alert),
            };
            if let Err(error) = given {
                pass.report.errors.push(CommandError {
                    host: id.clone(),
                    command: command.clone(),
                    error,
                });
                self.redo.insert(id.clone());
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Claims a place for `id`: nothing if it holds one or the class is full (the step then
    /// gives the host back). The claim is named by an order id, which is drawn only for a
    /// claim that could succeed: a host waiting for a place does not spend the counter at
    /// every look.
    fn claim(&self, pass: &mut Pass, id: &WorkerId) -> Result<(), ControllerError> {
        if pass.facts.slot.holds(id) {
            return Ok(());
        }
        let Some(class) = pass.facts.host(id).map(|h| h.slot_class()) else {
            return Ok(());
        };
        if !pass.facts.slot.has_room(class, pass.policy.max_hosts_out) {
            return Ok(());
        }
        let order = self.rows.mint_order(self.epoch)?;
        if let Some(slot) = self
            .rows
            .claim(class, id, order, pass.policy.max_hosts_out)?
        {
            pass.facts.slot = slot;
        }
        Ok(())
    }

    /// Sends an alert once per phase of the host.
    fn tell(&mut self, id: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        let told = self.told.entry(id.clone()).or_default();
        if told.contains(alert) {
            return Ok(());
        }
        self.transport.alert(id, alert)?;
        told.push(alert.clone());
        Ok(())
    }
}

fn no_member(host: &WorkerId) -> TransportError {
    TransportError::new(format!("{host} has no storage member to mark"))
}

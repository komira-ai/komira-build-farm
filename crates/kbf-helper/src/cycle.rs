//! One pass of the helper (`Helper::cycle`), and the verbs it carries out.
//!
//! The order within a pass is the safe one: finish a restart that is owed, revert a swap
//! whose window ran out, stage the newest offer, then act on the orders. Orders are acted
//! on in id order, once each: the id is written to `helper.json` before the file is
//! removed, so a crash between the two finds the order acknowledged and only removes it.
//! The links move before the service restarts and the restart is recorded as owed before it
//! is tried, so a crash anywhere leaves a state the next pass completes.

use std::io::Write;
use std::path::Path;

use kbf_host::{Exec, HostPath, Perms, Step};
use kbf_installer::CommitId;
use kbf_installer::activate::{self, Activated, Arm, Confirmed, Expiry};
use kbf_installer::inbox::{Entry, read_capped};
use kbf_installer::layout::{self, Layout, Platform};
use kbf_installer::refusal::Refusal;
use kbf_installer::release::ReleaseError;
use kbf_installer::stage::Staged;
use kbf_installer::state::{self, State, StateError};

use crate::memory::{Event, Memory, StagedFacts};
use crate::order::{self, Order, Verb};
use crate::restart::Restart;
use crate::status;

/// How the helper is set up on a host, by its own flags.
#[derive(Clone, Debug)]
pub struct Config {
    /// Root's directory: `status.json` goes here and `inbox/` (the service user's) is
    /// beneath it.
    pub update_dir: HostPath,
    pub restart: Restart,
    /// The window the boot check gives a swap to be confirmed (Linux).
    pub window_secs: u64,
    /// The starts a swap on trial gets (Linux).
    pub starts: u32,
}

/// Something the helper cannot go on from: the host's own files are wrong. A refused
/// offer or order is not one; it is recorded and reported.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("the host state: {0}")]
    State(#[from] StateError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("helper.json: {0}")]
    Memory(String),
}

/// A host's helper: its layout, how it is changed, and its flags.
pub struct Helper<E: Exec> {
    host: Layout,
    exec: E,
    cfg: Config,
    /// What the verbs printed; kept for the log, never trusted.
    log: Vec<u8>,
    /// The last `status.json` written.
    written: Vec<u8>,
}

/// What stages an inbox entry: the installer's `stage_from_inbox` on a host. A function
/// object, not a type parameter: each closure a test passed would be an instantiation of
/// every function that takes it, and the coverage ratchet counts the best one.
pub type Stager<'a, E> = &'a mut dyn FnMut(&mut E, &Path) -> Result<Staged, Refusal>;

fn memory_path(host: &Layout) -> HostPath {
    HostPath::new(&format!("{}/helper.json", host.state_dir().as_str()))
        .expect("the state directory and a plain name")
}

impl<E: Exec> Helper<E> {
    pub fn new(host: Layout, exec: E, cfg: Config) -> Helper<E> {
        Helper {
            host,
            exec,
            cfg,
            log: Vec::new(),
            written: Vec::new(),
        }
    }

    /// The host's executor, for a test to read what was done.
    pub fn exec(&self) -> &E {
        &self.exec
    }

    /// The host's executor, for a test to change how it answers.
    pub fn exec_mut(&mut self) -> &mut E {
        &mut self.exec
    }

    fn dir(&self, sub: &str) -> std::path::PathBuf {
        let name = format!("{}/{sub}", self.cfg.update_dir.as_str());
        self.host
            .join(&HostPath::new(&name).expect("the update directory and a plain name"))
    }

    fn load(&self) -> Result<Memory, HelperError> {
        let path = self.host.join(&memory_path(&self.host));
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| HelperError::Memory(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Memory::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn put(&mut self, path: HostPath, perms: u32, bytes: Vec<u8>) -> Result<(), HelperError> {
        self.exec.apply(&Step::WriteFile {
            path,
            perms: Perms::new(perms),
            owner: None,
            bytes,
        })?;
        Ok(())
    }

    fn save(&mut self, memory: &Memory) -> Result<(), HelperError> {
        let bytes = serde_json::to_vec(memory).expect("plain fields serialize");
        self.put(memory_path(&self.host), 0o600, bytes)
    }

    fn state(&self) -> Result<State, HelperError> {
        Ok(state::read_unlocked(&self.host)?.unwrap_or_default())
    }

    /// Run once at start: make the links agree with the state after an interrupted verb.
    /// A link that moved means the service may still run the old release: restart it.
    pub fn recover(&mut self) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        match activate::reconcile(&mut self.exec, &self.host, &mut self.log) {
            Ok(true) => memory.restart_due = true,
            Ok(false) => {}
            Err(e) => refuse(&mut memory, &e.to_string()),
        }
        self.save(&memory)
    }

    /// One pass. `now` is seconds since the epoch.
    pub fn cycle(&mut self, now: u64, stager: Stager<E>) -> Result<(), HelperError> {
        let mut memory = self.load()?;
        self.restart_if_owed(&mut memory)?;
        self.expire(now, &mut memory)?;
        self.offers(&mut memory, stager)?;
        self.orders(now, &mut memory)?;
        self.restart_if_owed(&mut memory)?;
        self.report(&memory)
    }

    fn restart_if_owed(&mut self, memory: &mut Memory) -> Result<(), HelperError> {
        if !memory.restart_due {
            return Ok(());
        }
        if let Err(e) = self.exec.apply(&self.cfg.restart.step()) {
            refuse(memory, &format!("the restart failed: {e}"));
            return self.save(memory);
        }
        memory.restart_due = false;
        self.save(memory)
    }

    fn expire(&mut self, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        // A swap whose window has run out is about to be reverted: owe the restart first,
        // so a kill between the links moving and the save below cannot lose it.
        let before = self.state()?;
        let due = before
            .pending_confirm
            .as_ref()
            .and_then(|p| p.deadline)
            .is_some_and(|d| now >= d);
        let prior = memory.restart_due;
        if due {
            memory.restart_due = true;
            self.save(memory)?;
        }
        let result = activate::expire(&mut self.exec, &self.host, now, &mut self.log);
        let moved = self.moved(&before)?;
        // `|`, not `||`: no short-circuit for a test to have to reach.
        memory.restart_due = prior | moved;
        match result {
            Ok(Expiry::Reverted(r)) => {
                memory.event = Event::Reverted;
                memory.refusal = format!(
                    "{} was not confirmed in time; back on {}",
                    short(&r.from),
                    r.to
                );
            }
            Ok(_) => {}
            Err(e) => refuse(memory, &e.to_string()),
        }
        self.save(memory)
    }

    /// Whether `current` is not what it was in `before`: the links followed the state, so
    /// the service runs a release other than the one the links name now.
    fn moved(&self, before: &State) -> Result<bool, HelperError> {
        Ok(self.state()?.current != before.current)
    }

    /// Stage the newest inbox entry, once.
    fn offers(&mut self, memory: &mut Memory, stager: Stager<E>) -> Result<(), HelperError> {
        let inbox = self.dir("inbox");
        let Ok(entry) = Entry::open(&inbox) else {
            return Ok(());
        };
        let names = match entry.names() {
            Ok(names) => names,
            Err(r) => {
                refuse(memory, &format!("{}: {r}", r.code()));
                return self.save(memory);
            }
        };
        let Some((name, commit)) = names.iter().filter_map(|n| offer_of(n)).max() else {
            return Ok(());
        };
        if memory.handled_entry == name {
            return Ok(());
        }
        let state = self.state()?;
        let installed = self
            .exec
            .stat(&layout::release_dir(&commit))
            .ok()
            .flatten()
            .is_some();
        if installed | (state.current.as_ref() == Some(&commit)) {
            memory.handled_entry = name;
            return self.save(memory);
        }
        match stager(&mut self.exec, &inbox.join(&name)) {
            Ok(staged) => {
                memory.event = Event::Idle;
                memory.refusal.clear();
                memory.staged = Some(facts(&staged));
            }
            Err(r) => {
                memory.event = Event::Refused;
                memory.refusal = format!("{}: {r}", r.code());
            }
        }
        memory.handled_entry = name;
        self.save(memory)
    }

    fn orders(&mut self, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        // `inbox` is opened without following a link and `orders` relative to it: a link
        // the service user puts in either place is refused, never followed.
        let Ok(inbox) = Entry::open(&self.dir("inbox")) else {
            return Ok(());
        };
        let Ok(dir) = inbox.sub("orders") else {
            return Ok(());
        };
        let names = match dir.names() {
            Ok(names) => names,
            Err(r) => {
                refuse(memory, &format!("{}: {r}", r.code()));
                return self.save(memory);
            }
        };
        // A name that is not an order's (a `.partial-` the client is writing, or junk) is left.
        let mut files: Vec<(u64, String)> = names
            .into_iter()
            .filter_map(|name| order::id_of(&name).map(|id| (id, name)))
            .collect();
        files.sort();
        for (id, name) in files {
            let read = dir
                .open_file(&name, order::MAX_BYTES)
                .and_then(|mut f| read_capped(&mut f, order::MAX_BYTES, &name));
            let parsed = read
                .map_err(|r| format!("{}: {r}", r.code()))
                .and_then(|bytes| order::parse(id, &bytes).map_err(|e| e.to_string()));
            match parsed {
                Ok(o)
                    if memory.acked_epoch != 0
                        && o.epoch > memory.acked_epoch.saturating_add(order::MAX_EPOCH_JUMP) =>
                {
                    refuse(
                        memory,
                        &format!(
                            "order {id} is from epoch {}, more than {} above the last acted on ({}): \
                             not acted on",
                            o.epoch,
                            order::MAX_EPOCH_JUMP,
                            memory.acked_epoch
                        ),
                    );
                }
                Ok(o) if o.epoch < memory.acked_epoch => {
                    refuse(
                        memory,
                        &format!(
                            "order {id} is from epoch {}, older than the last acted on ({})",
                            o.epoch, memory.acked_epoch
                        ),
                    );
                }
                // The same epoch and an id not above the last: a replay, dropped silently.
                Ok(o) if o.epoch == memory.acked_epoch && o.id <= memory.acked_order_id => {}
                Ok(o) => {
                    self.act(&o, now, memory)?;
                    memory.acked_order_id = o.id;
                    memory.acked_epoch = o.epoch;
                }
                // Not acknowledged: a garbage file must not move the floor.
                Err(why) => refuse(memory, &format!("order file {name}: {why}")),
            }
            self.save(memory)?;
            if let Err(r) = dir.remove_file(&name) {
                refuse(memory, &format!("{}: {r}", r.code()));
            }
        }
        self.save(memory)
    }

    /// Carry out one order; what came of it goes into `memory`. An activate or a revert
    /// moves the links, so the restart is owed (and saved) before the verb runs; a kill
    /// between the links and the next save then leaves the restart for the next pass.
    fn act(&mut self, order: &Order, now: u64, memory: &mut Memory) -> Result<(), HelperError> {
        let before = self.state()?;
        let prior = memory.restart_due;
        if order.verb != Verb::Confirm {
            memory.restart_due = true;
            self.save(memory)?;
        }
        let result = match order.verb {
            Verb::Activate => self.activate(order, now),
            Verb::Confirm => self.confirm(order),
            Verb::Revert => self.revert(),
        };
        let moved = self.moved(&before)?;
        match result {
            Ok(done) => {
                memory.event = done.event;
                memory.refusal = done.note;
                memory.restart_due = prior | done.restart | moved;
            }
            Err(why) => {
                refuse(memory, &why);
                memory.restart_due = prior | moved;
            }
        }
        Ok(())
    }

    fn arm(&self, order: &Order) -> Arm {
        match self.host.platform() {
            Platform::Linux => Arm::FromFirstStart {
                starts: self.cfg.starts,
            },
            Platform::MacOs => Arm::FromActivation {
                window_secs: u64::from(order.window_s)
                    .clamp(activate::WINDOW_MIN_SECS, activate::WINDOW_MAX_SECS),
            },
        }
    }

    fn activate(&mut self, order: &Order, now: u64) -> Result<Done, String> {
        let ordered = activate::Order {
            commit: order.commit.clone(),
            // The epoch is part of the name: a restored store counts ids again, and an activation
            // under the same id as a pending one would be taken for its repeat.
            order_id: format!("{}-{}", order.epoch, order.id),
            serial: order.id,
        };
        let arm = self.arm(order);
        match activate::activate(
            &mut self.exec,
            &self.host,
            &ordered,
            arm,
            now,
            &mut self.log,
        ) {
            Ok(Activated::Swapped | Activated::Repeat) => Ok(Done::restart()),
            Ok(Activated::AlreadyCurrent) => Ok(Done::quiet(Event::Idle)),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn confirm(&mut self, order: &Order) -> Result<Done, String> {
        let state = self.state().map_err(|e| e.to_string())?;
        // The activation's own id: the confirm order names the commit, the state names the swap.
        let activation = state
            .pending_confirm
            .as_ref()
            .filter(|p| p.commit == order.commit)
            .and_then(|p| p.order_id.clone())
            .unwrap_or_default();
        match activate::confirm(&self.host, &activation, &order.commit) {
            Ok(Confirmed::Kept | Confirmed::AlreadyKept) => Ok(Done::quiet(Event::Confirmed)),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn revert(&mut self) -> Result<Done, String> {
        match activate::revert(&mut self.exec, &self.host, &mut self.log) {
            Ok(r) => Ok(Done {
                event: Event::Reverted,
                note: format!("reverted by order: {} is current", r.to),
                restart: true,
            }),
            Err(e) => Err(release_err(&e)),
        }
    }

    fn report(&mut self, memory: &Memory) -> Result<(), HelperError> {
        let bytes = status::render(&self.state()?, memory);
        if bytes == self.written {
            return Ok(());
        }
        let path = HostPath::new(&format!("{}/status.json", self.cfg.update_dir.as_str()))
            .expect("the update directory and a plain name");
        self.put(path, 0o644, bytes.clone())?;
        self.written = bytes;
        Ok(())
    }

    /// Print what the installer verbs wrote to `out`.
    pub fn drain_log(&mut self, out: &mut dyn Write) {
        let _ = out.write_all(&std::mem::take(&mut self.log));
    }
}

/// What an order did.
struct Done {
    event: Event,
    note: String,
    restart: bool,
}

impl Done {
    fn restart() -> Done {
        Done {
            event: Event::Idle,
            note: String::new(),
            restart: true,
        }
    }

    fn quiet(event: Event) -> Done {
        Done {
            event,
            note: String::new(),
            restart: false,
        }
    }
}

fn refuse(memory: &mut Memory, why: &str) {
    memory.event = Event::Refused;
    memory.refusal = why.to_owned();
}

fn release_err(e: &ReleaseError) -> String {
    e.to_string()
}

fn short(c: &Option<CommitId>) -> String {
    c.as_ref()
        .map_or_else(|| "the release".to_owned(), |c| c.as_str()[..12].to_owned())
}

/// The commit an inbox entry directory is for: `<generation:020>-<commit>`.
fn offer_of(name: &str) -> Option<(String, CommitId)> {
    let (generation, commit) = name.split_once('-')?;
    if (generation.len() != 20) | !generation.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((name.to_owned(), CommitId::new(commit).ok()?))
}

fn facts(staged: &Staged) -> StagedFacts {
    let narrow = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    StagedFacts {
        commit: staged.commit.clone(),
        protocol: narrow(staged.protocol),
        min_protocol: narrow(staged.min_protocol),
        state_schema: narrow(staged.state_schema),
    }
}

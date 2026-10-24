//! The roll state machine.
//!
//! One roll at a time, over the hosts of a plan: a frontend first (the real health check refuses
//! one), then one host at a time.
//! Per host the turn is `pending`, `checking` (the node's identity, safe-to-stop and, for a
//! daemon host, the drain; nodes only), `installing`, `waiting_healthy`, `done`. Every phase
//! change is written to `roll.json` before the work of the new phase starts, so a restart
//! finds the host where its work began and never earlier.
//! The first failure stops the roll and no later host is asked about, let alone touched.
//!
//! A `mac-daemon` host takes the daemon's turn with one difference. Its `install` only starts
//! the apply on the Mac and answers `apply_started`; the roll marks the host `waiting_healthy`
//! and polls `query` every [`Timing::apply_poll`] until the apply is `done` with the target
//! installed, for at most [`Timing::apply_deadline`]. `failed:<reason>` and the deadline end
//! the roll with the node left drained and the reason named. Only then the daemon's health
//! hold, the undrain and the full health follow. A restart finds the host in `waiting_healthy`
//! and polls again; it installs again only from `installing`, and only if the host says no
//! apply is running and the target is not installed. A Mac that answers `already_installed`
//! (it runs the target) is not polled, but a restart in `waiting_healthy` cannot tell that
//! answer from an install, so it polls: a stale `failed` status ends the resumed roll at once,
//! and none ends it at the deadline, either way with the node left drained. A Mac that
//! answers `installed`, or a host of another role that answers `apply_started`, fails the
//! roll.
//!
//! The machine is deterministic: no clock but the [`Clock`] port, no randomness, no threads of
//! its own. [`Updater::drive`] runs one roll to the end on the caller's thread; the other
//! methods only read or write the record and return at once, so a caller may answer
//! `GET` and `abort` while another thread drives.
//!
//! Memory never runs ahead of disk: a change is applied to a copy, written, and kept only if
//! the write succeeded. A roll whose record cannot be written stops where it is
//! (`state_lost`) rather than act on a phase it could not remember.
//!
//! Nor does memory fall behind it. A save that fails may still have renamed the new record
//! into place (the directory fsync is after the rename), so after any save error the
//! updater is poisoned: every operation answers `state_lost` until [`Updater::reload`] (or a
//! new process) has read `roll.json` again and so knows what the disk says.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::api::{
    ApiError, ErrorCode, Failure, HostRoll, Hosts, Phase, Roll, RollRequest, RollState,
};
use crate::inventory::{Host, Inventory, Role};
use crate::ports::{
    Apply, Clock, Drain, Health, HealthCheck, InstallAnswer, Installed, Installer, PortError,
    SafeToStop, Stop,
};
use crate::record::{FORMAT, Record, RollStore, StoreError, roll_id};
use crate::timing::Timing;
use crate::version::Version;

/// The five ports of a roll.
pub struct Ports {
    pub safe: Box<dyn SafeToStop>,
    pub installer: Box<dyn Installer>,
    pub drain: Box<dyn Drain>,
    pub health: Box<dyn HealthCheck>,
    pub clock: Box<dyn Clock>,
}

/// `halt`, with the node it leaves drained named. A store error is not changed.
fn left_drained(host: &Host, halt: Halt) -> Halt {
    let node = host.node_id.as_deref().unwrap_or(&host.alias);
    let note = format!("node {node} ({}) is left drained", host.alias);
    noted(halt, &note)
}

/// `halt`, with `note` after its detail. A store error is not changed.
fn noted(halt: Halt, note: &str) -> Halt {
    match halt {
        Halt::Fail(code, detail) => Halt::Fail(code, format!("{detail}; {note}")),
        Halt::Interrupt(detail) => Halt::Interrupt(format!("{detail}; {note}")),
        Halt::Store(e) => Halt::Store(e),
    }
}

/// The longest detail kept, in characters.
const DETAIL_MAX: usize = 200;

/// One line of at most [`DETAIL_MAX`] characters: control characters become spaces, so a
/// port's text cannot forge a line of the audit log or carry a screenful of apt output.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(DETAIL_MAX)
        .collect()
}

/// What `POST` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub roll: Roll,
    /// False when the idempotency key named a roll that already exists (a 200, not a 202).
    pub created: bool,
}

pub(crate) struct State {
    pub(crate) rec: Option<Record>,
    /// A thread is inside `drive`.
    pub(crate) driving: bool,
    /// The roll was found `running` at start-up and has not been resumed yet.
    recovering: bool,
    /// A save failed, so the record on disk may be ahead of `rec`; see the module docs.
    poisoned: bool,
}

/// Installs of one host per roll: the first, and the one rerun after a restart.
const MAX_INSTALLS: u32 = 2;

/// Why an install call did not succeed.
enum InstallError {
    /// No answer: whether it ran is unknown.
    Unreachable,
    Halt(Halt),
}

/// Why a host's turn ended early.
enum Halt {
    Fail(ErrorCode, String),
    Interrupt(String),
    Store(ApiError),
}

impl From<ApiError> for Halt {
    fn from(e: ApiError) -> Halt {
        Halt::Store(e)
    }
}

pub struct Updater {
    inventory: Inventory,
    store: RollStore,
    ports: Ports,
    timing: Timing,
    state: Mutex<State>,
}

/// Clears `driving` when `drive` returns, however it returns.
struct Driving<'a>(&'a Updater);

impl Drop for Driving<'_> {
    fn drop(&mut self) {
        self.0.lock().driving = false;
    }
}

impl Updater {
    /// Open the updater over a store. Fails if `roll.json` exists and cannot be trusted:
    /// the updater does not start on a record it cannot read (fail closed). A roll left
    /// `running` is resumed by the next [`Updater::drive`] of its id.
    pub fn open(
        inventory: Inventory,
        store: RollStore,
        ports: Ports,
        timing: Timing,
    ) -> Result<Updater, StoreError> {
        let rec = store.load()?;
        if let Some(r) = &rec {
            // An explicit list must be the plan the inventory makes of it, in its order (the
            // record alone cannot tell which host is the frontend). A host that left the
            // inventory is the roll's business at that host, not a corrupt file.
            if let (Hosts::Aliases(_), Ok(plan)) =
                (&r.request.hosts, inventory.plan(&r.request.hosts))
            {
                let want: Vec<&str> = plan.iter().map(|h| h.alias.as_str()).collect();
                let have: Vec<&str> = r.roll.hosts.iter().map(|h| h.alias.as_str()).collect();
                if want != have {
                    return Err(StoreError::Inconsistent(format!(
                        "the roll's hosts {have:?} are not the plan {want:?}"
                    )));
                }
            }
        }
        let recovering = rec
            .as_ref()
            .is_some_and(|r| r.roll.state == RollState::Running);
        Ok(Updater {
            inventory,
            store,
            ports,
            timing,
            state: Mutex::new(State {
                rec,
                driving: false,
                recovering,
                poisoned: false,
            }),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuse while the record in memory may be behind the disk.
    fn check_live(st: &State) -> Result<(), ApiError> {
        if st.poisoned {
            return Err(ApiError::new(
                ErrorCode::StateLost,
                "a write of roll.json failed, so the updater no longer knows what it says; \
                 reload it or restart the updater",
            ));
        }
        Ok(())
    }

    /// Write `next` and, only if that worked, keep it. A failed write poisons the updater.
    fn commit(&self, st: &mut State, next: Record) -> Result<Roll, ApiError> {
        Updater::check_live(st)?;
        if let Err(e) = self.store.save(&next) {
            st.poisoned = true;
            return Err(ApiError::new(
                ErrorCode::StateLost,
                format!("roll.json cannot be written: {e}"),
            ));
        }
        let roll = next.roll.clone();
        st.rec = Some(next);
        Ok(roll)
    }

    /// Change the current record by `f`, durably.
    fn update(&self, f: impl FnOnce(&mut Record)) -> Result<Roll, ApiError> {
        let mut st = self.lock();
        let mut next = Updater::live_record(&st)?;
        f(&mut next);
        self.commit(&mut st, next)
    }

    fn live_record(st: &State) -> Result<Record, ApiError> {
        Updater::check_live(st)?;
        st.rec
            .clone()
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, "there is no roll"))
    }

    fn snapshot(&self) -> Result<Record, ApiError> {
        Updater::live_record(&self.lock())
    }

    /// The roll in memory, whether or not the updater is poisoned (a test of "memory never
    /// runs ahead of disk").
    #[cfg(test)]
    pub(crate) fn memory(&self) -> Option<Roll> {
        self.lock().rec.as_ref().map(|r| r.roll.clone())
    }

    /// The current (latest) roll, if any has run.
    pub fn current(&self) -> Result<Option<Roll>, ApiError> {
        let st = self.lock();
        Updater::check_live(&st)?;
        Ok(st.rec.as_ref().map(|r| r.roll.clone()))
    }

    pub fn get(&self, id: &str) -> Result<Roll, ApiError> {
        self.current()?
            .filter(|r| r.id == id)
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, format!("no roll {id}")))
    }

    /// Read `roll.json` again and make it the updater's state, clearing a poison. A running
    /// roll is then resumed by the next [`Updater::drive`] as after a restart. Refused (409)
    /// while a thread drives, and `state_lost` if the file cannot be trusted (the updater
    /// stays poisoned).
    pub fn reload(&self) -> Result<Option<Roll>, ApiError> {
        let mut st = self.lock();
        if st.driving {
            return Err(ApiError::new(
                ErrorCode::RollActive,
                "a roll is being driven; reload after it stops",
            ));
        }
        let rec = self.store.load().map_err(|e| {
            ApiError::new(
                ErrorCode::StateLost,
                format!("roll.json cannot be read: {e}"),
            )
        })?;
        st.recovering = rec
            .as_ref()
            .is_some_and(|r| r.roll.state == RollState::Running);
        st.rec = rec;
        st.poisoned = false;
        Ok(st.rec.as_ref().map(|r| r.roll.clone()))
    }

    /// Begin a roll: validate the aliases, apply the idempotency key and the one-roll rule,
    /// and write the record. Nothing runs until [`Updater::drive`].
    pub fn start(&self, req: &RollRequest) -> Result<Started, ApiError> {
        let mut st = self.lock();
        Updater::check_live(&st)?;
        if let Some(rec) = &st.rec {
            if rec.request.idempotency_key == req.idempotency_key {
                if rec.request == *req {
                    return Ok(Started {
                        roll: rec.roll.clone(),
                        created: false,
                    });
                }
                return Err(ApiError::new(
                    ErrorCode::KeyReused,
                    "this idempotency_key was used with a different body",
                )
                .with_roll(&rec.roll.id));
            }
            if !rec.roll.state.is_terminal() {
                return Err(ApiError::new(
                    ErrorCode::RollActive,
                    format!("roll {} is {:?}", rec.roll.id, rec.roll.state),
                )
                .with_roll(&rec.roll.id));
            }
        }
        let plan = self.inventory.plan(&req.hosts)?;
        let seq = st.rec.as_ref().map_or(1, |r| r.seq + 1);
        let next = Record {
            format: FORMAT,
            seq,
            request: req.clone(),
            roll: Roll {
                id: roll_id(seq),
                state: RollState::Running,
                hosts: plan
                    .iter()
                    .map(|h| HostRoll {
                        alias: h.alias.clone(),
                        phase: Phase::Pending,
                        attempts: 0,
                        detail: None,
                    })
                    .collect(),
                failure: None,
            },
            resumes: 0,
            abort_requested: false,
        };
        st.recovering = false;
        let roll = self.commit(&mut st, next)?;
        Ok(Started {
            roll,
            created: true,
        })
    }

    /// Abort a roll. A running roll that a thread is driving stops after the host in hand
    /// (between hosts) and ends `aborted`, also when that host was the last one (the abort
    /// was asked for before the roll was over); the flag is consumed by that. If every host
    /// is already done when the abort arrives, the reply says `done` and the roll ends
    /// `done`. A roll nobody drives, and an interrupted one, is aborted at once. A finished
    /// roll is a 409.
    pub fn abort(&self, id: &str) -> Result<Roll, ApiError> {
        let mut st = self.lock();
        let mut next = Updater::live_record(&st)?;
        if next.roll.id != id {
            return Err(ApiError::new(
                ErrorCode::UnknownRoll,
                format!("no roll {id}"),
            ));
        }
        if next.roll.state.is_terminal() {
            return Err(ApiError::new(
                ErrorCode::RollTerminal,
                format!("roll {id} is {:?}", next.roll.state),
            ));
        }
        if st.driving {
            if next.roll.hosts.iter().all(|h| h.phase == Phase::Done) {
                // Every host is finished and the driver is about to say so: too late, and
                // the reply says what the roll is.
                let mut roll = next.roll;
                roll.state = RollState::Done;
                return Ok(roll);
            }
            next.abort_requested = true;
        } else {
            next.roll.state = RollState::Aborted;
        }
        self.commit(&mut st, next)
    }

    /// Run the roll `id` to the end, or to the first failure, interruption or abort, and
    /// return it. A roll that is not running is returned as it is. The first `drive` after a
    /// restart resumes: it bumps the record's resume counter and picks each host up where its
    /// phase says (see the host's phase handling in `step`).
    pub fn drive(&self, id: &str) -> Result<Roll, ApiError> {
        {
            let mut st = self.lock();
            let rec = Updater::live_record(&st)?;
            if rec.roll.id != id {
                return Err(ApiError::new(
                    ErrorCode::UnknownRoll,
                    format!("no roll {id}"),
                ));
            }
            if rec.roll.state != RollState::Running {
                return Ok(rec.roll);
            }
            if st.driving {
                return Err(ApiError::new(
                    ErrorCode::RollActive,
                    format!("roll {id} is already being driven"),
                )
                .with_roll(id));
            }
            st.driving = true;
            if st.recovering {
                st.recovering = false;
                let mut next = rec;
                next.resumes += 1;
                if let Err(e) = self.commit(&mut st, next) {
                    st.driving = false;
                    return Err(e);
                }
            }
        }
        let _driving = Driving(self);
        loop {
            let rec = self.snapshot()?;
            let next = rec.roll.hosts.iter().position(|h| h.phase != Phase::Done);
            // The abort flag is consumed here, after the last host as after any other.
            if rec.abort_requested {
                return self.update(|r| {
                    r.abort_requested = false;
                    r.roll.state = RollState::Aborted;
                });
            }
            let Some(idx) = next else {
                return self.update(|r| r.roll.state = RollState::Done);
            };
            match self.step(idx, &rec.request.version) {
                Ok(()) => {}
                Err(Halt::Fail(code, detail)) => {
                    let detail = clean(&detail);
                    return self.update(|r| {
                        let alias = r.roll.hosts[idx].alias.clone();
                        r.roll.hosts[idx].phase = Phase::Failed;
                        r.roll.hosts[idx].detail = Some(detail.clone());
                        r.roll.state = RollState::Failed;
                        r.roll.failure = Some(Failure {
                            alias,
                            code,
                            detail,
                        });
                    });
                }
                Err(Halt::Interrupt(detail)) => {
                    let detail = clean(&detail);
                    return self.update(|r| {
                        r.roll.hosts[idx].detail = Some(detail);
                        r.roll.state = RollState::Interrupted;
                    });
                }
                Err(Halt::Store(e)) => return Err(e),
            }
        }
    }

    fn mark(&self, idx: usize, phase: Phase, attempt: bool) -> Result<(), ApiError> {
        self.update(|r| {
            let h = &mut r.roll.hosts[idx];
            h.phase = phase;
            h.detail = None;
            h.attempts += u32::from(attempt);
        })
        .map(drop)
    }

    /// One host's turn, from the phase the record holds. A phase reached here at the top of
    /// the loop is one a restart found: `pending` and `checking` begin (or repeat) the check,
    /// `installing` asks the host what happened to the install, `waiting_healthy` resumes
    /// the wait with a fresh bound, and a `failed` host is never resumed past.
    ///
    /// A daemon host's node is drained at the end of `checking` and given back after the host
    /// is healthy. From `installing` on the roll holds it, whether this process drained it or
    /// the one before a restart did: every failure on the way names the node and leaves it
    /// drained, for an operator to look at.
    fn step(&self, idx: usize, target: &Version) -> Result<(), Halt> {
        let rec = self.snapshot()?;
        let h = &rec.roll.hosts[idx];
        let host = self.inventory.host(&h.alias).ok_or_else(|| {
            Halt::Fail(
                ErrorCode::UnknownAlias,
                format!("host {} is no longer in the inventory", h.alias),
            )
        })?;
        let phase = h.phase;
        if phase == Phase::Failed {
            return Err(Halt::Fail(
                ErrorCode::StateLost,
                "the record holds a failed host on a running roll".to_owned(),
            ));
        }
        if matches!(phase, Phase::Pending | Phase::Checking) {
            // A host the health port will not judge is not installed on either: refused
            // before the installer, so that nothing on it changes.
            if let Some(why) = self.ports.health.refusal(host) {
                return Err(Halt::Fail(ErrorCode::SwapAbandoned, why));
            }
            if host.role != Role::Frontend {
                self.mark(idx, Phase::Checking, false)?;
                self.check_identity(host)?;
                self.check_safe(host)?;
                if host.role.is_daemon() {
                    self.drain(host)?;
                }
            }
        }
        let held = host.role.is_daemon();
        self.install_and_wait(idx, host, target, phase, held)
            .map_err(|halt| if held { left_drained(host, halt) } else { halt })?;
        if held {
            self.undrain(host)?;
            self.wait_healthy(host, target, false)?;
        }
        self.mark(idx, Phase::Done, false)?;
        Ok(())
    }

    /// The install of a host (or the check of what a restart left) and the wait for it to run
    /// the target. `held`: its node is drained, so it is not asked to be serving.
    ///
    /// A Mac host's install only starts its apply. The host is `waiting_healthy` from the
    /// answer on, and that phase begins with the wait for the apply ([`Updater::wait_apply`]),
    /// so a restart that finds it there polls again and never installs again.
    fn install_and_wait(
        &self,
        idx: usize,
        host: &Host,
        target: &Version,
        phase: Phase,
        held: bool,
    ) -> Result<(), Halt> {
        let mut apply = host.role == Role::MacDaemon;
        match phase {
            Phase::Pending | Phase::Checking => {
                self.mark(idx, Phase::Installing, true)?;
                match self.install(host, target) {
                    Ok(answer) => {
                        apply &= answer == InstallAnswer::ApplyStarted;
                        self.mark(idx, Phase::WaitingHealthy, false)?;
                    }
                    // No answer: the install may or may not have run. Ask, as after a restart.
                    Err(InstallError::Unreachable) => {
                        apply = self.recover_install(idx, host, target)?;
                    }
                    Err(InstallError::Halt(h)) => return Err(h),
                }
            }
            Phase::Installing => apply = self.recover_install(idx, host, target)?,
            Phase::WaitingHealthy | Phase::Done | Phase::Failed => {}
        }
        if apply {
            self.wait_apply(host, target)?;
        }
        self.wait_healthy(host, target, held)
    }

    /// The node the host's report names must be the one the inventory lists for it. A daemon
    /// host without a node id is refused: it cannot be told from another, and a drain is
    /// aimed by that id. A host that lists one has it checked, a daemon or not. Nothing has
    /// been drained when this fails.
    fn check_identity(&self, host: &Host) -> Result<(), Halt> {
        let fail = |why: String| Err(Halt::Fail(ErrorCode::NodeIdentity, why));
        let Some(listed) = host.node_id.as_deref() else {
            return if host.role.is_daemon() {
                fail(format!(
                    "{} is listed without a node id, so its node cannot be told from another; \
                     add the id from its kbf configuration to the inventory",
                    host.alias
                ))
            } else {
                Ok(())
            };
        };
        let report = self.ports.installer.report(host).map_err(|e| {
            Halt::Fail(
                ErrorCode::NodeIdentity,
                format!("{} could not be asked its node id: {e}", host.alias),
            )
        })?;
        match report.node_id.as_deref() {
            Some(found) if found == listed => Ok(()),
            Some(found) => fail(format!(
                "{} reports node id {found}, not the {listed} the inventory lists for it",
                host.alias
            )),
            None => fail(format!(
                "{} reports no node id, not the {listed} the inventory lists for it",
                host.alias
            )),
        }
    }

    /// Drain the host's node and wait until it holds no lease, or until the drain deadline
    /// has passed (its leases then run on and the install goes ahead). A failure leaves the
    /// node as the frontend has it, which may be drained.
    fn drain(&self, host: &Host) -> Result<(), Halt> {
        let fail = |why: &str, e: PortError| {
            left_drained(
                host,
                Halt::Fail(ErrorCode::DrainFailed, format!("{why}: {e}")),
            )
        };
        let deadline = self.timing.drain_deadline;
        self.ports
            .drain
            .drain(host, deadline)
            .map_err(|e| fail("the drain was refused", e))?;
        let start = self.now();
        loop {
            let held = self
                .ports
                .drain
                .leases(host)
                .map_err(|e| fail("the node's leases could not be read", e))?;
            if held == 0 || self.since(start) >= deadline {
                return Ok(());
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// Give the host's node back, now that the host runs the target.
    fn undrain(&self, host: &Host) -> Result<(), Halt> {
        self.ports.drain.undrain(host).map_err(|e| {
            left_drained(
                host,
                Halt::Fail(ErrorCode::UndrainFailed, format!("the undrain failed: {e}")),
            )
        })
    }

    fn now(&self) -> Duration {
        self.ports.clock.now()
    }

    fn since(&self, start: Duration) -> Duration {
        self.now().saturating_sub(start)
    }

    fn check_safe(&self, host: &Host) -> Result<(), Halt> {
        let start = self.now();
        loop {
            let (code, why) = match self.ports.safe.check(host) {
                Ok(Stop::Unknown) => {
                    return Err(Halt::Fail(
                        ErrorCode::UnknownMember,
                        format!("the cluster does not know {}", host.alias),
                    ));
                }
                Ok(Stop::Answer(v)) if v.safe => return Ok(()),
                Ok(Stop::Answer(v)) => (ErrorCode::UnsafeToStop, v.reasons.join("; ")),
                Err(e) => (ErrorCode::SafeToStopUnavailable, e.to_string()),
            };
            if self.since(start) >= self.timing.safe_bound {
                return Err(Halt::Fail(code, why));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// One install call. A Mac host's answer is `apply_started` or `already_installed`; a
    /// Linux host's is `installed` or `already_installed`. The other pairing is the host
    /// being not what the inventory says, and fails the roll.
    fn install(&self, host: &Host, target: &Version) -> Result<InstallAnswer, InstallError> {
        let answer = match self.ports.installer.install(host, target) {
            Ok(answer) => answer,
            Err(e) if e.is_unreachable() => return Err(InstallError::Unreachable),
            Err(e) => {
                return Err(InstallError::Halt(Halt::Fail(
                    ErrorCode::InstallFailed,
                    e.to_string(),
                )));
            }
        };
        let mac = host.role == Role::MacDaemon;
        let wrong = match answer {
            InstallAnswer::Installed if mac => Some("finished the install at once"),
            InstallAnswer::ApplyStarted if !mac => Some("started an apply"),
            _ => None,
        };
        match wrong {
            Some(did) => Err(InstallError::Halt(Halt::Fail(
                ErrorCode::InstallFailed,
                format!(
                    "{} is listed as a {} host but {did}",
                    host.alias,
                    if mac { "mac-daemon" } else { "non-Mac" }
                ),
            ))),
            None => Ok(answer),
        }
    }

    /// A host found `installing`: ask, do not install blindly. The target means the install
    /// finished; another version means it never started or did not finish, and one more
    /// install is safe (it is idempotent, and an older version is installed like any other);
    /// "in progress" waits it out; silence for [`Timing::query_bound`] interrupts the roll.
    ///
    /// A Mac whose apply is running has been installed on: the host is `waiting_healthy` and
    /// the apply is waited for there. The answer is whether the apply is still to be waited
    /// for (false after an `already_installed`).
    fn recover_install(&self, idx: usize, host: &Host, target: &Version) -> Result<bool, Halt> {
        let start = self.now();
        let mut last_answer = start;
        let mac = host.role == Role::MacDaemon;
        loop {
            match self.ports.installer.report(host) {
                Ok(report) if mac && report.apply == Some(Apply::Running) => {
                    self.mark(idx, Phase::WaitingHealthy, false)?;
                    return Ok(true);
                }
                Ok(report) => match report.installed {
                    Installed::Version(v) if v == target.as_str() => {
                        self.mark(idx, Phase::WaitingHealthy, false)?;
                        return Ok(mac);
                    }
                    // Not the target: the install did not run or did not finish. One more
                    // (section 13(e): rerun once); a host that is still not there after that
                    // is for an operator.
                    Installed::Version(v) => {
                        if let Some(apply) =
                            self.reinstall(idx, host, target, &format!("runs {v}"))?
                        {
                            return Ok(apply);
                        }
                    }
                    // A fresh host: nothing is older than the target, so it is installed.
                    Installed::NotInstalled => {
                        if let Some(apply) =
                            self.reinstall(idx, host, target, "has no kbf installed")?
                        {
                            return Ok(apply);
                        }
                    }
                    Installed::InProgress => {
                        last_answer = self.now();
                        if self.since(start) >= self.timing.install_bound {
                            return Err(Halt::Interrupt(format!(
                                "an install on {} was still in progress after {} s",
                                host.alias,
                                self.timing.install_bound.as_secs()
                            )));
                        }
                    }
                },
                Err(e) => {
                    if self.since(last_answer) >= self.timing.query_bound {
                        return Err(Halt::Interrupt(format!(
                            "{} did not answer for {} s: {e}",
                            host.alias,
                            self.timing.query_bound.as_secs()
                        )));
                    }
                }
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// One more install of a host that is not at the target, if the cap allows. `Ok(Some(_))`:
    /// it ran and the host is `waiting_healthy`, and the value is whether a Mac's apply is
    /// still to be waited for; `Ok(None)`: it got no answer, so the caller asks again. `state`
    /// says what the host was found as, for the interrupt text.
    fn reinstall(
        &self,
        idx: usize,
        host: &Host,
        target: &Version,
        state: &str,
    ) -> Result<Option<bool>, Halt> {
        let attempts = self.snapshot()?.roll.hosts[idx].attempts;
        if attempts >= MAX_INSTALLS {
            return Err(Halt::Interrupt(format!(
                "{} {state} and not {} after {attempts} install attempts",
                host.alias,
                target.as_str()
            )));
        }
        self.mark(idx, Phase::Installing, true)?;
        match self.install(host, target) {
            Ok(answer) => {
                self.mark(idx, Phase::WaitingHealthy, false)?;
                Ok(Some(
                    host.role == Role::MacDaemon && answer == InstallAnswer::ApplyStarted,
                ))
            }
            Err(InstallError::Unreachable) => Ok(None),
            Err(InstallError::Halt(h)) => Err(h),
        }
    }

    /// Wait for a Mac host's apply to be `done` for the target: ask `query` every
    /// [`Timing::apply_poll`] for [`Timing::apply_deadline`]. `failed:<reason>` ends the wait
    /// at once and names the reason. A host that does not answer, reports no apply, reports
    /// one still running, or is done but on another version is waited for until the deadline.
    fn wait_apply(&self, host: &Host, target: &Version) -> Result<(), Halt> {
        let start = self.now();
        let bound = self.timing.apply_deadline;
        loop {
            let last = match self.ports.installer.report(host) {
                Ok(report) => match (report.apply, report.installed) {
                    (Some(Apply::Failed(why)), _) => {
                        return Err(Halt::Fail(
                            ErrorCode::InstallFailed,
                            format!("the apply on {} failed: {why}", host.alias),
                        ));
                    }
                    (Some(Apply::Done), Installed::Version(v)) if v == target.as_str() => {
                        return Ok(());
                    }
                    (Some(Apply::Done), Installed::Version(v)) => {
                        format!("the apply is done but {} runs {v}", host.alias)
                    }
                    (Some(Apply::Done), _) => {
                        format!("the apply is done but {} reports no version", host.alias)
                    }
                    (Some(Apply::Running), _) => "the apply is running".to_owned(),
                    (None, _) => format!("{} reports no apply", host.alias),
                },
                Err(e) => e.to_string(),
            };
            if self.since(start) >= bound {
                return Err(Halt::Fail(
                    ErrorCode::HealthTimeout,
                    format!(
                        "the apply on {} was not done for {} after {} s: {last}",
                        host.alias,
                        target.as_str(),
                        bound.as_secs()
                    ),
                ));
            }
            self.ports.clock.sleep(self.timing.apply_poll);
        }
    }

    /// Wait for `host` to run the target. `held`: its node is drained by the roll, which a
    /// daemon is not asked to be serving through.
    fn wait_healthy(&self, host: &Host, target: &Version, held: bool) -> Result<(), Halt> {
        let bound = self.timing.healthy(host.role);
        self.wait_until(bound, || {
            if held {
                self.ports.health.check_held(host, target)
            } else {
                self.ports.health.check(host, target)
            }
        })
    }

    /// Ask `check` every [`Timing::poll`] until it says healthy, for at most `bound`. A host
    /// that will not get there fails the wait at once; one that is only not there yet, or
    /// cannot be asked, is asked again until the bound is out.
    fn wait_until(
        &self,
        bound: Duration,
        check: impl Fn() -> Result<Health, PortError>,
    ) -> Result<(), Halt> {
        let start = self.now();
        loop {
            let last = match check() {
                Ok(Health::Healthy) => return Ok(()),
                Ok(Health::Abandoned(why)) => {
                    return Err(Halt::Fail(ErrorCode::SwapAbandoned, why));
                }
                Ok(Health::Waiting(why)) => why,
                Err(e) => e.to_string(),
            };
            if self.since(start) >= bound {
                return Err(Halt::Fail(
                    ErrorCode::HealthTimeout,
                    format!("{} s: {last}", bound.as_secs()),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater_tests::{Rig, all};

    /// Catches the internal readers panicking or inventing a roll when there is none: both
    /// say `unknown_roll`.
    #[test]
    fn the_readers_of_an_empty_updater_say_unknown_roll() {
        let u = Rig::new("empty-readers").open();
        assert_eq!(u.snapshot().expect_err("none").code, ErrorCode::UnknownRoll);
        assert_eq!(
            u.update(|_| {}).expect_err("none").code,
            ErrorCode::UnknownRoll
        );
    }

    /// Catches a roll that resumes past a failed host if a record ever holds one on a running
    /// roll (the loader refuses such a file, so this is the second lock on the door): the
    /// roll fails with `state_lost` and no port is called.
    #[test]
    fn a_failed_host_on_a_running_roll_is_never_resumed_past() {
        let rig = Rig::new("failed-on-running");
        let u = rig.open();
        u.start(&all("2.0", "k")).expect("start");
        u.lock().rec.as_mut().expect("rec").roll.hosts[0].phase = Phase::Failed;
        let roll = u.drive("roll-1").expect("drive");
        assert_eq!(roll.state, RollState::Failed);
        assert_eq!(roll.failure.expect("f").code, ErrorCode::StateLost);
        assert!(rig.log.events().is_empty());
    }

    /// Catches port text reaching the record raw: a newline would forge a log line, and an
    /// unbounded string would put apt's output in a public GET.
    #[test]
    fn detail_is_one_short_line() {
        assert_eq!(clean("a\nb\r\0c\td"), "a b  c d");
        assert_eq!(clean(&"x".repeat(500)).chars().count(), DETAIL_MAX);
        assert_eq!(clean(&"\u{e9}".repeat(500)).chars().count(), DETAIL_MAX);
    }
}

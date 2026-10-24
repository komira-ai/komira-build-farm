//! What every probe shares: the context a probe runs in, running VMs and waiting on them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::spec::{Event, VmSpec};
use crate::system::{Poll, System, VmProcess};

/// How long the probes wait for each thing. The binary uses [`Timing::real`]; tests use
/// [`Timing::instant`].
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// From spawning a helper to its `started` event.
    pub start_timeout: Duration,
    /// How long a started VM must stay running to count as booted.
    pub hold: Duration,
    /// How long to wait for a marker the guest writes; longer than the guest's own wait
    /// for the share, so the guest gives up first and says why.
    pub guest_timeout: Duration,
    /// One wait for a helper event or a file.
    pub tick: Duration,
    /// How long to retry a start that the two-guest limit refuses.
    pub retry_window: Duration,
    /// How often the host samples a running guest (CPU, disk writes).
    pub sample_every: Duration,
}

impl Timing {
    #[must_use]
    pub fn real() -> Self {
        Self {
            start_timeout: Duration::from_secs(90),
            hold: Duration::from_secs(20),
            guest_timeout: Duration::from_secs(320),
            tick: Duration::from_millis(100),
            retry_window: Duration::from_secs(60),
            sample_every: Duration::from_secs(10),
        }
    }

    #[must_use]
    pub fn instant() -> Self {
        let z = Duration::from_millis(5);
        Self {
            start_timeout: z,
            hold: z,
            guest_timeout: z,
            tick: Duration::from_millis(1),
            retry_window: z,
            sample_every: Duration::from_millis(2),
        }
    }
}

/// What a probe runs against.
pub struct Ctx<'a> {
    pub sys: &'a dyn System,
    /// Where clones, shares and logs go.
    pub work: PathBuf,
    /// The prepared base bundle.
    pub base: PathBuf,
    /// A `kbf-guest` built for the guest, copied into the shared folder.
    pub guest_binary: Option<PathBuf>,
    /// Leave the clones and shares on disk when the probe ends (`--keep`), for
    /// `image harvest`.
    pub keep: bool,
    /// Give the second VM of a pair a graphics device (`--graphics`).
    pub graphics: bool,
    pub timing: Timing,
}

/// A started helper, with every event it has printed.
pub struct Vm {
    pub name: String,
    pub proc: Box<dyn VmProcess>,
    pub events: Vec<Event>,
    /// When each of `events` arrived, in milliseconds after the spawn (same order).
    pub event_ms: Vec<u64>,
    /// Why the helper's run ended before it was asked to, if it did: the terminal event
    /// or the helper's exit. A wait for the guest stops as soon as this is set.
    pub ended: Option<String>,
    pub began: Instant,
    gone: bool,
}

/// What one [`Vm::watch`] ended on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Failed(Boot),
    Started,
    Timeout,
}

/// How a VM start went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    pub booted: bool,
    pub started_ms: Option<u64>,
    /// The failure, when it did not boot: the helper's error text or how it ended.
    pub failure: Option<String>,
    /// The framework error code name of the failure, when there is one.
    pub code: Option<String>,
}

impl Boot {
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "booted": self.booted,
            "started_ms": self.started_ms,
            "failure": self.failure,
            "code": self.code,
        })
    }
}

impl Vm {
    /// Spawns a helper for `spec`.
    ///
    /// # Errors
    /// The helper could not be spawned.
    pub fn spawn(
        sys: &dyn System,
        name: &str,
        spec: &VmSpec,
        run_as: Option<u32>,
    ) -> Result<Self, String> {
        let proc = sys.start_vm(spec, run_as)?;
        Ok(Self {
            name: name.to_owned(),
            proc,
            events: Vec::new(),
            event_ms: Vec::new(),
            ended: None,
            began: Instant::now(),
            gone: false,
        })
    }

    /// Reads helper events for up to `wait`, keeping them, until a terminal one (an error,
    /// a stop, the helper's end) or, when `until_started`, the `started` event.
    pub fn watch(&mut self, wait: Duration, tick: Duration, until_started: bool) -> Step {
        let end = Instant::now() + wait;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Step::Timeout;
            }
            match self.proc.poll(left.min(tick)) {
                Poll::Quiet => {}
                Poll::Event(e) => {
                    let failed = terminal(&e);
                    let started = matches!(e, Event::Started { .. });
                    self.events.push(e);
                    self.event_ms.push(self.ms());
                    if let Some(t) = failed {
                        self.ended = t.failure.clone();
                        return Step::Failed(t);
                    }
                    if started && until_started {
                        return Step::Started;
                    }
                }
                Poll::Gone { code, signal } => {
                    self.gone = true;
                    let log = self
                        .proc
                        .diagnostics()
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| format!("; helper log: {}", t.trim()))
                        .unwrap_or_default();
                    let failure = format!("helper ended (code {code:?}, signal {signal:?}){log}");
                    self.ended = Some(failure.clone());
                    return Step::Failed(Boot {
                        booted: false,
                        started_ms: self.started_ms(),
                        failure: Some(failure),
                        code: None,
                    });
                }
            }
        }
    }

    /// Milliseconds since the spawn.
    #[must_use]
    pub fn ms(&self) -> u64 {
        u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Every helper event with the time it arrived, and why the run ended if it did.
    #[must_use]
    pub fn trace_json(&self) -> Value {
        let events: Vec<Value> = self
            .events
            .iter()
            .zip(&self.event_ms)
            .map(|(e, ms)| json!({"host_ms": ms, "event": serde_json::to_value(e).unwrap_or(Value::Null)}))
            .collect();
        json!({"events": events, "ended": self.ended})
    }

    /// The `started` event's time, if it came.
    #[must_use]
    pub fn started_ms(&self) -> Option<u64> {
        self.events.iter().find_map(|e| match e {
            Event::Started { ms } => Some(*ms),
            _ => None,
        })
    }

    /// Waits for the `started` event, then requires the VM to stay up for the hold time.
    pub fn await_boot(&mut self, t: &Timing) -> Boot {
        match self.watch(t.start_timeout, t.tick, true) {
            Step::Failed(b) => return b,
            Step::Timeout => {
                return Boot {
                    booted: false,
                    started_ms: None,
                    failure: Some("no started event in time".to_owned()),
                    code: None,
                };
            }
            Step::Started => {}
        }
        match self.watch(t.hold, t.tick, false) {
            Step::Failed(b) => b,
            Step::Timeout | Step::Started => Boot {
                booted: true,
                started_ms: self.started_ms(),
                failure: None,
                code: None,
            },
        }
    }

    /// Waits for the file `path` to exist, up to `wait`; the time it took. The wait ends
    /// early, with `None`, when the VM stops or errors or the helper exits (the guest
    /// will write nothing more): [`ended`](Self::ended) says why.
    pub fn wait_file(&mut self, path: &Path, wait: Duration, tick: Duration) -> Option<Duration> {
        let began = Instant::now();
        loop {
            // Existence only, without following a link the guest may have left.
            if std::fs::symlink_metadata(path).is_ok() {
                return Some(began.elapsed());
            }
            if began.elapsed() >= wait || self.gone || self.ended.is_some() {
                return None;
            }
            // Events keep arriving while the guest boots: keep them.
            self.watch(tick, tick, false);
        }
    }

    /// Asks the helper to stop its VM and waits for it; kills it when it does not exit.
    /// True when it exited by itself.
    pub fn stop(&mut self, t: &Timing) -> bool {
        self.proc.request_stop();
        let ok = self.proc.wait_gone(t.start_timeout);
        if !ok {
            self.proc.kill9();
            self.proc.wait_gone(t.start_timeout);
        }
        self.gone = true;
        ok
    }

    /// SIGKILL, and wait for the process to go.
    pub fn kill9(&mut self, t: &Timing) {
        self.proc.kill9();
        self.proc.wait_gone(t.start_timeout);
        self.gone = true;
    }

    /// The vsock ports and console text the guest used, from the events so far.
    #[must_use]
    pub fn guest_traffic(&self) -> Value {
        let vsock: Vec<u32> = self
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Vsock { port } => Some(*port),
                _ => None,
            })
            .collect();
        let console: Vec<Value> = self
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Console { port, text } => Some(json!({"port": port, "text": text})),
                _ => None,
            })
            .collect();
        json!({"vsock_connections": vsock, "console_writes": console})
    }
}

/// An event that ends a start attempt, as a failed [`Boot`].
fn terminal(e: &Event) -> Option<Boot> {
    match e {
        Event::Error { message, code, .. } => Some(Boot {
            booted: false,
            started_ms: None,
            failure: Some(message.clone()),
            code: code.clone(),
        }),
        Event::Stopped { .. } => Some(Boot {
            booted: false,
            started_ms: None,
            failure: Some("the VM stopped".to_owned()),
            code: None,
        }),
        Event::State { state, .. } if state == "error" || state == "stopped" => Some(Boot {
            booted: false,
            started_ms: None,
            failure: Some(format!("the VM went to state {state}")),
            code: None,
        }),
        _ => None,
    }
}

impl Ctx<'_> {
    /// A clone of the base under `<work>/<name>`, started as a VM.
    ///
    /// # Errors
    /// The clone or the spawn failed.
    pub fn start_clone(
        &self,
        name: &str,
        identity: Identity,
        edit: impl FnOnce(&mut VmSpec),
        run_as: Option<u32>,
    ) -> Result<(Vm, std::time::Duration), String> {
        let dest = self.work.join(name);
        let took = bundle::clone_bundle(self.sys, &self.base, &dest, identity).map_err(|e| {
            // A half-made clone is of no use; one that was already there is not ours.
            if !matches!(e, bundle::BundleError::Exists(_)) {
                let _ = std::fs::remove_dir_all(&dest);
            }
            e.to_string()
        })?;
        let mut spec = VmSpec::minimal(dest);
        edit(&mut spec);
        let vm = Vm::spawn(self.sys, name, &spec, run_as)?;
        Ok((vm, took))
    }
}

/// A start that was retried because of the two-guest limit.
pub struct Retried {
    pub vm: Vm,
    /// How many starts it took (1 means the first one worked).
    pub attempts: u32,
    /// From the first attempt to the `started` event.
    pub waited: Duration,
}

impl Ctx<'_> {
    /// Starts a helper for `spec`, retrying while the framework refuses with the
    /// two-guest limit, for up to the retry window.
    ///
    /// # Errors
    /// The spawn failed, the start failed for another reason, or the window ran out.
    pub fn spawn_retrying(&self, name: &str, spec: &VmSpec) -> Result<Retried, String> {
        let began = Instant::now();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut vm = Vm::spawn(self.sys, name, spec, None)?;
            match vm.watch(self.timing.start_timeout, self.timing.tick, true) {
                Step::Started => {
                    return Ok(Retried {
                        vm,
                        attempts,
                        waited: began.elapsed(),
                    });
                }
                Step::Failed(b) if b.code.as_deref() == Some(crate::spec::LIMIT_EXCEEDED) => {
                    vm.proc.wait_gone(self.timing.tick);
                    if began.elapsed() >= self.timing.retry_window {
                        return Err(format!("still at the guest limit after {attempts} starts"));
                    }
                    std::thread::sleep(self.timing.tick);
                }
                Step::Failed(b) => return Err(b.failure.unwrap_or_default()),
                Step::Timeout => return Err("no started event in time".to_owned()),
            }
        }
    }
}

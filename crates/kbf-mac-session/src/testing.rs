//! A [`Host`] for tests: user records, launchd domains and processes in memory, with
//! a log of every call and failures on demand.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::helper::{Host, NewUser};

/// The fake clock's time.
pub(crate) const NOW: u64 = 1_800_000_000;

#[derive(Default)]
pub(crate) struct State {
    /// Every call, in order, as text.
    pub(crate) log: Vec<String>,
    /// Users by name.
    pub(crate) users: BTreeMap<String, NewUser>,
    /// Uids some user record outside the helper's has.
    pub(crate) foreign: BTreeSet<u32>,
    /// Live processes per uid.
    pub(crate) procs: BTreeMap<u32, usize>,
    /// Per uid, how many more `kill_all` calls leave its processes alive.
    pub(crate) stubborn: BTreeMap<u32, usize>,
    /// Calls that fail, by name ("uid_taken", "make_home", "create_user",
    /// "delete_user", "bootout gui", "kill_all", "live_processes").
    pub(crate) fail: BTreeSet<&'static str>,
}

/// The fake host; clones share their state.
#[derive(Clone, Default)]
pub(crate) struct FakeHost(pub(crate) Arc<Mutex<State>>);

impl FakeHost {
    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    fn call(&self, name: &'static str, text: String) -> io::Result<()> {
        let mut state = self.state();
        state.log.push(text);
        if state.fail.contains(name) {
            return Err(io::Error::other(format!("{name} failed")));
        }
        Ok(())
    }
}

impl Host for FakeHost {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW)
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        self.call("uid_taken", format!("uid_taken {uid}"))?;
        let state = self.state();
        Ok(state.foreign.contains(&uid) || state.users.values().any(|u| u.uid == uid))
    }

    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
        self.call(
            "make_home",
            format!("make_home {} {uid}:{gid}", homes.join(name).display()),
        )
    }

    fn create_user(&self, user: &NewUser) -> io::Result<()> {
        self.call("create_user", format!("create_user {}", user.name))?;
        self.state().users.insert(user.name.clone(), user.clone());
        Ok(())
    }

    fn delete_user(&self, name: &str) -> io::Result<bool> {
        self.call("delete_user", format!("delete_user {name}"))?;
        Ok(self.state().users.remove(name).is_some())
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        let name = if domain.starts_with("gui/") {
            "bootout gui"
        } else {
            "bootout user"
        };
        self.call(name, format!("bootout {domain}"))
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        self.call("kill_all", format!("kill_all {uid}"))?;
        let mut state = self.state();
        let stubborn = state.stubborn.get(&uid).copied().unwrap_or(0);
        if stubborn > 0 {
            state.stubborn.insert(uid, stubborn - 1);
        } else {
            state.procs.remove(&uid);
        }
        Ok(())
    }

    fn live_processes(&self, uid: u32) -> io::Result<usize> {
        self.call("live_processes", format!("live_processes {uid}"))?;
        Ok(self.state().procs.get(&uid).copied().unwrap_or(0))
    }

    fn pause(&self) {
        self.state().log.push("pause".to_owned());
    }
}

/// A fresh scratch directory for one test.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-mac-session-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Shows tracing output in test logs, so the log statements' arguments run.
pub(crate) fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

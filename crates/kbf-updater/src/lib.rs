//! `kbf-updater`: roll a new kbf version over the hosts of a farm, one host at a time.
//!
//! The core:
//!
//! - [`api`]: the request, the roll as a poller sees it, the closed set of error codes;
//! - [`version`]: the one rule for a version, shared with the host-side wrapper;
//! - [`inventory`]: the root-owned host file, and the order a roll touches the hosts in;
//! - [`ports`]: safe-to-stop, install and query, drain, health and the clock, as traits;
//! - [`ssh_installer`]: the install port over ssh to the `kbf-host-install` forced command;
//! - [`record`]: `roll.json`, the durable record;
//! - [`Updater`]: the state machine over all of the above. One roll at a time, the frontends
//!   first, the first failure stops it, a restart asks the host before it installs again; a
//!   `mac-daemon` host's install starts an apply that the roll polls until it is done;
//! - [`fakes`]: a scripted fake of every port.
//!
//! And around it:
//!
//! - [`server`] and [`http`]: the roll API on a unix socket;
//! - [`listener`]: where that socket comes from (systemd socket activation in production);
//! - [`operator`], [`reply`] and [`secret`]: the client of the frontend's loopback operator
//!   API, and the token files it reads;
//! - [`safe_http`]: the real safe-to-stop, a call to the operator API;
//! - [`node_drain`]: the real drain of a daemon host's node, over the operator API;
//! - [`health`] and [`node_health`]: the real health check of a storage or daemon host, over
//!   the operator API; a frontend is refused (it is a container that `kbf-host-install`
//!   drains and recreates);
//! - [`daemon`]: the flags and the wiring of all of it.
//!
//! And for the coordinator that tells each host's updater what to run
//! (`docs/design/updates.md`): [`wire`], the heartbeat and the reply.

pub mod agent;
pub mod api;
pub mod daemon;
pub mod fakes;
pub mod health;
pub mod http;
pub mod identity;
pub mod inventory;
pub mod listener;
pub mod node_drain;
pub mod node_health;
pub mod operator;
pub mod ports;
pub mod record;
pub mod reply;
pub mod safe_http;
pub mod secret;
pub mod server;
pub mod ssh_installer;
pub mod version;
pub mod wire;

mod timing;
mod updater;

pub use timing::Timing;
pub use updater::{Ports, Started, Updater};

#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod identity_tests;
// The activated descriptor tests use an abstract socket address, which is Linux's.
#[cfg(all(test, target_os = "linux"))]
mod listener_tests;
#[cfg(test)]
mod node_drain_tests;
#[cfg(test)]
mod node_health_own_tests;
#[cfg(test)]
mod node_health_tests;
#[cfg(test)]
mod reply_tests;
#[cfg(test)]
mod safe_http_tests;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
mod testutil;
#[cfg(test)]
mod updater_drain_tests;
#[cfg(test)]
mod updater_followup_tests;
#[cfg(test)]
mod updater_fresh_host_tests;
#[cfg(test)]
mod updater_mac_tests;
#[cfg(test)]
mod updater_restart_tests;
#[cfg(test)]
mod updater_tests;
#[cfg(test)]
mod wire_tests;

/// The program's text for `--version`.
pub fn version_line() -> String {
    format!("kbf-updater {}", env!("CARGO_PKG_VERSION"))
}

/// The binary, as a function so that it is tested: the argument list in, the exit code and the
/// text out. `--version` prints the version; `agent` and the flags of [`agent::flags`] start
/// the agent mode ([`run_agent_with`]), `enroll` enrolls the host ([`run_enroll_with`]); the flags of [`daemon`] start the updater,
/// call `ready` with a handle that stops it, and return when it is stopped. Anything else is
/// a usage error (exit 2); a start that fails is exit 1 with the reason.
pub fn run_with(args: &[String], ready: impl FnOnce(daemon::Stopper)) -> (u8, String) {
    if args.get(1).map(String::as_str) == Some("--version") {
        return (0, format!("{}\n", version_line()));
    }
    let cfg = match daemon::parse_flags(
        args.get(1..).unwrap_or_default(),
        &listener::Activation::from_env(),
    ) {
        Ok(cfg) => cfg,
        Err(why) => return (2, format!("kbf-updater: {why}\n{}", daemon::USAGE)),
    };
    daemon::run_config(&cfg, ready)
}

/// The agent mode (`kbf-updater agent ...`): the flags in, `ready` called with a handle that
/// stops it, and the exit code and text out when it is stopped. A usage error is exit 2, a
/// start that fails is exit 1 with the reason.
pub fn run_agent_with(args: &[String], ready: impl FnOnce(agent::Stop)) -> (u8, String) {
    match agent::flags::parse_flags(args.get(2..).unwrap_or_default()) {
        Ok(cfg) => agent::run_config(&cfg, ready),
        Err(why) => (2, format!("kbf-updater: {why}\n{}", agent::flags::USAGE)),
    }
}

/// `kbf-updater enroll ...` ([`agent::enroll`]): the host gets its updater certificate with a
/// one-time token. The user is the process's, the time the clock's.
pub fn run_enroll_with(args: &[String]) -> (u8, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    agent::enroll::run_enroll(args.get(2..).unwrap_or_default(), secret::euid(), now)
}

/// [`run_with`], [`run_agent_with`] or [`run_enroll_with`], for the binary: nothing to do once
/// started.
pub fn run(args: &[String]) -> (u8, String) {
    match args.get(1).map(String::as_str) {
        Some("agent") => run_agent_with(args, |_| {}),
        Some("enroll") => run_enroll_with(args),
        _ => run_with(args, |_| {}),
    }
}

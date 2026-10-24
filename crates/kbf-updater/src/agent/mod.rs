//! The agent mode of `kbf-updater`: an unprivileged updater on every host that dials the
//! coordinator and owns that host's update (`docs/design/updates.md`). The roll mode of the
//! crate (the roll API behind a front door, ssh to `kbf-host-install`'s forced command) is
//! untouched and keeps working.
//!
//! - [`flags`]: the flags of `kbf-updater agent`;
//! - [`coordinator`]: the heartbeat, one `POST` over mutual TLS;
//! - [`local`]: `kbf-host-install`'s local socket, which does the root work;
//! - [`enroll`] and [`renew`]: the host's certificate, made once with a token and renewed by the
//!   agent at half its life;
//! - [`status`]: what the heartbeat says;
//! - [`update`]: one host's A/B, step by step;
//! - [`run`]: the two threads, and the wiring of the real ports ([`build`]).

pub mod coordinator;
pub mod enroll;
pub mod flags;
pub mod local;
pub mod renew;
pub mod run;
pub mod status;
pub mod update;

mod build;

pub use build::{NO_CREDENTIAL, NoCredential, run_agent, run_config};
pub use run::{Agent, Stop, World};

#[cfg(test)]
mod build_tests;
#[cfg(test)]
mod coordinator_tests;
#[cfg(test)]
mod enroll_tests;
#[cfg(test)]
mod entry_tests;
#[cfg(test)]
mod flags_tests;
#[cfg(test)]
mod frontend_tests;
#[cfg(test)]
mod local_tests;
#[cfg(test)]
mod pki;
#[cfg(test)]
mod renew_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod update_tests;

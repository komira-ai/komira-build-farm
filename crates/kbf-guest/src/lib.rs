//! `kbf-guest`: the agent inside a macOS VM guest (`docs/design/macos-vms.md` section 6,
//! step 4). It is started as a LaunchAgent of the guest's non-admin auto-login user,
//! refuses to run as root, and runs exactly one command per boot for the host.
//!
//! - [`wire`]: the framed, versioned messages, byte for byte.
//! - [`server`]: the handshake (version and per-boot token), the one-command rule,
//!   `Kill`, the timeout, and the exit report.
//! - [`run`]: the working directory checked against the inputs share, stdout and stderr
//!   written to the outputs share, the command's process group, `wait4` usage, and the
//!   report on requested outputs.
//! - [`client`]: the host side.
//! - [`session`]: the launchd session type `Ready` reports, and the root refusal.
//!
//! It serves a Unix socket. The virtio socket a VM gives it, the relay through
//! `kbf-vmm`, the image that installs it and the VM driver that calls it are planned and
//! not built.

pub mod client;
pub mod run;
pub mod server;
pub mod session;
pub mod wire;

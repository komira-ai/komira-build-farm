//! `kbf-mdm`: the MDM gate (`kbf-mdm-gate`) and the backend it drives.
//!
//! The gate is a small process beside the Mac fleet's MDM. It alone holds the MDM's
//! API key, and offers `kbf-server`, over mutual TLS pinned to the server's client
//! key, only the verbs of docs/design/mdm-backend.md (M2.2) and
//! fleet-updates-security.md (S5.2): status reads; `enforce` and `withdraw` of a macOS
//! build a signed set names for the Mac's pool; `profile` by an allowlisted digest;
//! `grant-admin`; and two erase verbs with no authority of their own, a relay for an
//! operator-signed erase request and `bring-forward` of an erase the gate already
//! scheduled under one. Every policy (inventory, caps, floors, the `kbf.` prefix,
//! signatures) lives here, whichever MDM is behind the [`backend::MdmBackend`] trait.
//!
//! Modules:
//! - [`request`]: the operator's signed erase request text;
//! - [`signers`]: the allowed-signers file and security-key signature checks;
//! - [`sets`]: the signed software set and key statement `enforce` needs;
//! - [`backend`]: the trait the gate drives, and [`nanohub`], its NanoHUB client;
//! - [`gate`]: the verbs and their rules, with [`state`] (what survives a restart),
//!   [`journal`] (audit log and alerts) and [`grant`] (the admin grant it signs);
//! - [`api`] and [`tls`]: the HTTP API and its mutual-TLS listener;
//! - [`config`]: files, flags and startup.

pub mod api;
pub mod backend;
pub mod clock;
pub mod config;
pub mod gate;
pub mod grant;
pub mod journal;
pub mod nanohub;
pub mod request;
pub mod sets;
pub mod signers;
pub mod state;
pub mod tls;
pub mod trusted;

#[cfg(test)]
mod fake_nanohub;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tlskit;

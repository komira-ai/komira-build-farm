//! `kbf-mdm-api`: what `kbf-server` and `kbf-mdm-gate` agree on
//! (`docs/design/mdm-backend.md` section M2, `fleet-updates-security.md` section S5.2).
//!
//! - [`pb`]: the `kbf.mdmgate.v1` gRPC service, client and server, generated from
//!   `proto/kbf/mdmgate/v1/gate.proto`: `Status`, `Enforce`, `Withdraw`, `Profile`.
//!   It has no erase verb and no pass-through to the MDM.
//! - [`names`]: the values both sides check the same way: a serial, the `kbf.`
//!   declaration prefix and `kbf.osupdate.<serial>`, a `TargetLocalDateTime`, a
//!   catalogue date, a macOS version, a profile digest.
//! - [`catalogue`]: Apple's public catalogue (`gdmf.apple.com/v2/pmv`): its parser, and
//!   [`catalogue::DailyCatalogue`], which reads it through an injected
//!   [`catalogue::CatalogueFetcher`] at most once a day.
//!
//! The crate holds no policy: what the gate refuses, and what the server does with an
//! answer, live in those programs.

pub mod catalogue;
pub mod names;

/// The generated `kbf.mdmgate.v1` messages, client (`mdm_gate_client`) and server
/// (`mdm_gate_server`).
#[allow(clippy::all)]
pub mod pb {
    tonic::include_proto!("kbf.mdmgate.v1");
}

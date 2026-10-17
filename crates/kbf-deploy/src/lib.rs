//! `kbf-deploy`: the deploy tool. The binary's subcommands are the kits; this library
//! holds them, the command line and what they share beyond `kbf-host`.
//!
//! A kit that changes a host does it the one way `kbf-host` gives: it builds a
//! `kbf_host::Plan` of steps and carries it out with `kbf_host::execute`, which prints
//! each step as `DRY-RUN: <step>` by default and does it only with `--apply`.
//!
//! - [`host`]: the host side of commit-confirm deploys: the releases' layout on disk,
//!   the deploy state and its lock, and `host status` (read-only); and the SQLite
//!   state store's file backed up online (`host state-backup`) and restored offline
//!   under the store's own lock (`host state-restore`).
//! - [`https`]: a small HTTPS client (rustls with ring, the host's trusted roots) for
//!   the kits that download or check over TLS.
//! - [`config`]: `--config FILE`, `KEY=VALUE` lines over a closed key set; an unknown
//!   or repeated key is refused.
//! - [`unit`]: a systemd unit file read as systemd reads it (sections, `\` continuations,
//!   the last assignment wins), shared with the docs check in `kbf-it`.
//! - [`template`]: strict `@KEY@` substitution; a placeholder with no value is refused.
//! - [`cli`]: the `kbf-deploy` command line, one subcommand per kit.
//!
//! The kits:
//! - [`minio`]: `minio buckets` (kbf's buckets, their policy and a service account key
//!   file) and `minio nodeport` (a NodePort Service for a MinIO's S3 port).
//! - [`linux`]: a converged storage node's systemd units, rendered from compiled-in
//!   templates (`linux render-units`) and checked against the host's priorities
//!   (`linux check-units`); and, as root, the units installed and the services started
//!   (`linux install-units`) and the swapfile (`linux swap`).
//! - [`mdm`]: an Apple MDM on one Linux host: its pinned upstream archives fetched
//!   (`mdm fetch`), its services installed as root (`mdm install`), and read-only
//!   checks of what runs (`mdm verify`); and, on an operator's own machine, an erase
//!   request for one Mac signed with their security key (`mdm erase-request`).
//! - [`envoy`]: a client front's Kubernetes objects and Envoy config, rendered from
//!   the settings (`envoy render`), byte for byte the same for the same settings, and
//!   a render checked for what the REAPI path through the front depends on
//!   (`envoy check`), and the render applied to a cluster through kubectl after the
//!   cluster's prerequisites and a server-side dry run pass (`envoy apply`).
//! - [`log`]: the last lines of a kbf-server's durable log across its rotated files
//!   (`log`; read-only).
//! - [`pilot`]: a pilot kbf-server run as a systemd user service by the operator's own
//!   user (`pilot server-install`), and the buck2 overlay that points one linked
//!   worktree at it, which never replaces a file it did not write (`pilot buckconfig`).

pub mod cli;
pub mod config;
pub mod envoy;
pub mod host;
pub mod https;
pub mod linux;
pub mod log;
pub mod mdm;
pub mod minio;
pub mod pilot;
pub mod template;
pub mod unit;

pub use config::{Config, ConfigError};

#[cfg(test)]
pub(crate) mod testkit;

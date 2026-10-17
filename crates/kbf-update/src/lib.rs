//! The node's end of a pull-based update.
//!
//! The frontend tells a node, over the session the daemon already holds, which release it
//! wants ([`Offer`]) and when this node may change ([`Order`]). This crate is what the
//! node does with that, and nothing about the wire:
//!
//! - [`start`] makes a [`Handle`] the session feeds and a [`Worker`] that runs beside it.
//! - An offer is checked ([`Offer::check`]), then fetched through a [`Source`] into the
//!   [inbox](inbox), where a root helper picks it up. Every byte is counted against the
//!   offered size and hashed on the way; a directory becomes visible by one rename after
//!   the digest matched.
//! - An order is written into the inbox as a small fixed file when it is for the current
//!   session, newer than every order before it, for a release the helper holds, and, for
//!   an activation, when no lease is live. An accepted activation closes the update
//!   gate ([`Handle::gate`]); the daemon takes no lease while it is closed.
//! - [`Status`] says where the node stands, from its own state and the helper's status
//!   file ([`helper`]).
//!
//! # Trust
//!
//! The client does not decide trust. It copies bytes the frontend served into a
//! directory the unprivileged service user owns, and a root helper verifies the GitHub
//! build attestation of what it finds there, offline, before anything is installed.
//! Whatever runs as the service user can write the inbox: on a host whose actions share
//! that uid it can stage a release that verifies or order an early activation. That is
//! a loss of availability, never an unsigned install.

mod client;
#[cfg(test)]
mod fixtures;
mod floors;
mod grpc;
pub mod helper;
pub mod inbox;
mod offer;
mod source;
mod stage;
mod types;
mod wire;

pub use client::{Config, Decision, Handle, StartError, Worker, start};
pub use grpc::ReleaseClient;
pub use offer::Refusal;
pub use source::{Chunks, FetchError, Source};
pub use stage::Backoff;
pub use types::{
    COMPONENT_NAMES, Component, FREE_DISK_FACTOR, MAX_ATTESTATION_BYTES, MAX_COMPONENT_BYTES,
    Offer, Order, Staged, State, Status, Verb,
};

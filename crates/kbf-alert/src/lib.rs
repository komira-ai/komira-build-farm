//! Alerting: an alert book with hysteresis, a durable outbox, and a webhook notifier.
//!
//! - [`Alert`] names a problem and the exact fix an operator applies. Its key is the
//!   rule and the subject ([`Key`]), so one rule raises one alert per node or resource.
//! - [`AlertBook`] turns checks into events. A key is raised after
//!   [`Hysteresis::raise_after`] bad checks in a row and resolved after
//!   [`Hysteresis::resolve_after`] good checks in a row, each exactly once; it records
//!   when the problem was first and last seen.
//! - [`Outbox`] is the ordered queue of events not yet delivered, with its encoding.
//! - [`Webhook`] delivers the outbox: it POSTs each event as JSON from its own thread,
//!   in order, retrying with backoff, and keeps the outbox in a file under a state
//!   directory, so an event survives a restart until it is delivered.
//!
//! `alert`, `book` and `outbox` are pure: they read no clock, do no I/O and deny the
//! items `clippy.toml` lists. Time enters them as [`UnixMillis`]. `webhook` is the one
//! impure module.

mod alert;
mod book;
mod outbox;
mod webhook;

pub use crate::alert::{Alert, AlertError, Event, Key, Severity, Transition, UnixMillis};
pub use crate::book::{AlertBook, Hysteresis, Record};
pub use crate::outbox::{Backoff, Delivery, Outbox, OutboxError};
pub use crate::webhook::{OUTBOX_FILE, Status, Webhook, WebhookConfig, WebhookError};

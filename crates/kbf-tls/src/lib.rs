//! TLS for a server listener whose certificate, key and client CA are files that change
//! while the process runs.
//!
//! tonic fixes a listener's TLS configuration when the server is built, so a renewed
//! certificate would need a restart, and a restart ends every build in flight. This crate
//! accepts the TLS connections itself instead: [`TlsSource`] holds the configuration the
//! next handshake uses and re-reads the files on an interval; [`Incoming`] is the stream
//! of accepted connections a tonic server is given (`serve_with_incoming`).
//!
//! - A connection already established keeps the configuration it was accepted under,
//!   including the trust roots it was verified against. A new handshake uses whatever
//!   configuration is current when it starts.
//! - A change is applied only if the new files make a working configuration: every file
//!   reads, the chain and the key parse, the key matches the certificate, the client CA
//!   holds at least one certificate, and the leaf has not expired. Otherwise the
//!   configuration in use stays, and the failure is logged and counted
//!   ([`render_metrics`]). A pair renewed in two writes is a mismatch between them; the
//!   next look finds the pair whole and applies it.
//! - The first load must succeed, as it always has: a server does not start on a bad
//!   file.
//!
//! The peer's certificate chain and address reach the services through [`peer_certs`] and
//! [`remote_addr`], which also answer for a connection tonic's own TLS accepted.

mod config;
mod incoming;
mod source;

pub use config::{TlsError, TlsFiles};
pub use incoming::{
    Conn, ConnInfo, HANDSHAKE_TIMEOUT, Incoming, peer_certs, peer_certs_of, remote_addr,
};
pub use source::{Outcome, Reloads, TlsSource, render_metrics};

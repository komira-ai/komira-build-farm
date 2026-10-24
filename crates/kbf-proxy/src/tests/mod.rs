//! End-to-end tests of the serving core, over loopback sockets, with fake children. They run
//! on a current-thread runtime under paused time, so every wait is virtual and no test asserts
//! on a wall clock.

mod bypass;
mod flood;
mod frames;
mod h1;
mod keepalive;
mod kit;
mod lifetime;
mod operator;
mod own_headers;
pub(crate) mod pki;
mod pool_kit;
mod ready;
mod reload;
mod routing;
mod secure;
mod sni;
mod ui;

//! Chain replication over a farm of real services on loopback, each with an in-memory
//! store, a switch that cuts it off, and a map the test publishes.

mod chain;
mod degrade;
mod farm;
mod generation;
mod leg;
mod plan;
mod tls;
mod worker;

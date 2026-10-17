//! The routed data plane ([`kbf_transfer::RoutedCas`]): the conformance checks over real
//! storage nodes, and what the checks cannot reach over farms whose nodes are fakes that
//! can lie, stall and fail (one member, or several for failover). One test binary, so
//! that one copy of the code under test is run by all of it.

mod conformance;
mod owners;
mod support;

mod errors;
mod failover;
mod limits;
mod missing;

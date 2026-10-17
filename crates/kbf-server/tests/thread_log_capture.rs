//! The per-thread log capture keeps a line whose callsite another thread reached
//! first while the subscriber was being installed.

mod thread_log;

/// The one callsite both threads reach.
fn say() {
    tracing::info!("the line under test");
}

/// Catches: a callsite first reached on another thread while the global subscriber is
/// installed (its dispatcher registered, the global default not yet set) caching
/// "never" for good, so a test that logs through it reads an empty log. Tests run in
/// parallel threads, so any callsite can be reached that way; this binary has one test
/// so that it is the one to install the subscriber.
#[test]
fn a_callsite_reached_elsewhere_during_install_is_still_captured() {
    thread_log::install_with(|| std::thread::spawn(say).join().expect("the other thread"));
    let log = thread_log::logged(say);
    assert!(log.contains("the line under test"), "not captured: {log:?}");
}

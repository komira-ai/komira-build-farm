//! Log capture per thread, for test binaries whose tests run in parallel: one global
//! subscriber writes every line to the writing thread's buffer, so a test reads only
//! the lines its own thread logged.

use std::cell::RefCell;
use std::sync::Once;

std::thread_local! {
    static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Writes log lines to this thread's buffer.
struct ThreadLog;

impl std::io::Write for ThreadLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG.with(|log| log.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the global subscriber (info and above) once. `in_window` runs after the
/// subscriber's dispatcher is registered and before it is the global default, the
/// window `tracing::subscriber::set_global_default` also has; `logged` passes nothing.
/// Every call then re-evaluates the callsites, so one reached in that window logs.
pub fn install_with(in_window: impl FnOnce()) {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadLog)
            .with_ansi(false)
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        in_window();
        tracing::dispatcher::set_global_default(dispatch).expect("one global subscriber");
    });
    // A callsite another thread first reached in that window asked no subscriber and
    // cached "never", and nothing asks again unless told to: re-evaluate every callsite
    // against the subscriber now that it is the global default, on every call (a
    // thread caught mid-registration in the window may store its "never" late).
    tracing_core::callsite::rebuild_interest_cache();
}

/// Log lines written on this thread while `f` runs, at info and above.
pub fn logged(f: impl FnOnce()) -> String {
    install_with(|| {});
    LOG.with(|log| log.borrow_mut().clear());
    f();
    LOG.with(|log| String::from_utf8(log.borrow().clone()).expect("UTF-8 log"))
}

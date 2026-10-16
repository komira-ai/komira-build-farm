//! A lease's run with its driver's panics contained (the all-in-one design,
//! komira-build-farm#657, section 7.3).

use std::any::Any;
use std::panic::AssertUnwindSafe;

use futures::FutureExt as _;
use kbf_proto::reapi::ActionResult;
use kbf_types::LeaseId;

use crate::runtime::{RuntimeError, Work};
use crate::runtimes::AnyRuntime;

/// Runs `work` on `run`. A driver that panics ends this run, never the daemon (or, in
/// kbf-all-in-one, the server beside it), and the run still reports: a run that never
/// reported would keep its lease listed as running for ever.
pub(crate) async fn contained(
    run: &dyn AnyRuntime,
    id: LeaseId,
    work: Work,
) -> Result<ActionResult, RuntimeError> {
    match AssertUnwindSafe(run.run(work)).catch_unwind().await {
        Ok(outcome) => outcome,
        Err(panic) => {
            // What the run left behind is the runtime's to stop, as for a cancelled
            // lease; a kill that panics too changes nothing more.
            let _ = AssertUnwindSafe(run.kill(id)).catch_unwind().await;
            Err(panicked(run.driver(), panic.as_ref()))
        }
    }
}

/// The failure a run whose driver panicked reports: INTERNAL, the farm's (a driver
/// bug), naming the driver and the panic's message.
pub(crate) fn panicked(driver: &str, panic: &(dyn Any + Send)) -> RuntimeError {
    let message = panic
        .downcast_ref::<&str>()
        .map(|m| (*m).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned());
    RuntimeError::Failed(format!("the {driver} driver panicked: {message}"))
}

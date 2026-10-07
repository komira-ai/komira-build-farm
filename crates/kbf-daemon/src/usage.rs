//! What an action used: the [`ResourceUsage`] a runtime reports inside the action's
//! `ActionResult`, and [`Child`], a process measured by the kernel when it is reaped.
//!
//! A process's CPU time and peak resident memory are known exactly only when it is
//! reaped: `wait4` returns them for the process and every descendant it waited for.
//! The standard library and tokio reap without returning them, so [`Child`] reaps
//! through `libc` itself. Those two calls are this crate's only unsafe code.

use std::io;
use std::process::Command;
use std::time::Duration;

use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::ResourceUsage;
use prost::Message;
use prost_types::Any;
use tokio::time::Instant;

/// The type URL of a [`ResourceUsage`] in `ExecutedActionMetadata.auxiliary_metadata`.
pub const USAGE_TYPE_URL: &str = "type.googleapis.com/kbf.worker.v1.ResourceUsage";

/// How often [`Child::wait`] asks whether the process has exited. The wall time it
/// reports is late by at most this much.
pub const POLL: Duration = Duration::from_millis(2);

/// `usage` as an `auxiliary_metadata` entry.
#[must_use]
pub fn usage_any(usage: &ResourceUsage) -> Any {
    Any {
        type_url: USAGE_TYPE_URL.to_owned(),
        value: usage.encode_to_vec(),
    }
}

/// The resource usage a result carries, if it carries one that decodes.
#[must_use]
pub fn usage_of(result: &ActionResult) -> Option<ResourceUsage> {
    let metadata: &ExecutedActionMetadata = result.execution_metadata.as_ref()?;
    let any = metadata
        .auxiliary_metadata
        .iter()
        .find(|any| any.type_url == USAGE_TYPE_URL)?;
    ResourceUsage::decode(any.value.as_slice()).ok()
}

/// How a measured process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exited {
    /// The exit status, or 128 plus the signal number for a process a signal ended
    /// (the shell's convention).
    pub exit_code: i32,
    pub usage: ResourceUsage,
}

/// A spawned process, the leader of its own process group, not yet reaped. Its pid
/// cannot be reused until it is reaped, and only [`Child::wait`] reaps it, so a signal
/// sent before then reaches this process group and no other.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
    started: Instant,
}

impl Child {
    /// Spawns `command` as the leader of a new process group.
    ///
    /// # Errors
    /// The process could not be started.
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;
        let started = Instant::now();
        let child = command.process_group(0).spawn()?;
        // Dropping a `std::process::Child` neither waits for it nor kills it: the
        // process is this type's to reap. A pid always fits `pid_t`.
        let pid = child.id() as libc::pid_t;
        Ok(Self { pid, started })
    }

    /// Waits for the process to exit, polling every [`POLL`]. When `stop` completes
    /// first, the process group is killed and waited for, and `None` is returned.
    ///
    /// # Errors
    /// The process cannot be waited for (it is not this process's child).
    pub async fn wait(
        self,
        stop: impl std::future::Future<Output = ()>,
    ) -> io::Result<Option<Exited>> {
        let mut stop = std::pin::pin!(stop);
        let mut killed = false;
        loop {
            if let Some(exited) = self.try_reap()? {
                return Ok((!killed).then_some(exited));
            }
            tokio::select! {
                () = &mut stop, if !killed => {
                    self.kill();
                    killed = true;
                }
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// Reaps the process if it has exited.
    fn try_reap(&self) -> io::Result<Option<Exited>> {
        let mut status: libc::c_int = 0;
        // SAFETY: `rusage` is a C struct of integers; all zeroes is a valid value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals of the right types, which `wait4`
        // writes and does not keep. WNOHANG makes the call return at once.
        let reaped =
            unsafe { libc::wait4(self.pid, &raw mut status, libc::WNOHANG, &raw mut usage) };
        match reaped {
            0 => Ok(None),
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(Some(Exited {
                exit_code: exit_code(status),
                usage: ResourceUsage {
                    cpu_user_micros: micros(usage.ru_utime),
                    cpu_system_micros: micros(usage.ru_stime),
                    // Linux reports the peak resident set in KiB.
                    peak_memory_bytes: u64::try_from(usage.ru_maxrss).unwrap_or(0) * 1024,
                    wall_micros: u64::try_from(self.started.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                },
            })),
        }
    }

    /// Sends SIGKILL to the process group.
    fn kill(&self) {
        // SAFETY: `kill` takes plain integers. The group's leader is not reaped yet
        // (only `try_reap` reaps it), so the group id still names this process group.
        // A group that has already exited makes the call fail with ESRCH, which is
        // what a kill of finished work should do: nothing.
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
    }
}

fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    }
}

fn micros(t: libc::timeval) -> u64 {
    let micros = t.tv_sec * 1_000_000 + t.tv_usec;
    u64::try_from(micros).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a failed `wait4` read as "still running" (the lease would poll forever)
    /// rather than reported.
    #[tokio::test]
    async fn waiting_for_a_process_that_is_not_a_child_fails() {
        let not_ours = Child {
            pid: libc::pid_t::try_from(std::process::id()).expect("pid"),
            started: Instant::now(),
        };
        let error = not_ours
            .wait(std::future::pending())
            .await
            .expect_err("this process is not its own child");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }

    /// Catches usage written to the wrong field, or not found again by its type URL.
    #[test]
    fn usage_round_trips_through_the_result() {
        let usage = ResourceUsage {
            cpu_user_micros: 1,
            cpu_system_micros: 2,
            peak_memory_bytes: 3,
            wall_micros: 4,
        };
        let other = Any {
            type_url: "type.googleapis.com/other".to_owned(),
            value: vec![0xff],
        };
        let mut result = ActionResult::default();
        assert_eq!(usage_of(&result), None);
        result.execution_metadata = Some(ExecutedActionMetadata {
            auxiliary_metadata: vec![other, usage_any(&usage)],
            ..ExecutedActionMetadata::default()
        });
        assert_eq!(usage_of(&result), Some(usage));
    }
}

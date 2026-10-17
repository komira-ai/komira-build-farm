//! The `rollout` subcommand of the `kbf-deploy` command line ([`crate::cli`]).
//!
//! ```text
//! kbf-deploy rollout --inventory FILE --commit SHA --run-id ID
//!                    [--idle-timeout SECONDS] [--pause-file FILE] [--apply]
//! ```
//!
//! Without `--apply` it prints the plan as `DRY-RUN:` lines and touches nothing: no
//! lock, no host, not even a connection. With `--apply` it rolls out, printing `+
//! <step>` as each step starts. `--pause-file FILE` is the pause control: while the
//! file exists the rollout takes no further host (it finishes the one it is on).
//!
//! Exit codes: 0 done (or planned), 1 a host failed (`FAIL <host>: ...` on stderr, with
//! where the host was left), 2 refused before any step (`REFUSED: ...` on stderr), 3
//! stopped with nothing wrong: paused, or a host still had work in flight and was left
//! as it was.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use kbf_host::plan::DRY_RUN_NOTE;

use super::engine::{self, Limits, Outcome};
use super::inventory::Inventory;
use super::plan::Plan;
use super::transport::{Commit, RunId, Transport};

#[derive(Args)]
pub struct RolloutArgs {
    /// The hosts and their roles, one `NAME ROLE` per line.
    #[arg(long)]
    inventory: PathBuf,
    /// The commit to put on every host: 40 lower-case hexadecimal digits.
    #[arg(long, value_parser = Commit::new)]
    commit: Commit,
    /// Who is rolling; the name the fleet lock is held under.
    #[arg(long, value_parser = RunId::new)]
    run_id: RunId,
    /// How long to wait for the frontend's open Execute calls to end, in seconds.
    #[arg(long, default_value_t = 6 * 60 * 60)]
    idle_timeout: u64,
    /// While this file exists, take no further host.
    #[arg(long)]
    pause_file: Option<PathBuf>,
    /// Carry the rollout out.
    #[arg(long)]
    apply: bool,
}

/// Run `rollout`: plan it, or with `--apply` carry it out through `transport`. This
/// build has no transport to the hosts yet, so a binary passes `None` and `--apply` is
/// refused.
pub fn run(
    args: RolloutArgs,
    transport: Option<&mut dyn Transport>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let plan = match load(&args) {
        Ok(plan) => plan,
        Err(why) => return refused(err, &why),
    };
    if !args.apply {
        for step in plan.steps() {
            let _ = writeln!(out, "DRY-RUN: {step}");
        }
        let _ = writeln!(out, "{DRY_RUN_NOTE}");
        return ExitCode::SUCCESS;
    }
    let Some(transport) = transport else {
        return refused(
            err,
            "this build has no transport to the hosts, so --apply cannot run",
        );
    };
    let limits = Limits {
        frontend_idle: Duration::from_secs(args.idle_timeout),
        ..Limits::default()
    };
    let pause_file = args.pause_file;
    let paused = || pause_file.as_deref().is_some_and(Path::exists);
    match engine::run(&plan, transport, &limits, &paused, out) {
        Err(e) => refused(err, &e.to_string()),
        Ok(outcome) => finish(&plan, &outcome, out, err),
    }
}

fn load(args: &RolloutArgs) -> Result<Plan, String> {
    let text = fs::read_to_string(&args.inventory)
        .map_err(|e| format!("cannot read {}: {e}", args.inventory.display()))?;
    let inventory =
        Inventory::parse(&text).map_err(|e| format!("{}: {e}", args.inventory.display()))?;
    Ok(Plan::new(
        &inventory,
        args.run_id.clone(),
        args.commit.clone(),
    ))
}

fn refused(err: &mut dyn Write, why: &str) -> ExitCode {
    let _ = writeln!(err, "REFUSED: {why}");
    ExitCode::from(2)
}

fn finish(plan: &Plan, outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match outcome {
        Outcome::Done { hosts } => {
            let _ = writeln!(out, "rollout done: {hosts} hosts are on {}", plan.commit);
            ExitCode::SUCCESS
        }
        Outcome::Paused { done } => {
            let total = plan.hosts().len();
            let _ = writeln!(
                out,
                "rollout paused: {done} of {total} hosts done, the rest untouched"
            );
            ExitCode::from(3)
        }
        Outcome::Pending { host, why } => {
            let _ = writeln!(
                out,
                "rollout pending: {host}: {why}; the host was left as it was"
            );
            ExitCode::from(3)
        }
        Outcome::Failed(f) => {
            let _ = writeln!(err, "FAIL {}: {}; {}", f.host, f.why, f.recovery);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

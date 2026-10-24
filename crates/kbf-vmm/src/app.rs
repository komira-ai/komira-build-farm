//! Dispatch: runs the command the line names and prints its JSON.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::cli::{BenchArgs, Cli, Cmd, Common, ImageCmd, ProbeArgs};
use crate::image::BaseArgs;
use crate::probe::{Ctx, Timing};
use crate::report::{Report, Status};
use crate::system::System;
use crate::{bench, gates, image};

fn name_of(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Probe(_) => "probe",
        Cmd::ProbeBench(_) => "probe-bench",
        Cmd::Image(_) => "image",
        Cmd::VmRun { .. } => "vm-run",
    }
}

fn context<'a>(
    sys: &'a dyn System,
    common: &Common,
    timing: Timing,
    (keep, graphics): (bool, bool),
) -> Result<Ctx<'a>, String> {
    std::fs::create_dir_all(&common.work_dir)
        .map_err(|e| format!("create {}: {e}", common.work_dir.display()))?;
    // Absolute, because a helper started as another user need not share the directory.
    let work = std::path::absolute(&common.work_dir).unwrap_or_else(|_| common.work_dir.clone());
    let base = common.base.clone().unwrap_or_else(|| work.join("base"));
    Ok(Ctx {
        sys,
        work,
        base,
        guest_binary: common.guest_binary.clone(),
        keep,
        graphics,
        timing,
    })
}

fn probe(sys: &dyn System, args: &ProbeArgs, timing: Timing) -> Report {
    match context(sys, &args.common, timing, (args.keep, args.graphics)) {
        Ok(ctx) => gates::run(&ctx, args.gate, args.run_as_uid),
        Err(e) => Report::error("probe", e),
    }
}

fn bench_cmd(sys: &dyn System, args: &BenchArgs, timing: Timing) -> Report {
    match context(sys, &args.common, timing, (false, false)) {
        Ok(ctx) => bench::run(&ctx, args.runs),
        Err(e) => Report::error("probe-bench", e),
    }
}

/// Runs the command. `sys` is `None` where there is no hypervisor to drive (a platform
/// other than macOS): everything but `--help` and `--version` then refuses with status 2.
pub fn run(
    cli: Cli,
    sys: Option<&dyn System>,
    timing: Timing,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let Some(sys) = sys else {
        let _ = writeln!(
            err,
            "kbf-vmm: `{}` needs macOS with Virtualization.framework (the commands that touch a VM run on macOS only)",
            name_of(&cli.command)
        );
        return ExitCode::from(2);
    };
    let report = match &cli.command {
        Cmd::VmRun { spec } => return vm_run(sys, spec, err),
        Cmd::Probe(args) => probe(sys, args, timing),
        Cmd::ProbeBench(args) => bench_cmd(sys, args, timing),
        Cmd::Image(ImageCmd::Base {
            ipsw,
            sha256,
            out,
            disk_gib,
            offline_setup,
        }) => image::base(
            sys,
            &BaseArgs {
                ipsw: ipsw.clone(),
                sha256: sha256.clone(),
                out: out.clone(),
                disk_gib: *disk_gib,
                offline_setup: *offline_setup,
            },
        ),
        Cmd::Image(ImageCmd::Setup { base }) => image::setup(sys, base),
        Cmd::Image(ImageCmd::Harvest { bundle, out }) => image::harvest(sys, bundle, out),
    };
    let failed = report.status == Status::Error;
    let _ = writeln!(out, "{}", report.on_host(sys.host_facts()).to_line());
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The helper mode: its events go to stdout as the VM runs.
fn vm_run(sys: &dyn System, spec: &Path, err: &mut dyn Write) -> ExitCode {
    let parsed = std::fs::read_to_string(spec)
        .map_err(|e| format!("read {}: {e}", spec.display()))
        .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("{}: {e}", spec.display())));
    match parsed.and_then(|s| sys.run_vm_helper(&s)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-vmm vm-run: {e}");
            ExitCode::FAILURE
        }
    }
}

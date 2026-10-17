//! `kbf-helper run` and `kbf-helper boot-check` (see the library's docs). Configuration is
//! by flags only; the unit file that starts the helper is root's.

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use kbf_helper::cycle::{Config, Helper};
use kbf_helper::restart::Restart;
use kbf_host::{HostExec, HostPath, Root};
use kbf_installer::activate;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::release;
use kbf_installer::run::SystemRunner;
use kbf_installer::settings::Settings;
use kbf_installer::stage::{Context, Limits, stage_from_inbox};

#[derive(Parser)]
#[command(name = "kbf-helper", version)]
struct Cli {
    /// The directory the host's paths are under (for a rehearsal; a host's is `/`).
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Poll the inbox and act on it, until stopped.
    Run(RunArgs),
    /// Run before each start of the service: count the start of a release on trial, and
    /// revert one that has used its starts. Exits 0 unless the host's state is unreadable.
    BootCheck(BootArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Root's directory: status.json goes here; `inbox/` (the service user's) is beneath it.
    #[arg(long, default_value = "/var/lib/kbf-updater", value_parser = HostPath::new)]
    update_dir: HostPath,
    /// The systemd unit to restart after a swap (Linux).
    #[arg(
        long,
        conflicts_with = "launchd_label",
        required_unless_present = "launchd_label"
    )]
    unit: Option<String>,
    /// The launchd label to kickstart after a swap (macOS).
    #[arg(long)]
    launchd_label: Option<String>,
    /// Seconds between passes.
    #[arg(long, default_value_t = 3)]
    interval_secs: u64,
    /// The confirm window the boot check gives a release on trial (120 to 1800).
    #[arg(long, default_value_t = 900)]
    window_secs: u64,
    /// The starts a release on trial gets before it is reverted (Linux).
    #[arg(long, default_value_t = 3)]
    starts: u32,
    /// Stop after this many passes (a rehearsal or a test); without it, run until stopped.
    #[arg(long)]
    passes: Option<u64>,
}

#[derive(clap::Args)]
struct BootArgs {
    #[arg(long, default_value_t = 900)]
    window_secs: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host(root: PathBuf) -> Result<(Layout, HostExec), String> {
    let root = Root::new(root).map_err(|e| e.to_string())?;
    Ok((
        Layout::new(root.clone(), Platform::current()),
        HostExec::new(root),
    ))
}

fn run(root: PathBuf, args: RunArgs) -> Result<(), String> {
    let (layout, exec) = host(root.clone())?;
    // clap makes exactly one of the two flags present (`required_unless_present`,
    // `conflicts_with`), so the label is there whenever the unit is not.
    let restart = match args.unit {
        Some(unit) => Restart::Systemd(unit),
        None => Restart::Launchd(args.launchd_label.unwrap_or_default()),
    };
    let cfg = Config {
        update_dir: args.update_dir,
        restart,
        window_secs: args.window_secs,
        starts: args.starts,
    };
    let mut helper = Helper::new(layout.clone(), exec, cfg);
    let trust = HostExec::new(Root::new(root).map_err(|e| e.to_string())?);
    let mut stager = move |exec: &mut HostExec, entry: &std::path::Path| {
        let settings = Settings::load(&trust, &layout)?;
        let mut ctx = Context {
            host: &layout,
            settings: &settings,
            exec,
            trust: &trust,
            runner: &SystemRunner,
            owner: release::ROOT,
            limits: Limits::DEFAULT,
        };
        stage_from_inbox(&mut ctx, entry)
    };
    helper.recover().map_err(|e| e.to_string())?;
    let mut left = args.passes;
    while left != Some(0) {
        helper
            .cycle(now(), &mut stager)
            .map_err(|e| e.to_string())?;
        helper.drain_log(&mut std::io::stderr());
        left = left.map(|n| n - 1);
        sleep(Duration::from_secs(args.interval_secs));
    }
    Ok(())
}

fn boot_check(root: PathBuf, args: &BootArgs) -> Result<(), String> {
    let (layout, mut exec) = host(root)?;
    let mut out = Vec::new();
    let result = activate::boot_check(&mut exec, &layout, now(), args.window_secs, &mut out);
    eprint!("{}", String::from_utf8_lossy(&out));
    match result {
        Ok(decision) => {
            eprintln!("kbf-helper: boot check: {decision:?}");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.verb {
        Verb::Run(args) => run(cli.root, args),
        Verb::BootCheck(args) => boot_check(cli.root, &args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kbf-helper: {why}");
            ExitCode::FAILURE
        }
    }
}

//! `kbf-coverage`: prints per-crate line and branch coverage from an lcov tracefile and
//! checks it against the `coverage-baseline` ratchet. See the library docs.
//!
//! Exit status: 0 when every crate passes, 1 when the ratchet fails, 2 when an input
//! cannot be read or parsed (and for usage errors, as clap reports them).

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use kbf_coverage::baseline::{self, Baseline, Entry};
use kbf_coverage::{lcov, ratchet};

/// Per-crate coverage table and ratchet.
#[derive(Debug, Parser)]
struct Args {
    /// The lcov tracefile `cargo llvm-cov --lcov` wrote.
    #[arg(long)]
    lcov: PathBuf,
    /// The workspace root, spelled as the tracefile's source paths begin.
    #[arg(long)]
    root: PathBuf,
    /// The baseline file to check against.
    #[arg(long)]
    baseline: PathBuf,
    /// Also write the measured coverage, as a baseline file, to this path.
    #[arg(long)]
    write_baseline: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("kbf-coverage: {e}");
            ExitCode::from(2)
        }
    }
}

fn read(path: &Path) -> Result<String, Box<dyn Error>> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()).into())
}

/// Prints the table; returns whether the ratchet passed.
fn run(args: &Args) -> Result<bool, Box<dyn Error>> {
    let files =
        lcov::parse(&read(&args.lcov)?).map_err(|e| format!("{}: {e}", args.lcov.display()))?;
    let crates = ratchet::workspace_crates(&args.root)
        .map_err(|e| format!("list crates under {}: {e}", args.root.display()))?;
    let measured = ratchet::by_crate(&args.root, &crates, &files)?;
    // Written before the check, so a failing run still leaves the file to copy.
    if let Some(path) = &args.write_baseline {
        let now: Baseline = measured
            .iter()
            .map(|(k, c)| (k.clone(), Entry::of(*c)))
            .collect();
        std::fs::write(path, baseline::render(&now))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    let recorded = baseline::parse(&read(&args.baseline)?)
        .map_err(|e| format!("{}: {e}", args.baseline.display()))?;
    let rows = ratchet::rows(&measured, &recorded);
    print!("{}", ratchet::render(&rows));
    Ok(!rows.iter().any(|r| r.status().fails()))
}

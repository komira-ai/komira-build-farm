//! `kbf-daemon`: the worker daemon. A thin main over the `kbf_node` library, which
//! holds the flags, the driver set-up and the serve loop, so another binary can run
//! the daemon beside the server. Configuration is by flags only (`kbf-daemon --help`);
//! the flags and what each driver does are documented there.

use std::process::ExitCode;

use clap::Parser;
use kbf_node::{Cli, dispatch_helpers, run};

fn main() -> ExitCode {
    if let Some(code) = dispatch_helpers() {
        return code;
    }
    run(&Cli::parse())
}

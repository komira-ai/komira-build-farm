//! `kbf-no-podman`: a `podman` that must never run, for `tests/binary.rs`, which links it
//! as `podman` into a directory it puts first in `PATH`. kbf runs containers with `crun`
//! only; if the daemon ever starts `podman`, this logs its arguments, one per line, to
//! `podman.log` beside the link it is run through, says so on stderr and fails (exit
//! 125, as Podman does when it cannot do what it was asked). A test then asserts that
//! `podman.log` does not exist.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().expect("an argv0");
    // The link this was run through: argv0 itself, or as `PATH` finds a bare name.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let link = std::iter::once(PathBuf::from(&argv0))
        .chain(std::env::split_paths(&path).map(|dir| dir.join(&argv0)))
        .find(|p| p.is_absolute() && p.is_file())
        .expect("the link this was run through");
    let here = link.parent().expect("the link's directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(here.join("podman.log"))
        .expect("open podman.log");
    for arg in args {
        writeln!(log, "{arg}").expect("write podman.log");
    }
    eprintln!("kbf-no-podman: podman was removed; nothing in kbf runs it");
    ExitCode::from(125)
}

//! `kbf-mac-session`: see the library's documentation.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    kbf_mac_session::start::main(&kbf_mac_session::start::Args::parse())
}

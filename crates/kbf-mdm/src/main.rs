//! `kbf-mdm-gate`: see [`kbf_mdm::config`].
//!
//! On start it prints one line, `kbf-mdm-gate <version> listen=<addr>`, once its
//! signal handlers are installed and its listener is bound. SIGTERM or SIGINT stops it
//! with exit 0; bad flags or a failed start exit 2.

fn main() -> std::process::ExitCode {
    kbf_mdm::config::main(std::env::args_os().collect())
}

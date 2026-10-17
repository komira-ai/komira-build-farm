//! `kbf-server`: serves REAPI and `kbf.worker.v1` until interrupted. See the library
//! docs for what it wires together.
//!
//! On start it prints one line, `kbf-server <version> reapi=<addr> worker=<addr>`, with
//! its version (`<package version>+<commit>`, [`kbf_server::SERVER_VERSION`]) and
//! the addresses it bound (a port of 0 picks a free one), and ` invocations=<on|off>
//! blobs=<on|off> api=<addr>` at its end when the operator API listens
//! (`invocations=on` when the REAPI policy names `invocations.getAuthorizer` or
//! `--invocations-open` is given, so the build pages answer; `blobs=on` when it also
//! names `blobs.getAuthorizer`, or the flag is given, so `GET /v1/blobs` answers), then
//! ` log=<id>`, the id of
//! the metadata log (16 hex digits), then
//! ` warning=plaintext-off-loopback:<listeners>` at its very end when a listener serves
//! plain text on an address that is not loopback (`<listeners>` is `reapi`, `worker`
//! or `reapi,worker`; see [`Args::plain_text_off_loopback`]). The server starts either
//! way. On Unix the SIGINT and SIGTERM handlers are installed before that line is
//! printed, so either signal any time after it stops the server with exit 0: new
//! Execute calls are refused UNAVAILABLE, REAPI connections are sent GOAWAY, open
//! Execute and WaitExecution streams run until their action is done (Buck2 does not
//! recover a broken stream), and the server exits once they close; when
//! `--shutdown-timeout-secs` is set and runs out, the open streams end UNAVAILABLE, and
//! so they do at a second SIGINT or SIGTERM, which stops the server after a short
//! grace. `POST /v1/frontend/drain` on the operator API begins the same drain with no
//! signal. If a handler cannot be installed it exits 2 without printing the start line.
//!
//! Events go to stderr and, with `--log-dir` (by default `log/` in the `--hot-store`
//! directory), also to a size-capped rotating file (`kbf_server::log_file`); an INFO
//! line right after start says which. The tracing subscriber is installed after the
//! flags are parsed, since they say where the file is.
//!
//! The REAPI listener runs the policy `--reapi-auth-policy` names; a policy file that
//! cannot be read or is not a policy exits 2 before anything is bound.
//!
//! The metadata log (a single-voter Raft log) opens in `--meta-dir`, or in `meta/` of
//! the `--hot-store` directory, before anything is bound (creating it if the directory
//! is empty), and objects are written under the prefix the log committed; a directory
//! it refuses exits 2. `--cold-store` is the durable object store, `file:///dir` (a
//! `kbf_objstore::FsStore`, locked while the server runs, so a second server on it, or
//! a directory that cannot be created or locked, exits 2 before anything is bound) or
//! `s3://bucket/prefix?endpoint=...`; the start logs every store, path and budget it
//! resolved.
//!
//! `--hot-store` attaches the local write tier before anything is bound
//! (`kbf_server::write_tier`): it first raises the log to machine version 2 (once; a
//! server without a tier never does, so its log stays one an older binary reads). A
//! start the tier refuses (a wrong or missing `TIER` while objects are staged, a log
//! that cannot be raised) exits 2. Its flusher runs every second until the server stops.

use std::error::Error;
use std::process::ExitCode;

use clap::Parser;
use kbf_server::{Args, sizes_on, start_interruptible};
use kbf_threads::Topology;

fn main() -> ExitCode {
    let args = Args::parse();
    kbf_server::log_file::init(&args);
    match serve(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-server: {e}");
            ExitCode::from(2)
        }
    }
}

/// Builds the async runtime the thread flags size, then starts the server on it, prints
/// the start line and serves until a signal.
fn serve(args: &Args) -> Result<(), Box<dyn Error>> {
    let sizes = sizes_on(args, Topology::detect());
    sizes.log();
    sizes.runtime()?.block_on(async {
        let server = start_interruptible(args).await?;
        println!("{}", server.start_line());
        Ok(server.serve().await?)
    })
}

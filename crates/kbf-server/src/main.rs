//! `kbf-server`: serves REAPI and `kbf.worker.v1` until interrupted. See the library
//! docs for what it wires together.
//!
//! On start it prints one line, `kbf-server <version> reapi=<addr> worker=<addr>`, with
//! the addresses it bound (a port of 0 picks a free one). On Unix the SIGINT handler
//! is installed before that line is printed, so a SIGINT any time after it stops the
//! server with exit 0. If the handler cannot be installed it exits 2 without printing
//! the start line.

use std::error::Error;
use std::future::Future;
use std::io;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore, ObjectStore};
use kbf_server::{Args, StoreKind, bind_server};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let result = match args.store {
        StoreKind::Memory => {
            let store = MemoryStore::new(Capabilities::default());
            run(&args, store, KeyPrefix::default()).await
        }
        StoreKind::S3 => match args.s3_store(|name| std::env::var(name).ok()) {
            Ok((store, prefix)) => run(&args, store, prefix).await,
            Err(e) => Err(e.into()),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-server: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run<O: ObjectStore + 'static>(
    args: &Args,
    store: O,
    prefix: KeyPrefix,
) -> Result<(), Box<dyn Error>> {
    let listeners = args.listeners()?;
    let cache = Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        store,
        prefix,
    ));
    let shutdown = interrupted()?;
    let bound = bind_server(cache, listeners, shutdown)?;
    println!(
        "kbf-server {} reapi={} worker={}",
        env!("CARGO_PKG_VERSION"),
        bound.reapi,
        bound.worker
    );
    bound.serving.await?;
    Ok(())
}

/// Installs the SIGINT handler now and returns a future that completes on SIGINT.
///
/// `tokio::signal::ctrl_c()` installs its handler only when first polled, which is
/// after the start line is printed; a SIGINT in between killed the process by the
/// default action instead of stopping it cleanly (issue #86). `signal` installs the
/// handler when it is called.
///
/// # Errors
/// The handler cannot be installed.
#[cfg(unix)]
fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    Ok(async move {
        sigint.recv().await;
    })
}

/// Returns a future that completes on Ctrl-C. Elsewhere than Unix the handler is
/// installed when the future is first polled, after the start line is printed.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "the same signature as on Unix")]
fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

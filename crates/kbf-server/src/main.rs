//! `kbf-server`: serves REAPI and `kbf.worker.v1` until interrupted. See the library
//! docs for what it wires together.
//!
//! On start it prints one line, `kbf-server <version> reapi=<addr> worker=<addr>`, with
//! the addresses it bound (a port of 0 picks a free one).

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
            Err(e) => Err(e.to_string()),
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
) -> Result<(), String> {
    let listeners = args.listeners().map_err(|e| e.to_string())?;
    let cache = Arc::new(Cache::new(
        MemoryMetaLog::new(Retention::default()),
        store,
        prefix,
    ));
    let shutdown = async {
        // Without a signal handler the process cannot stop cleanly; it still stops.
        let _ = tokio::signal::ctrl_c().await;
    };
    let bound = bind_server(cache, listeners, shutdown).map_err(|e| e.to_string())?;
    println!(
        "kbf-server {} reapi={} worker={}",
        env!("CARGO_PKG_VERSION"),
        bound.reapi,
        bound.worker
    );
    bound.serving.await.map_err(|e| e.to_string())
}

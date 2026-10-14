//! The process's transfer engine (`kbf_transfer::Transfers`) and its `--transfer-*`
//! flags (`docs/design/worker-cache.md#45-knobs-flags-not-environment-variables`).
//!
//! One engine per daemon: every runtime (container, native, whole-machine) reads and
//! writes blobs through the same one, so the in-flight limit and the byte budget hold
//! for the whole process, over one HTTP/2 connection to `--cas`.

use kbf_transfer::{
    DEFAULT_IN_FLIGHT, DEFAULT_SEGMENT_STREAMS, DEFAULT_SEGMENT_UPLOADS, TransferConfig, Transfers,
};
use tonic::transport::Endpoint;

use super::{Cli, Error};
use kbf_daemon::DaemonConfig;

/// The `--transfer-*` flags. Each default is the engine's (`TransferConfig::default`);
/// the farm's deploy config sets measured values.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct TransferArgs {
    /// The most blob requests in flight at once, for the whole daemon: ByteStream reads
    /// and writes, segment ranges and FindMissing calls, over its one connection to
    /// `--cas`. Uploads hold at most all but a quarter of them.
    #[arg(long, default_value_t = DEFAULT_IN_FLIGHT as u64, value_parser = clap::value_parser!(u64).range(1..))]
    pub transfer_in_flight: u64,
    /// The most bytes of blobs read into memory in flight at once, for the whole
    /// daemon. A blob larger than this is fetched alone.
    #[arg(long, value_name = "BYTES", default_value_t = 256 << 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub transfer_bytes_in_flight: u64,
    /// The largest input file fetched through memory, where every lease asking for it
    /// at once shares one fetch; a larger one is streamed into its file.
    #[arg(long, value_name = "BYTES", default_value_t = 1 << 20)]
    pub transfer_memory_blob_bytes: u64,
    /// The most bytes one ByteStream write message carries.
    #[arg(long, value_name = "BYTES", default_value_t = 1 << 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub transfer_chunk_bytes: u64,
    /// The most digests one FindMissing call asks about.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    pub transfer_find_missing_chunk: u64,
    /// The HTTP/2 window of each stream to `--cas`; the connection's is eight times it.
    #[arg(long, value_name = "BYTES", default_value_t = 8 << 20, value_parser = clap::value_parser!(u32).range(65_535..=(1 << 28)))]
    pub transfer_stream_window_bytes: u32,
    /// How many times a blob request is retried after UNAVAILABLE or a broken
    /// connection.
    #[arg(long, default_value_t = 3)]
    pub transfer_retries: u32,
    /// How many `PutRange` calls one segment upload of an action's outputs has in
    /// flight at once, as HTTP/2 streams on the one connection to `--cas`
    /// (`docs/design/write-path.md`, section 4.1); also the parallel `ReadRange` calls
    /// of a segment-range read.
    #[arg(long, default_value_t = DEFAULT_SEGMENT_STREAMS as u64, value_parser = clap::value_parser!(u64).range(1..))]
    pub segment_upload_streams: u64,
    /// How many segment uploads of one action's outputs are in flight at once: the
    /// server checks and stages one upload while the next are sending
    /// (`docs/design/write-path.md`, section 4.1). Their `PutRange` calls still take
    /// permits of `--transfer-in-flight`'s uploads' share.
    #[arg(long, default_value_t = DEFAULT_SEGMENT_UPLOADS as u64, value_parser = clap::value_parser!(u64).range(1..))]
    pub segment_uploads_in_flight: u64,
}

impl TransferArgs {
    /// The engine's configuration.
    pub(crate) fn config(&self) -> TransferConfig {
        let size = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        TransferConfig {
            in_flight: size(self.transfer_in_flight),
            bytes_in_flight: size(self.transfer_bytes_in_flight),
            memory_blob_bytes: self.transfer_memory_blob_bytes,
            chunk_bytes: size(self.transfer_chunk_bytes),
            find_missing_chunk: size(self.transfer_find_missing_chunk),
            stream_window_bytes: self.transfer_stream_window_bytes,
            retries: self.transfer_retries,
            segment_streams: size(self.segment_upload_streams),
            segment_uploads: size(self.segment_uploads_in_flight),
        }
    }
}

/// The engine over the blob service `--cas` names, over mutual TLS with the daemon's
/// own TLS files and the daemon's HTTP/2 keepalive. Connects on first use.
pub(crate) fn engine(cli: &Cli) -> Result<Transfers, Error> {
    let url = cli
        .cas
        .clone()
        .ok_or("--cas is required by the container and native drivers")?;
    if !url.starts_with("https://") {
        return Err(format!(
            "--cas {url:?} must be an https:// URL: the server's worker listener, which \
             serves blobs over mutual TLS only"
        )
        .into());
    }
    let config = DaemonConfig::from_args(&cli.daemon);
    let endpoint = Endpoint::from_shared(url)?.tls_config(config.tls.load()?)?;
    Ok(Transfers::new(
        config.keep_alive.apply(endpoint),
        cli.transfer.config(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clap::Parser as _;
    use kbf_transfer::TransferConfig;

    use super::super::{Cli, Node};

    fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
        let base = [
            "kbf-daemon",
            "--server=https://front:7070",
            "--ca-cert=ca.pem",
            "--cert=c.pem",
            "--key=c.key",
            "--node-id=n1",
            "--driver=native",
        ];
        Cli::try_parse_from(base.iter().chain(extra))
    }

    /// Catches a `--transfer-*` flag (or `--segment-upload-streams`) that does not reach
    /// the engine, or one whose default is not the engine's own (a deploy that sets none
    /// would run other limits than the design's). Each value given differs from its
    /// default.
    #[test]
    fn transfer_flags_reach_the_configuration() {
        let defaults = parse(&[]).expect("flags").transfer.config();
        assert_eq!(defaults, TransferConfig::default());
        let cli = parse(&[
            "--transfer-in-flight=7",
            "--transfer-bytes-in-flight=1000",
            "--transfer-memory-blob-bytes=2000",
            "--transfer-chunk-bytes=3000",
            "--transfer-find-missing-chunk=40",
            "--transfer-stream-window-bytes=100000",
            "--transfer-retries=5",
            "--segment-upload-streams=9",
            "--segment-uploads-in-flight=6",
        ])
        .expect("flags");
        let expected = TransferConfig {
            in_flight: 7,
            bytes_in_flight: 1000,
            memory_blob_bytes: 2000,
            chunk_bytes: 3000,
            find_missing_chunk: 40,
            stream_window_bytes: 100_000,
            retries: 5,
            segment_streams: 9,
            segment_uploads: 6,
        };
        assert_eq!(cli.transfer.config(), expected);
        for refused in [
            "--transfer-in-flight=0",
            "--transfer-chunk-bytes=0",
            "--transfer-stream-window-bytes=100",
            "--segment-upload-streams=0",
            "--segment-uploads-in-flight=0",
        ] {
            assert!(parse(&[refused]).is_err(), "{refused} accepted");
        }
    }

    /// Catches a transfer engine per runtime (two drivers would run two connections and
    /// two in-flight limits): the second runtime gets the engine the first made.
    #[tokio::test]
    async fn every_runtime_shares_one_engine() {
        // The daemon's TLS files, which the engine reads once.
        let dir = super::super::tests::scratch("transfer-shared");
        super::super::tests::tls_files(&dir);
        let at = |name: &str| format!("{}", dir.join(name).display());
        let cli = Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://front:7070".to_owned(),
            format!("--ca-cert={}", at("ca.pem")),
            format!("--cert={}", at("node.pem")),
            format!("--key={}", at("node.key")),
            "--node-id=n1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
        ])
        .expect("flags");
        let mut node = Node::default();
        let first = node.transfers(&cli).expect("engine");
        let second = node.transfers(&cli).expect("engine");
        assert!(Arc::ptr_eq(&first, &second));
    }
}

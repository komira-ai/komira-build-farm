//! Runs the conformance suite against a real S3 bucket and exits non-zero on any failure.
//!
//! The key pair comes from the standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
//! variables (a secret never goes on the command line); everything else is a flag. Each
//! run writes under a fresh prefix, because objects under Object Lock outlive the run.
//!
//! cargo run -p kbf-objstore --example s3_conformance -- \
//!     --endpoint http://127.0.0.1:9000 --bucket kbf-conformance --create-bucket \
//!     --conditional-put --object-lock

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix, conformance};

#[derive(Parser)]
struct Args {
    /// `http://host[:port]` of the S3 service.
    #[arg(long)]
    endpoint: String,
    /// The bucket to test in.
    #[arg(long)]
    bucket: String,
    /// The signing region.
    #[arg(long, default_value = "us-east-1")]
    region: String,
    /// Create the bucket first (with Object Lock if `--object-lock`).
    #[arg(long)]
    create_bucket: bool,
    /// Claim conditional writes, so the suite checks that overwrites are refused.
    #[arg(long)]
    conditional_put: bool,
    /// Claim Object Lock, so the suite checks that retained objects survive delete.
    #[arg(long)]
    object_lock: bool,
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} must be set"))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("s3_conformance: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: Args) -> Result<bool, String> {
    let credentials = Credentials::new(env("AWS_ACCESS_KEY_ID")?, env("AWS_SECRET_ACCESS_KEY")?);
    let store = S3Store::new(S3Config {
        endpoint: args.endpoint,
        region: args.region,
        bucket: args.bucket,
        credentials,
        capabilities: Capabilities {
            conditional_put: args.conditional_put,
            object_lock: args.object_lock,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
    })
    .map_err(|e| e.to_string())?;
    if args.create_bucket {
        store
            .create_bucket()
            .await
            .map_err(|e| format!("create bucket: {e}"))?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let prefix = KeyPrefix::new(format!("conformance/{nonce}/")).map_err(|e| e.to_string())?;
    let report = conformance::run(&store, &prefix).await;
    print!("{report}");
    Ok(report.passed())
}

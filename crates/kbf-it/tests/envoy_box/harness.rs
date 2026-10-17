//! The farm of one test: the box render of `kbf-deploy` run by the pinned Envoy in front
//! of two stub instances, and the helpers that talk to it.
//!
//! Durations are scaled: a test patches a timeout of the render (its hours become
//! seconds) and holds a stream longer than that, or leaves the render as it is and shows
//! that a defect it plants is cut by the same stream. No test asserts how long anything
//! took; the deadlines here only end a hang.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_deploy::envoy::box_config::{BoxConfig, Instance as InstancePorts};
use kbf_deploy::envoy::box_render::render_box;
use kbf_it::pki::{self, PkiFiles};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{ExecuteRequest, WaitExecutionRequest};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::{Status, Streaming};

use super::stub::Instance;

/// A deadline that only ends a hang.
pub const HANG: Duration = Duration::from_secs(90);

const EXAMPLE: &str = include_str!("../../../kbf-deploy/templates/envoy/box.example");

/// The pinned Envoy binary CI fetched.
fn envoy_binary() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_ENVOY")
        .expect("KBF_TEST_ENVOY names the pinned Envoy binary (CI's test-x86 job fetches it)");
    PathBuf::from(path)
}

/// The public ports of one test and the instances' block.
#[derive(Clone, Copy)]
pub struct Ports {
    pub reapi: u16,
    pub worker: u16,
    pub admin: u16,
    pub base: u16,
}

static BLOCKS: AtomicU32 = AtomicU32::new(0);
static ENVOYS: AtomicU32 = AtomicU32::new(0);

/// A block of ports nobody listens on: public REAPI, worker and admin ports, and the
/// instances' ports from `base`, 40 ports apart below the ephemeral range. Each test of
/// the process takes its own block (and processes start apart), and a block with a bound
/// port is skipped.
fn ports() -> Ports {
    loop {
        let n = BLOCKS.fetch_add(1, Ordering::SeqCst) + std::process::id() * 7;
        let block = 20_000 + (n % 250) as u16 * 40;
        let p = Ports {
            reapi: block,
            worker: block + 1,
            admin: block + 2,
            base: block + 10,
        };
        let used = [
            p.reapi,
            p.worker,
            p.admin,
            p.base,
            p.base + 1,
            p.base + 2,
            p.base + 10,
            p.base + 11,
            p.base + 12,
        ];
        if used
            .iter()
            .all(|&port| std::net::TcpListener::bind(("0.0.0.0", port)).is_ok())
        {
            return p;
        }
    }
}

/// The example config with `key` set to `value`.
fn set_key(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut found = false;
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            if l.starts_with(&prefix) {
                found = true;
                format!("{prefix}{value}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    assert!(found, "{key} is not in the example");
    lines.join("\n") + "\n"
}

/// What a test chooses.
pub struct Opts {
    /// `KBF_BOX_STREAM_CEILING` and `KBF_BOX_REAPI_MAX_STREAMS`.
    pub ceiling: u32,
    pub max_streams: u32,
    /// Envoy's hot-restart flags are the render's, with these two values in seconds
    /// (`--parent-shutdown-time-s`, `--drain-time-s`) in place of the longest action's.
    pub hot_restart: Option<(u64, u64)>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            ceiling: 65536,
            max_streams: 10_000,
            hot_restart: None,
        }
    }
}

/// Envoy, two instances and the way in.
pub struct Farm {
    pub dir: PathBuf,
    pub ports: Ports,
    pub pki: PkiFiles,
    pub a: Arc<Instance>,
    pub b: Arc<Instance>,
    /// The loopback ports of `a` and `b`.
    pub inst: [InstancePorts; 2],
    launch: Launch,
    /// Epoch 0 of Envoy.
    pub envoy: Child,
}

/// What it takes to start an Envoy epoch on this farm's config.
struct Launch {
    dir: PathBuf,
    config: PathBuf,
    args: Vec<String>,
    base_id: u32,
}

impl Launch {
    fn spawn(&self, epoch: u32) -> Child {
        let log = std::fs::File::create(self.dir.join(format!("envoy-{epoch}.log"))).expect("log");
        let mut cmd = Command::new(envoy_binary());
        cmd.arg("--config-path")
            .arg(&self.config)
            .args(["--concurrency", "1", "--log-level", "warn"])
            .args(["--base-id", &self.base_id.to_string()])
            .args(["--restart-epoch", &epoch.to_string()])
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("log")))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        cmd.spawn().expect("start envoy")
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

impl Farm {
    /// Render the box, change the rendered `envoy.json` with `patch`, start the two
    /// instances and Envoy (epoch 0), and wait until Envoy is live.
    pub async fn start(name: &str, opts: &Opts, patch: impl FnOnce(&mut Value)) -> Farm {
        let dir = scratch(name);
        let ports = ports();
        let pki = pki::write(&dir, "envoy-box").expect("certificates");
        let path = |p: &Path| p.to_str().expect("utf-8").to_owned();
        let mut text = EXAMPLE.to_owned();
        for (key, value) in [
            ("KBF_BOX_HOST", pki::SERVER_NAME.to_owned()),
            ("KBF_BOX_REAPI_PORT", ports.reapi.to_string()),
            ("KBF_BOX_WORKER_PORT", ports.worker.to_string()),
            ("KBF_BOX_ADMIN_PORT", ports.admin.to_string()),
            ("KBF_BOX_BACKEND_PORT_BASE", ports.base.to_string()),
            ("KBF_BOX_CERT_FILE", path(&pki.server_cert)),
            ("KBF_BOX_KEY_FILE", path(&pki.server_key)),
            ("KBF_BOX_CA_FILE", path(&pki.ca_cert)),
            ("KBF_BOX_REAPI_MAX_STREAMS", opts.max_streams.to_string()),
            ("KBF_BOX_STREAM_CEILING", opts.ceiling.to_string()),
        ] {
            text = set_key(&text, key, &value);
        }
        let config = BoxConfig::parse(&text).expect("the box config");
        let rendered = render_box(&config);
        let mut envoy: Value = serde_json::from_str(&rendered.envoy).expect("JSON");
        patch(&mut envoy);
        let file = dir.join("envoy.json");
        std::fs::write(&file, serde_json::to_string_pretty(&envoy).expect("print")).expect("write");
        let [pa, pb] = config.instances();
        let a = Instance::start("a", &pki, pa, opts.max_streams).await;
        let b = Instance::start("b", &pki, pb, opts.max_streams).await;
        let args = match opts.hot_restart {
            None => vec!["--disable-hot-restart".to_owned()],
            Some((parent, drain)) => hot_restart_args(&rendered.args, parent, drain),
        };
        let base_id =
            100 + std::process::id() % 10_000 * 10 + ENVOYS.fetch_add(1, Ordering::SeqCst);
        let launch = Launch {
            dir: dir.clone(),
            config: file,
            args,
            base_id,
        };
        let envoy = launch.spawn(0);
        let farm = Farm {
            dir,
            ports,
            pki,
            a,
            b,
            inst: [pa, pb],
            launch,
            envoy,
        };
        farm.wait_live().await;
        farm
    }

    /// Start an Envoy epoch on this farm's config.
    pub fn spawn(&self, epoch: u32) -> Child {
        self.launch.spawn(epoch)
    }

    /// The log of one epoch, for a failure message.
    pub fn log(&self, epoch: u32) -> String {
        std::fs::read_to_string(self.dir.join(format!("envoy-{epoch}.log"))).unwrap_or_default()
    }

    /// Wait until the admin interface says live, which is after the first health checks.
    pub async fn wait_live(&self) {
        wait(|| async { admin(self.ports.admin, "/ready").await.contains("LIVE") })
            .await
            .unwrap_or_else(|| panic!("Envoy never went live:\n{}", self.log(0)));
    }

    /// Wait until the admin interface answers for `epoch`.
    pub async fn wait_epoch(&self, epoch: u64) {
        wait(|| async {
            let info = admin(self.ports.admin, "/server_info").await;
            let v: Value = serde_json::from_str(&info).unwrap_or(Value::Null);
            v["command_line_options"]["restart_epoch"] == epoch
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "no admin answer for epoch {epoch}:\n{}",
                self.log(epoch as u32)
            )
        });
    }

    /// A channel to Envoy's REAPI listener.
    pub async fn channel(&self) -> Channel {
        let ca = std::fs::read(&self.pki.ca_cert).expect("CA");
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca))
            .domain_name(pki::SERVER_NAME);
        Endpoint::from_shared(format!("https://127.0.0.1:{}", self.ports.reapi))
            .expect("endpoint")
            .tls_config(tls)
            .expect("tls")
            .connect()
            .await
            .expect("connect to Envoy")
    }

    /// Wait until Envoy's admin interface says the host at `port` in `cluster` failed its
    /// active health check.
    pub async fn wait_unhealthy(&self, cluster: &str, port: u16) {
        wait(|| async { self.failed_check(cluster, port).await })
            .await
            .unwrap_or_else(|| {
                let clusters = std::fs::read_to_string(self.dir.join("clusters.json"));
                panic!("{cluster} host {port} never failed its check: {clusters:?}")
            });
    }

    async fn failed_check(&self, cluster: &str, port: u16) -> bool {
        let text = admin(self.ports.admin, "/clusters?format=json").await;
        // The last answer, for a failure message.
        let _ = std::fs::write(self.dir.join("clusters.json"), &text);
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let hosts = v["cluster_statuses"]
            .as_array()
            .map_or(&[][..], |a| a.as_slice());
        hosts
            .iter()
            .filter(|c| c["name"] == cluster)
            .flat_map(|c| c["host_statuses"].as_array().into_iter().flatten())
            .any(|h| {
                h["address"]["socket_address"]["port_value"] == port
                    && h["health_status"]["failed_active_health_check"] == true
            })
    }

    /// A TCP session through the worker listener.
    pub async fn session(&self) -> tokio::net::TcpStream {
        tokio::net::TcpStream::connect(("127.0.0.1", self.ports.worker))
            .await
            .expect("connect to the worker port")
    }
}

/// The flags of `envoy.args` with the two timings replaced.
fn hot_restart_args(args: &str, parent: u64, drain: u64) -> Vec<String> {
    let mut out = Vec::new();
    for line in args.lines() {
        let (flag, value) = line.split_once(' ').unwrap_or((line, ""));
        out.push(flag.to_owned());
        match flag {
            "--parent-shutdown-time-s" => out.push(parent.to_string()),
            "--drain-time-s" => out.push(drain.to_string()),
            _ if !value.is_empty() => out.push(value.to_owned()),
            _ => {}
        }
    }
    out
}

/// Poll `ok` until it holds, or [`HANG`] passes.
pub async fn wait<F, Fut>(ok: F) -> Option<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    wait_within(HANG, ok).await
}

/// Poll `ok` until it holds, or `limit` passes.
pub async fn wait_within<F, Fut>(limit: Duration, mut ok: F) -> Option<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if ok().await {
            return Some(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// `GET path` on the admin interface: the body, with a chunked encoding undone; empty if
/// it does not answer.
pub async fn admin(port: u16, path: &str) -> String {
    let Ok(mut conn) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return String::new();
    };
    let request = format!("GET {path} HTTP/1.1\r\nhost: admin\r\nconnection: close\r\n\r\n");
    if conn.write_all(request.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut raw = Vec::new();
    let _ = conn.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return String::new();
    };
    if !head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return body.to_owned();
    }
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

/// What a stream's frames came to.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// The final frame (`done`) arrived, then the stream ended cleanly.
    Done,
    /// An error: its code and message.
    Error(String),
    /// The stream ended without the final frame.
    Short,
}

/// An open `Execute` call: the instance that answered and the rest of the stream.
pub struct Call {
    pub from: String,
    stream: Streaming<Operation>,
}

impl Call {
    /// Open an `Execute` and read its first frame: `Err` with the status if Envoy or the
    /// instance refused it.
    pub async fn open(ch: &Channel) -> Result<Call, String> {
        let reply = ExecutionClient::new(ch.clone())
            .execute(ExecuteRequest::default())
            .await;
        Call::first(reply).await
    }

    /// Open a `WaitExecution` the same way.
    pub async fn wait_for(ch: &Channel) -> Result<Call, String> {
        let reply = ExecutionClient::new(ch.clone())
            .wait_execution(WaitExecutionRequest::default())
            .await;
        Call::first(reply).await
    }

    async fn first(
        reply: Result<tonic::Response<Streaming<Operation>>, Status>,
    ) -> Result<Call, String> {
        let shown = |s: Status| format!("{:?}: {}", s.code(), s.message());
        let mut stream = reply.map_err(shown)?.into_inner();
        match stream.message().await {
            Ok(Some(op)) => Ok(Call {
                from: op.name,
                stream,
            }),
            Ok(None) => Err("ended before its first frame".to_owned()),
            Err(s) => Err(shown(s)),
        }
    }

    /// Read to the end, for at most [`HANG`].
    pub async fn finish(mut self) -> End {
        let mut done = false;
        loop {
            match tokio::time::timeout(HANG, self.stream.message()).await {
                Err(_) => return End::Error("still open after the deadline".to_owned()),
                Ok(Ok(Some(op))) => done |= op.done,
                Ok(Ok(None)) if done => return End::Done,
                Ok(Ok(None)) => return End::Short,
                Ok(Err(s)) => return End::Error(format!("{:?}: {}", s.code(), s.message())),
            }
        }
    }
}

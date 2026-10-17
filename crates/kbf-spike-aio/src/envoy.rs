//! Envoy bootstrap files, an Envoy child process and its admin interface.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const HCM: &str = "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager";
const ROUTER: &str = "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router";
const TCP: &str = "type.googleapis.com/envoy.extensions.filters.network.tcp_proxy.v3.TcpProxy";
const HTTP_OPTS: &str = "envoy.extensions.upstreams.http.v3.HttpProtocolOptions";

/// One HTTP listener with its cluster, one TCP listener per worker-like cluster.
#[derive(Clone)]
pub struct Cfg {
    pub admin: u16,
    /// REAPI-like listener port and its cluster's backends.
    pub http_port: u16,
    pub backends: Vec<u16>,
    /// Route `timeout` (`None` = Envoy's default, 15 s).
    pub route_timeout: Option<&'static str>,
    /// Route `idle_timeout` (`None` = the HCM's).
    pub route_idle: Option<&'static str>,
    /// HCM `stream_idle_timeout` (`None` = 300 s).
    pub hcm_idle: Option<&'static str>,
    /// Route `max_stream_duration.max_stream_duration`.
    pub max_stream_duration: Option<&'static str>,
    /// Cluster breakers; `None` = Envoy's defaults.
    pub breakers: Option<u32>,
    /// Active health check on the clusters.
    pub health_check: bool,
    pub hc_interval: &'static str,
    /// TCP listeners: (name, port, `idle_timeout`, `close_connections_on_host_health_failure`).
    pub tcp: Vec<(&'static str, u16, Option<&'static str>, bool)>,
    /// The backends of every TCP cluster (a silent TCP server, see `stub::Silent`), checked
    /// by TCP connect.
    pub tcp_backends: Vec<u16>,
    /// The `kbf_reapi` cluster's hosts come from this file (EDS, `path_config_source`),
    /// so a host can be added to a running Envoy; `backends` is then ignored.
    pub eds: Option<PathBuf>,
}

impl Cfg {
    pub fn new(admin: u16, http_port: u16, backends: Vec<u16>) -> Cfg {
        Cfg {
            admin,
            http_port,
            backends,
            route_timeout: Some("0s"),
            route_idle: Some("0s"),
            hcm_idle: None,
            max_stream_duration: None,
            breakers: None,
            health_check: true,
            hc_interval: "1s",
            tcp: Vec::new(),
            tcp_backends: Vec::new(),
            eds: None,
        }
    }

    fn endpoints(&self, cluster: &str, backends: &[u16]) -> Value {
        let lb: Vec<Value> = backends
            .iter()
            .map(|p| json!({"endpoint": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": p}}}}))
            .collect();
        json!({"cluster_name": cluster, "endpoints": [{"lb_endpoints": lb}]})
    }

    fn cluster(&self, name: &str, http2: bool, close_on_unhealthy: bool) -> Value {
        let mut c = json!({
            "name": name,
            "type": "STATIC",
            "connect_timeout": "1s",
            "lb_policy": "ROUND_ROBIN",
            "load_assignment": self.endpoints(name, if http2 { &self.backends } else { &self.tcp_backends }),
            "common_lb_config": {"healthy_panic_threshold": {"value": 0}},
        });
        let o = c.as_object_mut().expect("object");
        if let (Some(path), "kbf_reapi") = (&self.eds, name) {
            o.remove("load_assignment");
            o.insert("type".into(), "EDS".into());
            o.insert(
                "eds_cluster_config".into(),
                json!({"eds_config": {"resource_api_version": "V3", "path_config_source": {
                    "path": path,
                    "watched_directory": {"path": path.parent().expect("a directory")},
                }}}),
            );
        }
        if http2 {
            o.insert(
                "typed_extension_protocol_options".into(),
                json!({HTTP_OPTS: {
                    "@type": format!("type.googleapis.com/{HTTP_OPTS}"),
                    "explicit_http_config": {"http2_protocol_options": {}},
                }}),
            );
        }
        if close_on_unhealthy {
            o.insert("close_connections_on_host_health_failure".into(), true.into());
        }
        if self.health_check {
            let mut hc = json!({
                "timeout": "1s",
                "interval": self.hc_interval,
                "no_traffic_interval": self.hc_interval,
                "unhealthy_threshold": 2,
                "healthy_threshold": 1,
            });
            if http2 {
                hc["grpc_health_check"] = json!({});
            } else {
                hc["tcp_health_check"] = json!({});
            }
            o.insert("health_checks".into(), json!([hc]));
        }
        if let Some(n) = self.breakers {
            o.insert(
                "circuit_breakers".into(),
                json!({"thresholds": [{
                    "max_connections": n, "max_requests": n,
                    "max_pending_requests": n, "max_retries": 3,
                }]}),
            );
        }
        c
    }

    /// The bootstrap document.
    pub fn json(&self) -> Value {
        let mut route = serde_json::Map::new();
        route.insert("cluster".into(), "kbf_reapi".into());
        if let Some(t) = self.route_timeout {
            route.insert("timeout".into(), t.into());
        }
        if let Some(t) = self.route_idle {
            route.insert("idle_timeout".into(), t.into());
        }
        if let Some(t) = self.max_stream_duration {
            route.insert("max_stream_duration".into(), json!({"max_stream_duration": t}));
        }
        let mut hcm = json!({
            "@type": HCM,
            "stat_prefix": "reapi",
            "codec_type": "AUTO",
            "route_config": {"virtual_hosts": [{
                "name": "all", "domains": ["*"],
                "routes": [{"match": {"prefix": "/"}, "route": route}],
            }]},
            "http_filters": [{"name": "envoy.filters.http.router", "typed_config": {"@type": ROUTER}}],
        });
        if let Some(t) = self.hcm_idle {
            hcm["stream_idle_timeout"] = t.into();
        }
        let addr = |p: u16| json!({"socket_address": {"address": "127.0.0.1", "port_value": p}});
        let mut listeners = vec![json!({
            "name": "reapi", "address": addr(self.http_port),
            "filter_chains": [{"filters": [{"name": "envoy.filters.network.http_connection_manager", "typed_config": hcm}]}],
        })];
        let mut clusters = vec![self.cluster("kbf_reapi", true, false)];
        for (name, port, idle, close) in &self.tcp {
            let mut tp = json!({"@type": TCP, "stat_prefix": name, "cluster": name});
            if let Some(i) = idle {
                tp["idle_timeout"] = (*i).into();
            }
            listeners.push(json!({
                "name": name, "address": addr(*port),
                "filter_chains": [{"filters": [{"name": "envoy.filters.network.tcp_proxy", "typed_config": tp}]}],
            }));
            clusters.push(self.cluster(name, false, *close));
        }
        json!({
            "node": {"id": "spike", "cluster": "spike"},
            "admin": {"address": addr(self.admin)},
            "static_resources": {"listeners": listeners, "clusters": clusters},
        })
    }

    pub fn write(&self, path: &Path) {
        std::fs::write(path, serde_json::to_vec_pretty(&self.json()).expect("json")).expect("write");
    }
}

/// A running Envoy; a task owns the child and records when and how it exits.
pub struct Envoy {
    pub admin: u16,
    pub log: PathBuf,
    /// (seconds since the spike started, exit status) once the process is gone.
    pub exited: Arc<Mutex<Option<(f64, String)>>>,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Flags of one Envoy process.
#[derive(Clone, Default)]
pub struct Flags {
    pub base_id: Option<u32>,
    pub epoch: u32,
    pub parent_shutdown_s: Option<u32>,
    pub drain_s: Option<u32>,
}

impl Envoy {
    /// Starts `binary` on `config`, its stderr in `log`.
    pub async fn start(binary: &Path, config: &Path, log: &Path, admin: u16, f: &Flags) -> Envoy {
        let mut cmd = Command::new(binary);
        cmd.arg("-c").arg(config).args(["--log-level", "info", "--concurrency", "2"]);
        match f.base_id {
            Some(b) => {
                cmd.args(["--base-id", &b.to_string(), "--restart-epoch", &f.epoch.to_string()]);
            }
            None => {
                cmd.arg("--disable-hot-restart");
            }
        }
        if let Some(p) = f.parent_shutdown_s {
            cmd.args(["--parent-shutdown-time-s", &p.to_string()]);
        }
        if let Some(d) = f.drain_s {
            cmd.args(["--drain-time-s", &d.to_string()]);
        }
        let out = std::fs::File::create(log).expect("log");
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(out.try_clone().expect("clone"))
            .stderr(out)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn envoy");
        let exited = Arc::new(Mutex::new(None));
        let (kill, killed) = tokio::sync::oneshot::channel::<()>();
        let slot = Arc::clone(&exited);
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s.map_or_else(|e| e.to_string(), |s| s.to_string()),
                _ = killed => { let _ = child.kill().await; "killed by the spike".to_owned() }
            };
            *slot.lock().expect("lock") = Some((crate::now(), status));
        });
        Envoy { admin, log: log.to_owned(), exited, kill: Some(kill) }
    }

    /// When and how the process exited, if it has.
    pub fn exit(&self) -> Option<(f64, String)> {
        self.exited.lock().expect("lock").clone()
    }

    /// Kills the process.
    pub fn kill(&mut self) {
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }

    /// Waits until the admin interface answers `/ready` with 200 (or `limit` passes).
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        let end = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < end {
            if let Some((200, _)) = http_get(self.admin, "/ready").await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

/// `GET path` on 127.0.0.1:`port`: (status, body), or `None` when it cannot connect.
pub async fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_owned() };
    Some((status, body))
}

/// The health flags `/clusters?format=json` shows per host of `cluster`: (port, flags).
pub async fn host_flags(admin: u16, cluster: &str) -> Vec<(u64, Vec<String>)> {
    let Some((200, body)) = http_get(admin, "/clusters?format=json").await else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for c in v["cluster_statuses"].as_array().into_iter().flatten() {
        if c["name"] != cluster {
            continue;
        }
        for h in c["host_statuses"].as_array().into_iter().flatten() {
            let port = h["address"]["socket_address"]["port_value"].as_u64().unwrap_or(0);
            let flags = h["health_status"]
                .as_object()
                .map(|m| {
                    let mut f: Vec<String> = m
                        .iter()
                        .filter(|(_, v)| v.as_bool() == Some(true) || (v.is_string() && *v != "HEALTHY"))
                        .map(|(k, v)| if v.is_boolean() { k.clone() } else { format!("{k}={v}") })
                        .collect();
                    f.sort();
                    f
                })
                .unwrap_or_default();
            out.push((port, flags));
        }
    }
    out
}

/// The raw `health_status` objects of `cluster`'s hosts, one JSON line per host.
pub async fn host_raw(admin: u16, cluster: &str) -> String {
    let Some((200, body)) = http_get(admin, "/clusters?format=json").await else {
        return "admin did not answer".to_owned();
    };
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for c in v["cluster_statuses"].as_array().into_iter().flatten() {
        if c["name"] != cluster {
            continue;
        }
        for h in c["host_statuses"].as_array().into_iter().flatten() {
            let port = &h["address"]["socket_address"]["port_value"];
            out.push(format!("{{port:{port}, health_status:{}}}", h["health_status"]));
        }
    }
    out.join(" ")
}

/// The payload of a chunked HTTP/1.1 body.
fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else { break };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

/// Writes `ports` as the endpoints of `cluster` to `path`, by rename so Envoy's file watch
/// sees one complete document.
pub fn write_eds(path: &Path, cluster: &str, ports: &[u16], version: u32) {
    let lb: Vec<Value> = ports
        .iter()
        .map(|p| json!({"endpoint": {"address": {"socket_address": {"address": "127.0.0.1", "port_value": p}}}}))
        .collect();
    let doc = json!({
        "version_info": version.to_string(),
        "resources": [{
            "@type": "type.googleapis.com/envoy.config.endpoint.v3.ClusterLoadAssignment",
            "cluster_name": cluster,
            "endpoints": [{"lb_endpoints": lb}],
        }],
    });
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&doc).expect("json")).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

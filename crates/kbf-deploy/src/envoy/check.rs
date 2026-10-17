//! `envoy check`: the properties of a rendered client front that the REAPI path
//! through it depends on, read from `manifests.json` as `kubectl apply` would apply
//! it. The Envoy config checked is the ConfigMap's copy, parsed from the List.
//!
//! [`check`] returns one [`CheckFailure`] per broken property; each variant names the
//! defect it catches, and prints as the line `FAIL <check>: <why>` ([`CheckFailure::line`]).
//!
//! The model is Envoy's route choice for the calls a Buck2 or Bazel client makes
//! ([`METHODS`]): the first route whose `path` equals the call's path or whose `prefix`
//! starts it. A route that matches on anything else (headers, a regex, a runtime
//! fraction), or whose action does more than pick a cluster and set its timeouts, is a
//! failure of its own ([`CheckFailure::RouteMatch`], [`CheckFailure::RouteAction`]):
//! the check refuses what it does not model rather than mis-model it. A field the
//! check does not read is not checked; Envoy's own `--mode validate` is what refuses
//! an unknown field.
//!
//! A value of the wrong JSON type reads as absent. A duration that is not a protobuf
//! JSON duration (`"7200s"`, `"0.5s"`, at most nine fractional digits) stops the
//! Envoy checks with [`CheckFailure::EnvoyShape`].

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use super::box_render::RETRY_ON;
pub use super::check_failure::CheckFailure;
use super::render::{CONFIG_HASH_ANNOTATION, sha256_hex};

/// Every call a Buck2 or Bazel client makes through the front. Each must reach a route
/// with no bound on a stream's whole life (route timeout 0, no `max_stream_duration`)
/// and an idle timeout over the longest silent stream ([`SILENT`]): Execute and
/// WaitExecution are those streams, and the stream idle timeout is one value for every
/// route unless a route overrides it.
pub const METHODS: [&str; 12] = [
    "build.bazel.remote.execution.v2.Execution/Execute",
    "build.bazel.remote.execution.v2.Execution/WaitExecution",
    "build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
    "build.bazel.remote.execution.v2.ActionCache/GetActionResult",
    "build.bazel.remote.execution.v2.ActionCache/UpdateActionResult",
    "google.bytestream.ByteStream/Read",
    "google.bytestream.ByteStream/Write",
    "google.bytestream.ByteStream/QueryWriteStatus",
];

/// The longest silent stream: a 1 h action plus the 300 s unservable queue wait.
pub const SILENT: Duration = Duration::from_secs(3900);
/// Envoy's stream idle timeout when the connection manager sets none.
pub const DEFAULT_STREAM_IDLE: Duration = Duration::from_secs(300);
/// Envoy's route timeout when a route sets none.
pub const DEFAULT_ROUTE_TIMEOUT: Duration = Duration::from_secs(15);

/// The names kbf-server's grpc.health.v1 service answers (`kbf_server::grpc_health::
/// SERVICES`, which a kbf-it test compares with this list); any other name is
/// NOT_FOUND, so the server is never healthy.
pub const HEALTH_SERVICES: [&str; 6] = [
    "",
    "build.bazel.remote.execution.v2.Capabilities",
    "build.bazel.remote.execution.v2.ContentAddressableStorage",
    "google.bytestream.ByteStream",
    "build.bazel.remote.execution.v2.ActionCache",
    "build.bazel.remote.execution.v2.Execution",
];

/// The objects of a render, one of each.
pub const KINDS: [&str; 5] = [
    "ConfigMap",
    "Deployment",
    "Service",
    "NetworkPolicy",
    "Ingress",
];

/// The only RouteMatch fields the route model reads.
const MATCH_KEYS: [&str; 3] = ["path", "prefix", "grpc"];
/// The only RouteAction fields the route model reads.
const ACTION_KEYS: [&str; 4] = ["cluster", "timeout", "idle_timeout", "max_stream_duration"];
/// The listener must take the Service's traffic on the pod's address.
const ANY_ADDRESS: [&str; 2] = ["0.0.0.0", "::"];
const HCM: &str = "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager";
const UPSTREAM_TLS: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext";
const ROUTER: &str = "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router";
const HTTP_OPTIONS: &str = "envoy.extensions.upstreams.http.v3.HttpProtocolOptions";

/// A duration the Envoy checks cannot read; it becomes [`CheckFailure::EnvoyShape`].
#[derive(Debug)]
pub(super) struct Malformed(pub(super) String);

/// The failures found so far.
#[derive(Default)]
pub(super) struct Checker {
    pub(super) failures: Vec<CheckFailure>,
}

impl Checker {
    /// Record `failure()` unless `ok`; return `ok`.
    pub(super) fn require(&mut self, ok: bool, failure: impl FnOnce() -> CheckFailure) -> bool {
        if !ok {
            self.failures.push(failure());
        }
        ok
    }
}

static NULL: Value = Value::Null;

/// `v` as a list; anything else is an empty one.
pub(super) fn list(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// `v` for a message: a string as itself, anything else as JSON.
pub(super) fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// Python's truth: absent, null, false, 0, `""`, `[]` and `{}` are false.
pub(super) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The keys of the object `v` outside `known`, sorted.
fn extra_keys(v: &Value, known: &[&str]) -> Vec<String> {
    let keys = v.as_object().into_iter().flat_map(|o| o.keys());
    keys.filter(|k| !known.contains(&k.as_str()))
        .cloned()
        .collect()
}

/// `selector` is a non-empty set of labels and `labels` carries each of them.
fn selects(selector: &Value, labels: &Value) -> bool {
    let Some(sel) = selector.as_object().filter(|s| !s.is_empty()) else {
        return false;
    };
    sel.iter().all(|(k, v)| labels[k.as_str()] == *v)
}

/// A protobuf JSON duration (`"7200s"`, `"0.5s"`).
pub(super) fn duration(v: &Value) -> Result<Duration, Malformed> {
    let bad = || Malformed(format!("not a duration: {v}"));
    let num = v
        .as_str()
        .and_then(|s| s.strip_suffix('s'))
        .ok_or_else(bad)?;
    let (whole, frac) = num.split_once('.').unwrap_or((num, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(frac) || frac.len() > 9 {
        return Err(bad());
    }
    let secs: u64 = whole.parse().map_err(|_| bad())?;
    let nanos = format!("{frac:0<9}")
        .bytes()
        .fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
    Ok(Duration::new(secs, nanos))
}

/// A duration in seconds for a message: `15`, `0.5`.
pub(super) fn show(d: Duration) -> String {
    let frac = format!("{:09}", d.subsec_nanos());
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        d.as_secs().to_string()
    } else {
        format!("{}.{frac}", d.as_secs())
    }
}

/// A name-keyed view of a list, as a Python dict built from it: the first position of
/// each name, holding the last item with that name.
pub(super) fn by_name<'a>(
    items: impl Iterator<Item = (&'a Value, &'a Value)>,
) -> Vec<(&'a Value, &'a Value)> {
    let mut out: Vec<(&Value, &Value)> = Vec::new();
    for (k, v) in items {
        match out.iter_mut().find(|(seen, _)| *seen == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out
}

pub(super) fn get<'a>(dict: &[(&Value, &'a Value)], key: &Value) -> Option<&'a Value> {
    dict.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Every failure of the rendered List `doc` (manifests.json). `operator_namespace` is
/// the Tailscale operator's namespace, where its ingress proxies run.
pub fn check(doc: &Value, operator_namespace: &str) -> Vec<CheckFailure> {
    let mut c = Checker::default();
    let items = if doc["kind"] == "List" {
        list(&doc["items"])
    } else {
        &[]
    };
    let mut kinds: Vec<String> = items.iter().map(|i| text(&i["kind"])).collect();
    let got = json!(kinds).to_string();
    kinds.sort();
    let mut want = KINDS.map(str::to_owned).to_vec();
    want.sort();
    c.require(kinds == want, || CheckFailure::Kinds { got });
    let by = by_name(items.iter().map(|i| (&i["kind"], i)));
    let of = |kind: &str| get(&by, &json!(kind)).unwrap_or(&NULL);
    let namespaces: BTreeSet<String> = items
        .iter()
        .map(|i| i["metadata"]["namespace"].to_string())
        .collect();
    let one_namespace = items.iter().all(|i| i["metadata"]["namespace"].is_string());
    c.require(namespaces.len() == 1 && one_namespace, || {
        CheckFailure::Namespace {
            got: json!(namespaces).to_string(),
        }
    });
    let (cm, dep, svc, np, ing) = (
        of("ConfigMap"),
        of("Deployment"),
        of("Service"),
        of("NetworkPolicy"),
        of("Ingress"),
    );
    let Some(envoy_text) = cm["data"]["envoy.json"].as_str() else {
        c.failures.push(CheckFailure::EnvoyConfig {
            why: "no envoy.json string in data".to_owned(),
        });
        return c.failures;
    };
    let envoy: Value = match serde_json::from_str(envoy_text) {
        Ok(v) => v,
        Err(e) => {
            c.failures
                .push(CheckFailure::EnvoyConfig { why: e.to_string() });
            return c.failures;
        }
    };

    // The Ingress: Tailscale's class, a MagicDNS host, and a cleartext backend.
    let spec = &ing["spec"];
    c.require(spec["ingressClassName"] == "tailscale", || {
        CheckFailure::IngressClass {
            got: text(&spec["ingressClassName"]),
        }
    });
    let hosts: Vec<&Value> = list(&spec["tls"])
        .iter()
        .flat_map(|t| list(&t["hosts"]))
        .collect();
    let label = hosts
        .first()
        .and_then(|h| h.as_str())
        .is_some_and(|h| !h.contains('.'));
    c.require(hosts.len() == 1 && label, || CheckFailure::IngressHost {
        hosts: json!(hosts).to_string(),
    });
    let backend = &spec["defaultBackend"]["service"];
    c.require(backend["name"] == svc["metadata"]["name"], || {
        CheckFailure::IngressBackend {
            got: text(&backend["name"]),
        }
    });
    let bport = &backend["port"]["number"];
    let sport = list(&svc["spec"]["ports"])
        .iter()
        .find(|p| p["port"] == *bport);
    if c.require(sport.is_some(), || CheckFailure::IngressPort {
        port: text(bport),
    }) {
        let https = sport.is_some_and(|p| p["name"] == "https");
        c.require(*bport != 443 && !https, || CheckFailure::IngressCleartext);
    }

    // The Deployment runs this config, pinned, with the CA mounted.
    let pod = &dep["spec"]["template"];
    let containers = list(&pod["spec"]["containers"]);
    c.require(containers.len() == 1, || CheckFailure::Container {
        count: containers.len(),
    });
    let ctr = containers.first().unwrap_or(&NULL);
    let image = ctr["image"].as_str().unwrap_or("");
    c.require(pinned(image), || CheckFailure::ImagePinned {
        image: text(&ctr["image"]),
    });
    c.require(
        pod["metadata"]["annotations"][CONFIG_HASH_ANNOTATION] == *sha256_hex(envoy_text),
        || CheckFailure::ConfigHash,
    );
    let selector = &svc["spec"]["selector"];
    let labels = &pod["metadata"]["labels"];
    c.require(selects(selector, labels), || {
        CheckFailure::ServiceSelector {
            selector: selector.to_string(),
            labels: labels.to_string(),
        }
    });
    let vols = by_name(
        list(&pod["spec"]["volumes"])
            .iter()
            .map(|v| (&v["name"], v)),
    );
    let mounts = by_name(
        list(&ctr["volumeMounts"])
            .iter()
            .map(|m| (&m["name"], &m["mountPath"])),
    );
    let args = list(&ctr["args"]);
    let config_path = args
        .iter()
        .position(|a| *a == "--config-path")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&NULL);
    let cm_mount = vols
        .iter()
        .find(|(_, v)| v["configMap"]["name"] == cm["metadata"]["name"])
        .and_then(|(n, _)| get(&mounts, n))
        .unwrap_or(&NULL);
    let want_path = cm_mount
        .as_str()
        .map(|m| format!("{}/envoy.json", m.trim_end_matches('/')));
    c.require(
        want_path.is_some() && config_path.as_str() == want_path.as_deref(),
        || CheckFailure::ConfigMount {
            config_path: text(config_path),
            mount: text(cm_mount),
        },
    );
    let ca_vol = vols.iter().find(|(_, v)| v.get("secret").is_some());
    let ca = ca_vol.and_then(|(n, v)| Some((get(&mounts, n)?.as_str()?, *v)));
    let Some((ca_mount, ca_vol)) = ca else {
        c.failures.push(CheckFailure::CaMount);
        return c.failures;
    };
    // The file the Secret's key ca.pem becomes: its item's path, or the key name
    // itself when the volume projects every key.
    let ca_items = &ca_vol["secret"]["items"];
    let ca_path = if truthy(ca_items) {
        list(ca_items)
            .iter()
            .find(|i| i["key"] == "ca.pem")
            .and_then(|i| i["path"].as_str())
    } else {
        Some("ca.pem")
    };
    let ca_file = ca_path.map(|p| format!("{}/{p}", ca_mount.trim_end_matches('/')));
    // With no Service port the Ingress sends to (ingress-port failed), the port Envoy
    // and the NetworkPolicy must use is unknown, so those two checks are skipped.
    let target_port = sport.map(|p| &p["targetPort"]);
    check_netpol(&mut c, np, labels, target_port, operator_namespace);
    if let Err(Malformed(why)) = check_envoy(&mut c, &envoy, ca_file.as_deref(), target_port) {
        c.failures.push(CheckFailure::EnvoyShape { why });
    }
    c.failures
}

/// `image` ends in `@sha256:` and 64 lower-case hex digits.
fn pinned(image: &str) -> bool {
    image.rsplit_once("@sha256:").is_some_and(|(_, d)| {
        d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The NetworkPolicy must select the Envoy pods and admit the operator's namespace
/// (where its proxies run) on Envoy's port: anything else either blocks the proxy or
/// admits a namespace it should not.
fn check_netpol(
    c: &mut Checker,
    np: &Value,
    labels: &Value,
    port: Option<&Value>,
    operator_ns: &str,
) {
    let spec = &np["spec"];
    let selector = &spec["podSelector"]["matchLabels"];
    let ingress = list(&spec["policyTypes"]).iter().any(|t| *t == "Ingress");
    c.require(selects(selector, labels) && ingress, || {
        CheckFailure::NetpolPods {
            selector: selector.to_string(),
            labels: labels.to_string(),
        }
    });
    let rules = list(&spec["ingress"]);
    let [rule] = rules else {
        c.failures
            .push(CheckFailure::NetpolRule { count: rules.len() });
        return;
    };
    let ports = &rule["ports"];
    if let Some(port) = port {
        c.require(*ports == json!([{"protocol": "TCP", "port": port}]), || {
            CheckFailure::NetpolPort {
                ports: ports.to_string(),
                port: text(port),
            }
        });
    }
    let want = json!([{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": operator_ns}}}]);
    c.require(rule["from"] == want, || CheckFailure::NetpolFrom {
        from: rule["from"].to_string(),
        namespace: operator_ns.to_owned(),
    });
}

/// A stream's whole life must not be bounded: `max_stream_duration` absent or 0 in
/// `opts` (an HttpProtocolOptions or a RouteAction's MaxStreamDuration).
pub(super) fn unbounded(c: &mut Checker, opts: &Value, at: &str) -> Result<(), Malformed> {
    let d = &opts["max_stream_duration"];
    let ok = d.is_null() || duration(d)?.is_zero();
    c.require(ok, || CheckFailure::MaxStreamDuration {
        at: at.to_owned(),
        duration: text(d),
    });
    Ok(())
}

/// Envoy's route choice for a gRPC call on `path`: the first route whose match takes
/// it. Exact for a match on `path` or `prefix` only; the route-match check refuses any
/// other key.
pub(super) fn first_route<'a>(
    routes: &'a [Value],
    path: &str,
) -> Result<Option<&'a Value>, Malformed> {
    for r in routes {
        let m = &r["match"];
        if m["path"] == path {
            return Ok(Some(r));
        }
        match &m["prefix"] {
            Value::Null => {}
            Value::String(p) if path.starts_with(p.as_str()) => return Ok(Some(r)),
            Value::String(_) => {}
            other => return Err(Malformed(format!("a route prefix {other}"))),
        }
    }
    Ok(None)
}

/// The listener, its connection manager and routes, and each cluster a route uses.
fn check_envoy(
    c: &mut Checker,
    envoy: &Value,
    ca_file: Option<&str>,
    listen_port: Option<&Value>,
) -> Result<(), Malformed> {
    let resources = &envoy["static_resources"];
    let clusters = by_name(
        list(&resources["clusters"])
            .iter()
            .map(|cl| (&cl["name"], cl)),
    );
    let listeners = list(&resources["listeners"]);
    c.require(listeners.len() == 1, || CheckFailure::Listener {
        count: listeners.len(),
    });
    let Some(lst) = listeners.first() else {
        return Ok(());
    };
    let address = &lst["address"]["socket_address"];
    if let Some(listen_port) = listen_port {
        c.require(address["port_value"] == *listen_port, || {
            CheckFailure::ListenerPort {
                port: text(&address["port_value"]),
                service_port: text(listen_port),
            }
        });
    }
    let any = address["address"]
        .as_str()
        .is_some_and(|a| ANY_ADDRESS.contains(&a));
    c.require(any, || CheckFailure::ListenerAddress {
        address: text(&address["address"]),
    });
    let chains = list(&lst["filter_chains"]);
    let hcms: Vec<&Value> = chains
        .iter()
        .flat_map(|ch| list(&ch["filters"]))
        .map(|f| &f["typed_config"])
        .filter(|t| t["@type"] == HCM)
        .collect();
    let ([chain], [hcm]) = (chains, hcms.as_slice()) else {
        c.failures.push(CheckFailure::Hcm);
        return Ok(());
    };
    c.require(!truthy(&chain["transport_socket"]), || {
        CheckFailure::ListenerPlaintext
    });
    let used = check_hcm(c, hcm, &clusters, Kit::Front)?;
    for (name, cl) in &clusters {
        if used.contains(&name.to_string()) {
            check_cluster(c, cl, &text(name), ca_file, Kit::Front)?;
        }
    }
    Ok(())
}

/// Which topology's rules the connection manager is held to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kit {
    /// The Kubernetes front.
    Front,
    /// One host's Envoy in front of two instances ([`super::box_check`]).
    Box,
}

/// The connection manager `hcm`, its routes and the stream bounds every method of
/// [`METHODS`] meets; the names of the clusters the routes use.
pub(super) fn check_hcm(
    c: &mut Checker,
    hcm: &Value,
    clusters: &[(&Value, &Value)],
    kit: Kit,
) -> Result<BTreeSet<String>, Malformed> {
    let codec = hcm.get("codec_type");
    let h2 = codec.is_none_or(|v| *v == "AUTO" || *v == "HTTP2");
    c.require(h2, || CheckFailure::Codec {
        codec: codec.map_or_else(String::new, text),
    });
    let idle = match hcm.get("stream_idle_timeout") {
        Some(d) => duration(d)?,
        None => DEFAULT_STREAM_IDLE,
    };
    unbounded(
        c,
        &hcm["common_http_protocol_options"],
        "the connection manager",
    )?;
    let rt = &hcm["request_timeout"];
    let ok = rt.is_null() || duration(rt)?.is_zero();
    c.require(ok, || CheckFailure::RequestTimeout { timeout: text(rt) });
    let filters: Vec<&Value> = list(&hcm["http_filters"])
        .iter()
        .map(|f| &f["typed_config"]["@type"])
        .collect();
    c.require(matches!(filters.as_slice(), [f] if **f == ROUTER), || {
        CheckFailure::Filters {
            filters: json!(filters).to_string(),
        }
    });
    let vhosts = list(&hcm["route_config"]["virtual_hosts"]);
    let every_authority = vhosts.first().is_some_and(|v| v["domains"] == json!(["*"]));
    c.require(vhosts.len() == 1 && every_authority, || CheckFailure::Vhost);
    let routes = vhosts.first().map_or(&[][..], |v| list(&v["routes"]));
    for r in routes {
        check_route_shape(c, r, kit);
    }
    let mut used = BTreeSet::new();
    for method in METHODS {
        let path = format!("/{method}");
        let Some(r) = first_route(routes, &path)? else {
            c.failures.push(CheckFailure::Route { path });
            continue;
        };
        let action = &r["route"];
        let cluster = &action["cluster"];
        let defined = get(clusters, cluster).is_some();
        if defined {
            used.insert(cluster.to_string());
        }
        c.require(defined, || CheckFailure::RouteCluster {
            path: path.clone(),
            cluster: text(cluster),
        });
        // The route timeout bounds the whole response (from the end of the request to
        // the end of the response stream), not silence: any value cuts an Execute or
        // a ByteStream Read that lives longer, however busy.
        let timeout = match action.get("timeout") {
            Some(d) => duration(d)?,
            None => DEFAULT_ROUTE_TIMEOUT,
        };
        c.require(timeout.is_zero(), || CheckFailure::RouteTimeout {
            path: path.clone(),
            secs: show(timeout),
        });
        unbounded(c, &action["max_stream_duration"], &path)?;
        let route_idle = action.get("idle_timeout").map(duration).transpose()?;
        let idle = route_idle.unwrap_or(idle);
        c.require(idle.is_zero() || idle > SILENT, || {
            CheckFailure::IdleTimeout {
                path: path.clone(),
                secs: show(idle),
            }
        });
        if kit == Kit::Box {
            // An action's own timeout is uncapped, so a bound on silence that the
            // connection manager's value gives these two can cut a long one.
            let long = method.ends_with("/Execute") || method.ends_with("/WaitExecution");
            c.require(!long || route_idle.is_some_and(|d| d.is_zero()), || {
                CheckFailure::ExecuteIdle {
                    path: path.clone(),
                    got: route_idle.map_or_else(|| "absent".to_owned(), show),
                }
            });
            check_retry(c, r, &path);
        }
    }
    Ok(used)
}

/// A route's `retry_policy` (box only): only conditions that repeat no work
/// ([`RETRY_ON`]), and none on a ByteStream call, whose body streams.
fn check_retry(c: &mut Checker, r: &Value, path: &str) {
    let policy = &r["route"]["retry_policy"];
    if policy.is_null() {
        return;
    }
    let bytestream = path.starts_with("/google.bytestream.");
    let on: Vec<&str> = policy["retry_on"]
        .as_str()
        .map_or_else(Vec::new, |s| s.split(',').collect());
    let allowed = RETRY_ON.split(',').collect::<Vec<_>>();
    let extra = extra_keys(policy, &["retry_on", "num_retries"]);
    let why = if bytestream {
        Some("a ByteStream body streams and cannot be replayed".to_owned())
    } else if on.is_empty() || !on.iter().all(|c| allowed.contains(c)) {
        Some(format!(
            "retry_on {} is not within {RETRY_ON}",
            text(&policy["retry_on"])
        ))
    } else if !extra.is_empty() {
        Some(format!("unmodelled keys {}", json!(extra)))
    } else {
        None
    };
    if let Some(why) = why {
        c.failures.push(CheckFailure::RetryRoutes {
            path: path.to_owned(),
            why,
        });
    }
}

/// A route matches on `path` or `prefix` alone, is not a catch-all, and its action
/// only picks a cluster and sets timeouts.
fn check_route_shape(c: &mut Checker, r: &Value, kit: Kit) {
    let m = &r["match"];
    let prefix = m["prefix"].as_str();
    c.require(prefix != Some("") && prefix != Some("/"), || {
        CheckFailure::NoCatchAll
    });
    let extra = extra_keys(m, &MATCH_KEYS);
    let one_kind = usize::from(m.get("path").is_some()) + usize::from(m.get("prefix").is_some());
    let grpc = m.get("grpc").is_none_or(|g| *g == json!({}));
    c.require(extra.is_empty() && one_kind == 1 && grpc, || {
        CheckFailure::RouteMatch {
            matched: m.to_string(),
            extra: json!(extra).to_string(),
        }
    });
    let mut known = ACTION_KEYS.to_vec();
    if kit == Kit::Box {
        known.push("retry_policy");
    }
    let extra = extra_keys(&r["route"], &known);
    c.require(extra.is_empty(), || CheckFailure::RouteAction {
        matched: m.to_string(),
        extra: json!(extra).to_string(),
    });
}

/// A cluster a route uses: health-checked by grpc.health.v1 on a name kbf-server
/// answers, never sending to an unhealthy server, HTTP/2 over TLS to the mounted CA,
/// each server's name checked.
pub(super) fn check_cluster(
    c: &mut Checker,
    cl: &Value,
    name: &str,
    ca_file: Option<&str>,
    kit: Kit,
) -> Result<(), Malformed> {
    let cluster = || name.to_owned();
    let hcs = list(&cl["health_checks"]);
    c.require(!hcs.is_empty(), || CheckFailure::HealthCheck {
        cluster: cluster(),
    });
    for hc in hcs {
        c.require(hc.get("grpc_health_check").is_some(), || {
            CheckFailure::HealthCheckGrpc { cluster: cluster() }
        });
        c.require(hc.get("transport_socket_match_criteria").is_none(), || {
            CheckFailure::HealthCheckTls { cluster: cluster() }
        });
        let service = hc["grpc_health_check"].get("service_name");
        let known =
            service.is_none_or(|s| s.as_str().is_some_and(|s| HEALTH_SERVICES.contains(&s)));
        c.require(known, || CheckFailure::HealthCheckService {
            cluster: cluster(),
            service: service.map_or_else(String::new, |s| s.to_string()),
        });
    }
    let panic = &cl["common_lb_config"]["healthy_panic_threshold"]["value"];
    c.require(panic.as_f64() == Some(0.0), || {
        CheckFailure::PanicThreshold {
            cluster: cluster(),
            value: if panic.is_null() {
                "absent (Envoy's default 50)".to_owned()
            } else {
                panic.to_string()
            },
        }
    });
    let opts = &cl["typed_extension_protocol_options"][HTTP_OPTIONS];
    c.require(
        opts["explicit_http_config"]
            .get("http2_protocol_options")
            .is_some(),
        || CheckFailure::UpstreamH2 { cluster: cluster() },
    );
    unbounded(
        c,
        &opts["common_http_protocol_options"],
        &format!("cluster {name}"),
    )?;
    let matches = list(&cl["transport_socket_matches"]);
    let default = format!("cluster {name} default");
    tls(c, &cl["transport_socket"], &default, ca_file, None);
    let eps: Vec<&Value> = list(&cl["load_assignment"]["endpoints"])
        .iter()
        .flat_map(|g| list(&g["lb_endpoints"]))
        .collect();
    c.require(!eps.is_empty(), || CheckFailure::Endpoints {
        cluster: cluster(),
    });
    for e in eps {
        // The front dials each server by the name it checks; a box dials loopback ports
        // and checks the one name it was given.
        let host = match kit {
            Kit::Front => &e["endpoint"]["address"]["socket_address"]["address"],
            Kit::Box => &e["endpoint"]["hostname"],
        };
        let criteria = &e["metadata"]["filter_metadata"]["envoy.transport_socket_match"];
        let m = matches
            .iter()
            .find(|m| truthy(criteria) && m["match"] == *criteria);
        let at = format!("server {}", text(host));
        match m {
            Some(m) => tls(c, &m["transport_socket"], &at, ca_file, Some(host)),
            None => c
                .failures
                .push(CheckFailure::EndpointTlsMatch { host: text(host) }),
        }
    }
    Ok(())
}

/// A transport socket of the hop to the servers: TLS that trusts the mounted CA and
/// offers h2; for one server (`host`), its SNI and the certificate's name too.
pub(super) fn tls(
    c: &mut Checker,
    ts: &Value,
    at: &str,
    ca_file: Option<&str>,
    host: Option<&Value>,
) {
    let tc = &ts["typed_config"];
    if !c.require(tc["@type"] == UPSTREAM_TLS, || CheckFailure::UpstreamTls {
        at: at.to_owned(),
    }) {
        return;
    }
    let ctx = &tc["common_tls_context"];
    let vc = &ctx["validation_context"];
    let ca = &vc["trusted_ca"]["filename"];
    c.require(ca_file.is_some() && ca.as_str() == ca_file, || {
        CheckFailure::UpstreamCa {
            at: at.to_owned(),
            got: text(ca),
            want: ca_file.unwrap_or("(no ca.pem item)").to_owned(),
        }
    });
    let h2 = list(&ctx["alpn_protocols"]).iter().any(|p| *p == "h2");
    c.require(h2, || CheckFailure::UpstreamAlpn { at: at.to_owned() });
    let Some(host) = host else {
        return;
    };
    c.require(tc["sni"] == *host, || CheckFailure::UpstreamSni {
        at: at.to_owned(),
        sni: text(&tc["sni"]),
    });
    let want = json!([{"san_type": "DNS", "matcher": {"exact": host}}]);
    c.require(vc["match_typed_subject_alt_names"] == want, || {
        CheckFailure::UpstreamSan {
            at: at.to_owned(),
            host: text(host),
        }
    });
}

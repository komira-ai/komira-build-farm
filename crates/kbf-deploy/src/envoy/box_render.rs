//! `envoy render --topology box`: the Envoy of one host in front of two kbf instances.
//!
//! One static `envoy.json` lists both instances for ever (a kbf update changes no Envoy
//! file; routing changes by health alone), and `envoy.args` carries the command-line
//! settings of Envoy's hot restart, which are not part of the bootstrap. The settings
//! that keep a long silent `Execute` alive, and why each is not Envoy's default, are
//! the comments below and the rules of [`super::box_check`].

use super::box_config::{BoxConfig, Instance};
use super::json::Json;
use super::render::{
    HCM, HTTP_OPTIONS, LONG_STREAM, ROUTE_TIMEOUT, ROUTER, ROUTES, TLS_SOCKET,
    default_upstream_tls, socket_address, upstream_tls,
};

/// The Envoy cluster of the instances' REAPI listeners (L7).
pub const REAPI_CLUSTER: &str = "kbf_reapi";
/// The Envoy cluster of the instances' worker listeners (L4).
pub const WORKER_CLUSTER: &str = "kbf_workers";
/// The loopback address the instances listen on.
pub const LOOPBACK: &str = "127.0.0.1";
/// The retry conditions a route may carry: a connection that failed before a stream
/// started, or a stream the upstream refused before processing it. Neither repeats work.
pub const RETRY_ON: &str = "connect-failure,refused-stream";
/// How many times such a call is retried.
const NUM_RETRIES: i64 = 2;
/// The HTTP/2 PING Envoy sends an upstream connection, as kbf's own listeners do: `(interval,
/// timeout)`.
pub const UPSTREAM_KEEPALIVE: (&str, &str) = ("30s", "20s");
/// The health check on every instance: kbf's readiness, once a second.
const CHECK_SECS: &str = "1s";
/// Envoy's `--drain-time-s`: only schedules when open connections are told to go.
pub const DRAIN_SECS: u64 = 60;
const DOWNSTREAM_TLS: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext";
const TCP_PROXY: &str =
    "type.googleapis.com/envoy.extensions.filters.network.tcp_proxy.v3.TcpProxy";

/// The two files `envoy render --topology box` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxRendered {
    /// `envoy.json`: the Envoy bootstrap.
    pub envoy: String,
    /// `envoy.args`: Envoy's hot-restart flags, one per line, `FLAG VALUE` or `FLAG`.
    pub args: String,
}

/// Render both files.
pub fn render_box(c: &BoxConfig) -> BoxRendered {
    BoxRendered {
        envoy: envoy_config(c).to_file_text(),
        args: args(c),
    }
}

/// Envoy's hot-restart flags. `--parent-shutdown-time-s` is the longest action plus a
/// quarter hour: at expiry the old epoch is closed with every stream still open (its
/// default, 900 s, would fail every action over 15 minutes). `--drain-time-s` stays
/// below it, as Envoy requires. `gradual` is Envoy's default, written down.
fn args(c: &BoxConfig) -> String {
    format!(
        "--parent-shutdown-time-s {}\n--drain-time-s {DRAIN_SECS}\n--drain-strategy gradual\n--skip-hot-restart-on-no-parent\n",
        c.parent_shutdown_secs()
    )
}

fn secs(n: u64) -> Json {
    Json::Str(format!("{n}s"))
}

fn route_action(has_retry: bool, idle_off: bool) -> Json {
    let mut action = vec![
        ("cluster", REAPI_CLUSTER.into()),
        // The route timeout bounds a whole response, busy or silent (Envoy's default is
        // 15 s): an Execute runs for as long as its action.
        ("timeout", ROUTE_TIMEOUT.into()),
    ];
    // The HCM's stream idle timeout is a bound on silence that an action's own timeout
    // can exceed; Execute and WaitExecution carry none.
    if idle_off {
        action.push(("idle_timeout", secs(0)));
    }
    if has_retry {
        action.push((
            "retry_policy",
            Json::object([
                ("retry_on", RETRY_ON.into()),
                ("num_retries", NUM_RETRIES.into()),
            ]),
        ));
    }
    object(action)
}

fn routes() -> Json {
    Json::List(
        ROUTES
            .iter()
            .map(|&(kind, value)| {
                let call = value.rsplit('/').next().unwrap_or("");
                let long = kind == "path" && (call == "Execute" || call == "WaitExecution");
                // A ByteStream body streams (a 64 MiB write is not buffered for a retry).
                let retry = !value.starts_with("/google.bytestream.");
                Json::object([
                    (
                        "match",
                        Json::object([(kind, value.into()), ("grpc", Json::object([]))]),
                    ),
                    ("route", route_action(retry, long)),
                ])
            })
            .collect(),
    )
}

fn connection_manager() -> Json {
    let router = Json::object([
        ("name", "envoy.filters.http.router".into()),
        ("typed_config", Json::object([("@type", ROUTER.into())])),
    ]);
    Json::object([
        ("@type", HCM.into()),
        ("stat_prefix", "reapi".into()),
        ("codec_type", "AUTO".into()),
        ("stream_idle_timeout", LONG_STREAM.into()),
        (
            "route_config",
            Json::object([
                ("name", "reapi".into()),
                (
                    "virtual_hosts",
                    Json::List(vec![Json::object([
                        ("name", "kbf".into()),
                        ("domains", vec!["*"].into()),
                        ("routes", routes()),
                    ])]),
                ),
            ]),
        ),
        // Only the router: no filter that buffers a request.
        ("http_filters", Json::List(vec![router])),
    ])
}

fn reapi_listener(c: &BoxConfig) -> Json {
    let file = |f: &str| Json::object([("filename", f.into())]);
    let tls = Json::object([
        ("name", TLS_SOCKET.into()),
        (
            "typed_config",
            Json::object([
                ("@type", DOWNSTREAM_TLS.into()),
                (
                    "common_tls_context",
                    Json::object([
                        ("alpn_protocols", vec!["h2"].into()),
                        (
                            "tls_certificates",
                            Json::List(vec![Json::object([
                                ("certificate_chain", file(&c.cert_file)),
                                ("private_key", file(&c.key_file)),
                            ])]),
                        ),
                    ]),
                ),
            ]),
        ),
    ]);
    Json::object([
        ("name", "reapi".into()),
        (
            "address",
            socket_address("0.0.0.0", i64::from(c.reapi_port)),
        ),
        (
            "filter_chains",
            Json::List(vec![Json::object([
                (
                    "filters",
                    Json::List(vec![Json::object([
                        (
                            "name",
                            "envoy.filters.network.http_connection_manager".into(),
                        ),
                        ("typed_config", connection_manager()),
                    ])]),
                ),
                ("transport_socket", tls),
            ])]),
        ),
    ])
}

/// The daemons' mutual TLS passes through untouched: Envoy terminating it would lose the
/// certificate identity the server matches against the daemon's `Hello`. A daemon's
/// session stream is quiet between heartbeats, so the proxy's idle timeout (1 h) is off.
fn worker_listener(c: &BoxConfig) -> Json {
    let proxy = Json::object([
        ("@type", TCP_PROXY.into()),
        ("stat_prefix", "worker".into()),
        ("cluster", WORKER_CLUSTER.into()),
        ("idle_timeout", secs(0)),
    ]);
    Json::object([
        ("name", "worker".into()),
        (
            "address",
            socket_address("0.0.0.0", i64::from(c.worker_port)),
        ),
        (
            "filter_chains",
            Json::List(vec![Json::object([(
                "filters",
                Json::List(vec![Json::object([
                    ("name", "envoy.filters.network.tcp_proxy".into()),
                    ("typed_config", proxy),
                ])]),
            )])]),
        ),
    ])
}

/// One open `Execute` is one request, so Envoy's default limits (about a thousand)
/// would refuse the thousand-and-first action. For the REAPI cluster the request,
/// pending and retry limits are the ceiling; the connections needed for it are what
/// `max_streams` streams per connection allow, twice over for connections still
/// draining. The worker cluster is TCP: only its connection limit applies, and the
/// ceiling is far above the daemons and their transfers.
fn breakers(c: &BoxConfig, reapi: bool) -> Json {
    let ceiling = i64::from(c.stream_ceiling);
    let threshold = if reapi {
        let connections = i64::from(c.stream_ceiling.div_ceil(c.reapi_max_streams)) * 2;
        Json::object([
            ("priority", "DEFAULT".into()),
            ("max_connections", connections.into()),
            ("max_pending_requests", ceiling.into()),
            ("max_requests", ceiling.into()),
            ("max_retries", ceiling.into()),
        ])
    } else {
        Json::object([
            ("priority", "DEFAULT".into()),
            ("max_connections", ceiling.into()),
        ])
    };
    Json::object([("thresholds", Json::List(vec![threshold]))])
}

fn health_check(kind: (&str, Json)) -> Json {
    Json::List(vec![Json::object([
        ("timeout", CHECK_SECS.into()),
        ("interval", CHECK_SECS.into()),
        ("no_traffic_interval", CHECK_SECS.into()),
        ("unhealthy_threshold", 2i64.into()),
        ("healthy_threshold", 1i64.into()),
        kind,
    ])])
}

/// The settings both clusters share: loopback, both instances for ever, never a send to
/// an unready one, and a failed health check that leaves open streams alone.
fn cluster(
    c: &BoxConfig,
    name: &str,
    endpoint: impl Fn(&Instance) -> Json,
) -> Vec<(&'static str, Json)> {
    vec![
        ("name", name.into()),
        ("type", "STATIC".into()),
        ("connect_timeout", "1s".into()),
        ("lb_policy", "ROUND_ROBIN".into()),
        (
            "common_lb_config",
            Json::object([(
                "healthy_panic_threshold",
                Json::object([("value", 0i64.into())]),
            )]),
        ),
        // Envoy's default is false; written down because a true value cuts the open
        // streams of an instance that is only draining.
        ("close_connections_on_host_health_failure", false.into()),
        ("circuit_breakers", breakers(c, name == REAPI_CLUSTER)),
        (
            "load_assignment",
            Json::object([
                ("cluster_name", name.into()),
                (
                    "endpoints",
                    Json::List(vec![Json::object([(
                        "lb_endpoints",
                        Json::List(c.instances().iter().map(endpoint).collect()),
                    )])]),
                ),
            ]),
        ),
    ]
}

fn object(pairs: Vec<(&'static str, Json)>) -> Json {
    Json::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn reapi_cluster(c: &BoxConfig) -> Json {
    let host = c.host.as_str();
    let endpoint = |i: &Instance| {
        Json::object([
            (
                "endpoint",
                Json::object([
                    ("address", socket_address(LOOPBACK, i64::from(i.reapi))),
                    ("hostname", host.into()),
                ]),
            ),
            (
                "metadata",
                Json::object([(
                    "filter_metadata",
                    Json::object([(
                        "envoy.transport_socket_match",
                        Json::object([("server", host.into())]),
                    )]),
                )]),
            ),
        ])
    };
    let mut fields = cluster(c, REAPI_CLUSTER, endpoint);
    let (interval, timeout) = UPSTREAM_KEEPALIVE;
    fields.extend([
        (
            "typed_extension_protocol_options",
            Json::object([(
                HTTP_OPTIONS,
                Json::object([
                    (
                        "@type",
                        format!("type.googleapis.com/{HTTP_OPTIONS}")
                            .as_str()
                            .into(),
                    ),
                    (
                        "explicit_http_config",
                        Json::object([(
                            "http2_protocol_options",
                            Json::object([
                                (
                                    "connection_keepalive",
                                    Json::object([
                                        ("interval", interval.into()),
                                        ("timeout", timeout.into()),
                                    ]),
                                ),
                                // A fresh upstream connection is assumed to take any number
                                // of streams until the instance's SETTINGS arrive, so a
                                // burst of silent Executes is sent down the first
                                // connection and refused past kbf's limit; this is the limit.
                                (
                                    "max_concurrent_streams",
                                    i64::from(c.reapi_max_streams).into(),
                                ),
                            ]),
                        )]),
                    ),
                ]),
            )]),
        ),
        (
            "transport_socket_matches",
            Json::List(vec![Json::object([
                ("name", host.into()),
                ("match", Json::object([("server", host.into())])),
                ("transport_socket", upstream_tls(host, &c.ca_file)),
            ])]),
        ),
        ("transport_socket", default_upstream_tls(&c.ca_file)),
        // grpc.health.v1.Health/Check on the instance's REAPI listener, empty service
        // name: kbf's readiness.
        (
            "health_checks",
            health_check((
                "grpc_health_check",
                Json::object([("service_name", "".into())]),
            )),
        ),
    ]);
    object(fields)
}

fn worker_cluster(c: &BoxConfig) -> Json {
    let endpoint = |i: &Instance| {
        Json::object([(
            "endpoint",
            Json::object([
                ("address", socket_address(LOOPBACK, i64::from(i.worker))),
                // The check goes to the instance's API listener, which answers /readyz
                // without a token; the connection is plain loopback HTTP.
                (
                    "health_check_config",
                    Json::object([("port_value", i64::from(i.api).into())]),
                ),
            ]),
        )])
    };
    let mut fields = cluster(c, WORKER_CLUSTER, endpoint);
    fields.push((
        "health_checks",
        health_check((
            "http_health_check",
            Json::object([("path", "/readyz".into())]),
        )),
    ));
    object(fields)
}

/// The Envoy bootstrap of the box.
pub fn envoy_config(c: &BoxConfig) -> Json {
    Json::object([
        (
            "admin",
            Json::object([("address", socket_address(LOOPBACK, i64::from(c.admin_port)))]),
        ),
        (
            "static_resources",
            Json::object([
                (
                    "listeners",
                    Json::List(vec![reapi_listener(c), worker_listener(c)]),
                ),
                (
                    "clusters",
                    Json::List(vec![reapi_cluster(c), worker_cluster(c)]),
                ),
            ]),
        ),
    ])
}

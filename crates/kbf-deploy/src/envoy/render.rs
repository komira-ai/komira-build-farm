//! `envoy render`: the Envoy bootstrap and the Kubernetes objects of the client front,
//! built from an [`EnvoyConfig`].
//!
//! The front is a Tailscale operator Ingress in front of one Envoy Deployment, which
//! routes REAPI calls by gRPC method to the kbf-servers over TLS that must chain to the
//! farm's CA and name each server.

use sha2::{Digest, Sha256};

use super::config::{EnvoyConfig, Server};
use super::json::Json;

/// The name of the Envoy objects (ConfigMap, Deployment, Service, NetworkPolicy).
pub const APP: &str = "kbf-envoy";
/// The name of the Ingress.
pub const INGRESS: &str = "kbf";
/// The Envoy cluster of the kbf-servers.
pub const CLUSTER: &str = "kbf_servers";
/// Envoy's REAPI listener, and the Service's port.
pub const LISTEN_PORT: i64 = 8080;
/// Envoy's admin listener, on loopback only.
pub const ADMIN_PORT: i64 = 9901;
pub const CONFIG_DIR: &str = "/etc/envoy";
pub const CONFIG_FILE: &str = "/etc/envoy/envoy.json";
pub const CA_DIR: &str = "/etc/kbf-front/ca";
pub const CA_FILE: &str = "/etc/kbf-front/ca/ca.pem";
/// The pod template annotation holding the sha256 of the Envoy config: a changed
/// config changes the template and so rolls the pods (a ConfigMap edit alone does not).
pub const CONFIG_HASH_ANNOTATION: &str = "kbf-front/envoy-config-sha256";
/// The user the envoy-distroless images run as (nonroot).
pub const ENVOY_UID: i64 = 65532;

/// The stream idle timeout, above the longest silent REAPI stream: the container
/// driver's default action timeout (1 h) plus the unservable queue wait (300 s) is
/// 3900 s. Envoy's default is 300 s.
pub const LONG_STREAM: &str = "7200s";
/// The route timeout bounds a whole response, from the end of the request to the end
/// of the response stream, busy or silent (Envoy's default is 15 s). `0s` disables it:
/// an Execute that queues and runs for hours, or a long ByteStream Read, is bounded
/// only by the idle timeout.
pub const ROUTE_TIMEOUT: &str = "0s";

/// One route per row, first match wins (Envoy's order): the match kind (`path` or
/// `prefix`) and its value. Every row goes to [`CLUSTER`].
pub const ROUTES: &[(&str, &str)] = &[
    ("path", "/build.bazel.remote.execution.v2.Execution/Execute"),
    (
        "path",
        "/build.bazel.remote.execution.v2.Execution/WaitExecution",
    ),
    ("prefix", "/build.bazel.remote.execution.v2.Capabilities/"),
    (
        "prefix",
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
    ),
    ("prefix", "/build.bazel.remote.execution.v2.ActionCache/"),
    ("prefix", "/google.bytestream.ByteStream/"),
];

pub(super) const TLS_SOCKET: &str = "envoy.transport_sockets.tls";
pub(super) const UPSTREAM_TLS: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext";
pub(super) const HCM: &str = "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager";
pub(super) const ROUTER: &str =
    "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router";
pub(super) const HTTP_OPTIONS: &str = "envoy.extensions.upstreams.http.v3.HttpProtocolOptions";

/// The two files `envoy render` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// `manifests.json`: a v1 List of the ConfigMap, Deployment, Service,
    /// NetworkPolicy and Ingress.
    pub manifests: String,
    /// `envoy.json`: the Envoy bootstrap, the same bytes the ConfigMap carries.
    pub envoy: String,
}

/// Render both files.
pub fn render(c: &EnvoyConfig) -> Rendered {
    let envoy = envoy_config(&c.servers).to_file_text();
    let manifests = manifests(c, &envoy).to_file_text();
    Rendered { manifests, envoy }
}

/// The lower-case hex sha256 of `text`'s bytes.
pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(super) fn ca_validation(ca_file: &str) -> (&'static str, Json) {
    ("trusted_ca", Json::object([("filename", ca_file.into())]))
}

/// TLS to one kbf-server: its certificate must chain to the CA and name `host` (the
/// name Envoy dials); h2 is what gRPC needs.
pub(super) fn upstream_tls(host: &str, ca_file: &str) -> Json {
    let san = Json::object([
        ("san_type", "DNS".into()),
        ("matcher", Json::object([("exact", host.into())])),
    ]);
    Json::object([
        ("name", TLS_SOCKET.into()),
        (
            "typed_config",
            Json::object([
                ("@type", UPSTREAM_TLS.into()),
                ("sni", host.into()),
                (
                    "common_tls_context",
                    Json::object([
                        ("alpn_protocols", vec!["h2"].into()),
                        (
                            "validation_context",
                            Json::object([
                                ca_validation(ca_file),
                                ("match_typed_subject_alt_names", Json::List(vec![san])),
                            ]),
                        ),
                    ]),
                ),
            ]),
        ),
    ])
}

pub(super) fn socket_address(address: &str, port: i64) -> Json {
    Json::object([(
        "socket_address",
        Json::object([("address", address.into()), ("port_value", port.into())]),
    )])
}

fn routes() -> Json {
    Json::List(
        ROUTES
            .iter()
            .map(|&(kind, value)| {
                Json::object([
                    (
                        "match",
                        Json::object([(kind, value.into()), ("grpc", Json::object([]))]),
                    ),
                    (
                        "route",
                        Json::object([
                            ("cluster", CLUSTER.into()),
                            ("timeout", ROUTE_TIMEOUT.into()),
                        ]),
                    ),
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
        // The ingress proxy forwards gRPC as cleartext HTTP/2 (prior knowledge).
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
        // Only the router: no filter that buffers a request, so a 64 MiB ByteStream
        // write streams through.
        ("http_filters", Json::List(vec![router])),
    ])
}

fn endpoint(s: &Server) -> Json {
    let host = s.host.as_str();
    Json::object([
        (
            "endpoint",
            Json::object([
                ("address", socket_address(host, i64::from(s.port))),
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
}

/// The cluster's default socket (for an endpoint no match names): TLS to the CA too,
/// never plain text.
pub(super) fn default_upstream_tls(ca_file: &str) -> Json {
    Json::object([
        ("name", TLS_SOCKET.into()),
        (
            "typed_config",
            Json::object([
                ("@type", UPSTREAM_TLS.into()),
                (
                    "common_tls_context",
                    Json::object([
                        ("alpn_protocols", vec!["h2"].into()),
                        ("validation_context", Json::object([ca_validation(ca_file)])),
                    ]),
                ),
            ]),
        ),
    ])
}

fn cluster(servers: &[Server]) -> Json {
    let socket_matches = servers
        .iter()
        .map(|s| {
            Json::object([
                ("name", s.host.as_str().into()),
                ("match", Json::object([("server", s.host.as_str().into())])),
                ("transport_socket", upstream_tls(&s.host, CA_FILE)),
            ])
        })
        .collect();
    Json::object([
        ("name", CLUSTER.into()),
        ("type", "STRICT_DNS".into()),
        ("dns_lookup_family", "V4_ONLY".into()),
        ("connect_timeout", "5s".into()),
        ("lb_policy", "ROUND_ROBIN".into()),
        // Envoy's default (50 %) sends traffic to every host, healthy or not, once half
        // are unhealthy; with one server that is always. 0: never.
        (
            "common_lb_config",
            Json::object([(
                "healthy_panic_threshold",
                Json::object([("value", 0i64.into())]),
            )]),
        ),
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
                        Json::object([("http2_protocol_options", Json::object([]))]),
                    ),
                ]),
            )]),
        ),
        (
            "load_assignment",
            Json::object([
                ("cluster_name", CLUSTER.into()),
                (
                    "endpoints",
                    Json::List(vec![Json::object([(
                        "lb_endpoints",
                        Json::List(servers.iter().map(endpoint).collect()),
                    )])]),
                ),
            ]),
        ),
        // Each server's own SNI and name check; the default (an endpoint no match
        // names) is TLS to the CA too, never plain text.
        ("transport_socket_matches", Json::List(socket_matches)),
        ("transport_socket", default_upstream_tls(CA_FILE)),
        // grpc.health.v1.Health/Check on each server's REAPI listener. The empty
        // service name is the server as a whole.
        (
            "health_checks",
            Json::List(vec![Json::object([
                ("timeout", "3s".into()),
                ("interval", "5s".into()),
                ("no_traffic_interval", "10s".into()),
                ("unhealthy_threshold", 2i64.into()),
                ("healthy_threshold", 1i64.into()),
                (
                    "grpc_health_check",
                    Json::object([("service_name", "".into())]),
                ),
            ])]),
        ),
    ])
}

/// The Envoy bootstrap: one listener, one cluster of `servers`.
pub fn envoy_config(servers: &[Server]) -> Json {
    let listener = Json::object([
        ("name", "reapi".into()),
        ("address", socket_address("0.0.0.0", LISTEN_PORT)),
        (
            "filter_chains",
            Json::List(vec![Json::object([(
                "filters",
                Json::List(vec![Json::object([
                    (
                        "name",
                        "envoy.filters.network.http_connection_manager".into(),
                    ),
                    ("typed_config", connection_manager()),
                ])]),
            )])]),
        ),
    ]);
    Json::object([
        (
            "admin",
            Json::object([("address", socket_address("127.0.0.1", ADMIN_PORT))]),
        ),
        (
            "static_resources",
            Json::object([
                ("listeners", Json::List(vec![listener])),
                ("clusters", Json::List(vec![cluster(servers)])),
            ]),
        ),
    ])
}

fn labels() -> Json {
    Json::object([
        ("app.kubernetes.io/name", APP.into()),
        ("app.kubernetes.io/part-of", "kbf-front".into()),
    ])
}

fn select_app() -> Json {
    Json::object([("app.kubernetes.io/name", APP.into())])
}

fn meta(name: &str, namespace: &str) -> Json {
    Json::object([
        ("name", name.into()),
        ("namespace", namespace.into()),
        ("labels", labels()),
    ])
}

fn object(api_version: &str, kind: &str, metadata: Json, body: (&str, Json)) -> Json {
    Json::object([
        ("apiVersion", api_version.into()),
        ("kind", kind.into()),
        ("metadata", metadata),
        body,
    ])
}

fn tcp_socket() -> Json {
    Json::object([("port", LISTEN_PORT.into())])
}

fn container(image: &str) -> Json {
    let readiness = Json::object([("tcpSocket", tcp_socket()), ("periodSeconds", 5i64.into())]);
    let liveness = Json::object([
        ("tcpSocket", tcp_socket()),
        ("periodSeconds", 10i64.into()),
        ("failureThreshold", 6i64.into()),
    ]);
    let mount = |name: &str, path: &str| {
        Json::object([
            ("name", name.into()),
            ("mountPath", path.into()),
            ("readOnly", true.into()),
        ])
    };
    Json::object([
        ("name", "envoy".into()),
        ("image", image.into()),
        (
            "args",
            vec!["--config-path", CONFIG_FILE, "--log-level", "info"].into(),
        ),
        (
            "ports",
            Json::List(vec![Json::object([
                ("name", "grpc".into()),
                ("containerPort", LISTEN_PORT.into()),
                ("protocol", "TCP".into()),
            ])]),
        ),
        ("readinessProbe", readiness),
        ("livenessProbe", liveness),
        (
            "resources",
            Json::object([
                (
                    "requests",
                    Json::object([("cpu", "100m".into()), ("memory", "128Mi".into())]),
                ),
                ("limits", Json::object([("memory", "512Mi".into())])),
            ]),
        ),
        (
            "securityContext",
            Json::object([
                ("allowPrivilegeEscalation", false.into()),
                ("readOnlyRootFilesystem", true.into()),
                ("runAsNonRoot", true.into()),
                ("runAsUser", ENVOY_UID.into()),
                ("runAsGroup", ENVOY_UID.into()),
                ("capabilities", Json::object([("drop", vec!["ALL"].into())])),
            ]),
        ),
        (
            "volumeMounts",
            Json::List(vec![mount("config", CONFIG_DIR), mount("ca", CA_DIR)]),
        ),
    ])
}

fn deployment(c: &EnvoyConfig, envoy_text: &str) -> Json {
    let volumes = Json::List(vec![
        Json::object([
            ("name", "config".into()),
            ("configMap", Json::object([("name", APP.into())])),
        ]),
        Json::object([
            ("name", "ca".into()),
            (
                "secret",
                Json::object([
                    ("secretName", c.ca_secret.as_str().into()),
                    (
                        "items",
                        Json::List(vec![Json::object([
                            ("key", "ca.pem".into()),
                            ("path", "ca.pem".into()),
                        ])]),
                    ),
                ]),
            ),
        ]),
    ]);
    let template = Json::object([
        (
            "metadata",
            Json::object([
                ("labels", labels()),
                (
                    "annotations",
                    Json::object([(
                        CONFIG_HASH_ANNOTATION,
                        sha256_hex(envoy_text).as_str().into(),
                    )]),
                ),
            ]),
        ),
        (
            "spec",
            Json::object([
                ("automountServiceAccountToken", false.into()),
                (
                    "securityContext",
                    Json::object([(
                        "seccompProfile",
                        Json::object([("type", "RuntimeDefault".into())]),
                    )]),
                ),
                ("containers", Json::List(vec![container(&c.image)])),
                ("volumes", volumes),
            ]),
        ),
    ]);
    let spec = Json::object([
        ("replicas", 1i64.into()),
        ("selector", Json::object([("matchLabels", select_app())])),
        (
            "strategy",
            Json::object([
                ("type", "RollingUpdate".into()),
                (
                    "rollingUpdate",
                    Json::object([("maxUnavailable", 0i64.into()), ("maxSurge", 1i64.into())]),
                ),
            ]),
        ),
        ("template", template),
    ]);
    object(
        "apps/v1",
        "Deployment",
        meta(APP, &c.namespace),
        ("spec", spec),
    )
}

fn manifests(c: &EnvoyConfig, envoy_text: &str) -> Json {
    let ns = c.namespace.as_str();
    let configmap = object(
        "v1",
        "ConfigMap",
        meta(APP, ns),
        ("data", Json::object([("envoy.json", envoy_text.into())])),
    );
    let service = object(
        "v1",
        "Service",
        meta(APP, ns),
        (
            "spec",
            Json::object([
                ("type", "ClusterIP".into()),
                ("selector", select_app()),
                // Not 443 and not named https: the operator would then dial it with TLS.
                (
                    "ports",
                    Json::List(vec![Json::object([
                        ("name", "grpc".into()),
                        ("port", LISTEN_PORT.into()),
                        ("targetPort", LISTEN_PORT.into()),
                        ("protocol", "TCP".into()),
                        ("appProtocol", "kubernetes.io/h2c".into()),
                    ])]),
                ),
            ]),
        ),
    );
    // Only the operator's proxies (in its namespace) reach Envoy; a CNI that does not
    // enforce NetworkPolicy ignores this.
    let from = Json::object([(
        "namespaceSelector",
        Json::object([(
            "matchLabels",
            Json::object([(
                "kubernetes.io/metadata.name",
                c.operator_namespace.as_str().into(),
            )]),
        )]),
    )]);
    let netpol = object(
        "networking.k8s.io/v1",
        "NetworkPolicy",
        meta(APP, ns),
        (
            "spec",
            Json::object([
                ("podSelector", Json::object([("matchLabels", select_app())])),
                ("policyTypes", vec!["Ingress"].into()),
                (
                    "ingress",
                    Json::List(vec![Json::object([
                        ("from", Json::List(vec![from])),
                        (
                            "ports",
                            Json::List(vec![Json::object([
                                ("protocol", "TCP".into()),
                                ("port", LISTEN_PORT.into()),
                            ])]),
                        ),
                    ])]),
                ),
            ]),
        ),
    );
    let ingress = object(
        "networking.k8s.io/v1",
        "Ingress",
        meta(INGRESS, ns),
        (
            "spec",
            Json::object([
                ("ingressClassName", "tailscale".into()),
                (
                    "defaultBackend",
                    Json::object([(
                        "service",
                        Json::object([
                            ("name", APP.into()),
                            ("port", Json::object([("number", LISTEN_PORT.into())])),
                        ]),
                    )]),
                ),
                // The MagicDNS name: <hostname>.<tailnet>.ts.net.
                (
                    "tls",
                    Json::List(vec![Json::object([(
                        "hosts",
                        vec![c.hostname.as_str()].into(),
                    )])]),
                ),
            ]),
        ),
    );
    Json::object([
        ("apiVersion", "v1".into()),
        ("kind", "List".into()),
        (
            "items",
            Json::List(vec![
                configmap,
                deployment(c, envoy_text),
                service,
                netpol,
                ingress,
            ]),
        ),
    ])
}

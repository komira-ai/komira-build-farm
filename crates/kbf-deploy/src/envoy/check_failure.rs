//! The failures `envoy check` reports ([`super::check`]): one variant per check, its
//! name the word after `FAIL`, its message the defect it lets through.

use super::check::{KINDS, SILENT};

/// One broken property of a render, and the defect it lets through.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckFailure {
    #[error("objects {got} are not one each of {want}", want = KINDS.join(", "))]
    Kinds { got: String },
    #[error("objects in namespaces {got}")]
    Namespace { got: String },
    #[error("the ConfigMap has no parseable envoy.json ({why})")]
    EnvoyConfig { why: String },
    #[error("the Envoy config is malformed ({why})")]
    EnvoyShape { why: String },
    #[error("ingressClassName {got} is not the tailscale IngressClass")]
    IngressClass { got: String },
    #[error("tls hosts {hosts} is not one MagicDNS label (the operator takes the first)")]
    IngressHost { hosts: String },
    #[error("the Ingress sends to {got}, not the Envoy Service")]
    IngressBackend { got: String },
    #[error("the Service has no port {port}")]
    IngressPort { port: String },
    #[error(
        "port 443 or a port named https makes the operator dial Envoy with TLS (https+insecure)"
    )]
    IngressCleartext,
    #[error("want one container, got {count}")]
    Container { count: usize },
    #[error("image {image} is not pinned by digest")]
    ImagePinned { image: String },
    #[error("the pod template's config hash is stale, so a config change does not roll the pods")]
    ConfigHash,
    #[error("the Service selects {selector} but the pods carry {labels}")]
    ServiceSelector { selector: String, labels: String },
    #[error("Envoy reads {config_path} but the ConfigMap is mounted at {mount}")]
    ConfigMount { config_path: String, mount: String },
    #[error("no Secret (the internal CA) is mounted")]
    CaMount,
    #[error("the NetworkPolicy selects {selector}, not the Envoy pods {labels}")]
    NetpolPods { selector: String, labels: String },
    #[error("want one ingress rule, got {count}")]
    NetpolRule { count: usize },
    #[error("the NetworkPolicy admits {ports}, not TCP {port} (Envoy's listener)")]
    NetpolPort { ports: String, port: String },
    #[error("the NetworkPolicy admits {from}, not the operator's namespace {namespace}")]
    NetpolFrom { from: String, namespace: String },
    #[error("want one listener, got {count}")]
    Listener { count: usize },
    #[error("Envoy listens on {port} but the Service sends to {service_port}")]
    ListenerPort { port: String, service_port: String },
    #[error("Envoy listens on {address}, so the Service (the pod's address) cannot reach it")]
    ListenerAddress { address: String },
    #[error("want one chain with one HTTP connection manager")]
    Hcm,
    #[error("the listener expects TLS, but the Tailscale proxy forwards cleartext HTTP/2")]
    ListenerPlaintext,
    #[error("codec {codec} cannot take gRPC's HTTP/2")]
    Codec { codec: String },
    #[error(
        "{at}: max_stream_duration {duration} ends every stream older than that, silent or not"
    )]
    MaxStreamDuration { at: String, duration: String },
    #[error("request_timeout {timeout} cuts a ByteStream Write still uploading after that")]
    RequestTimeout { timeout: String },
    #[error(
        "http_filters {filters} is not the router alone (a buffering filter caps a 64 MiB write)"
    )]
    Filters { filters: String },
    #[error("want one virtual host for every authority (the proxy's Host is not ours to pin)")]
    Vhost,
    #[error("a catch-all route sends any path (grpc.health.v1, the operator API) to the servers")]
    NoCatchAll,
    #[error(
        "match {matched} is not a path or prefix alone (unmodelled keys {extra}): the route choice this check models is not Envoy's"
    )]
    RouteMatch { matched: String, extra: String },
    #[error("route {matched} has {extra}: a rewrite, retry or redirect this check does not model")]
    RouteAction { matched: String, extra: String },
    #[error("no route for {path}")]
    Route { path: String },
    #[error("{path} goes to {cluster}, not a defined cluster")]
    RouteCluster { path: String, cluster: String },
    #[error(
        "{path} is cut {secs} s into its response (route timeout); it must be 0, with the idle timeout bounding silence"
    )]
    RouteTimeout { path: String, secs: String },
    #[error("{path} is cut after {secs} s of silence, under the {silent} s silent stream", silent = SILENT.as_secs())]
    IdleTimeout { path: String, secs: String },
    #[error("cluster {cluster} has no health check")]
    HealthCheck { cluster: String },
    #[error("cluster {cluster}: a health check that is not grpc.health.v1")]
    HealthCheckGrpc { cluster: String },
    #[error("cluster {cluster}: the health check picks its own transport socket")]
    HealthCheckTls { cluster: String },
    #[error(
        "cluster {cluster}: kbf-server answers service {service} NOT_FOUND, so no server is ever healthy"
    )]
    HealthCheckService { cluster: String, service: String },
    #[error(
        "cluster {cluster}: healthy_panic_threshold {value} sends traffic to unhealthy servers"
    )]
    PanicThreshold { cluster: String, value: String },
    #[error("cluster {cluster}: upstream is not HTTP/2")]
    UpstreamH2 { cluster: String },
    #[error("{at}: not TLS (the hop to the servers must verify the internal CA)")]
    UpstreamTls { at: String },
    #[error("{at}: trusted_ca {got} is not the mounted internal CA {want}")]
    UpstreamCa {
        at: String,
        got: String,
        want: String,
    },
    #[error("{at}: ALPN without h2")]
    UpstreamAlpn { at: String },
    #[error("{at}: SNI {sni}")]
    UpstreamSni { at: String, sni: String },
    #[error("{at}: the certificate's name is not checked against {host}")]
    UpstreamSan { at: String, host: String },
    #[error("cluster {cluster} has no server")]
    Endpoints { cluster: String },
    #[error("server {host}: no transport socket match, so its name is not checked")]
    EndpointTlsMatch { host: String },
    #[error("the box config has {got} listeners, not the REAPI one and the worker one")]
    Listeners { got: String },
    #[error(
        "the REAPI listener has no TLS with a certificate and h2: REAPI clients dial it with TLS"
    )]
    ListenerTls,
    #[error(
        "{path} is cut after {got} s of silence, not never: an action's own timeout is uncapped, so Execute and WaitExecution need a route idle_timeout of 0s"
    )]
    ExecuteIdle { path: String, got: String },
    #[error("{path}: retry policy outside what repeats no work ({why})")]
    RetryRoutes { path: String, why: String },
    #[error(
        "cluster {cluster}: close_connections_on_host_health_failure cuts the open streams of an instance that is only draining"
    )]
    HealthKeepsStreams { cluster: String },
    #[error("cluster {cluster}: {why}, so a concurrent stream past it is refused")]
    Breakers { cluster: String, why: String },
    #[error("cluster {cluster}: no HTTP/2 keepalive on the upstream connections")]
    UpstreamKeepalive { cluster: String },
    #[error(
        "cluster {cluster}: upstream max_concurrent_streams is {got}, not at most kbf's {want}: a burst of streams is sent down one fresh connection and refused"
    )]
    UpstreamStreams {
        cluster: String,
        got: String,
        want: String,
    },
    #[error("cluster {cluster}: {why}")]
    Instances { cluster: String, why: String },
    #[error("port {port} is used twice ({first} and {second})")]
    InstancePorts {
        port: String,
        first: String,
        second: String,
    },
    #[error("the worker listener is not a tcp_proxy alone to kbf_workers ({why})")]
    WorkerListener { why: String },
    #[error("{at}: TLS in Envoy on the worker hop loses the daemon's certificate identity")]
    WorkerTls { at: String },
    #[error(
        "the worker proxy's idle_timeout is {got}, not 0s: a daemon's session is quiet between heartbeats"
    )]
    WorkerIdle { got: String },
    #[error("cluster kbf_workers: {why}")]
    WorkerHealth { why: String },
    #[error("the admin interface listens on {address}, not loopback")]
    AdminLoopback { address: String },
    #[error(
        "--parent-shutdown-time-s is {got}, under the longest action ({want} s): the old epoch is closed with its streams open"
    )]
    ParentShutdown { got: String, want: String },
    #[error("--drain-time-s {got} is not below --parent-shutdown-time-s {parent}")]
    DrainTime { got: String, parent: String },
}

impl CheckFailure {
    /// Every check's name, in the order [`check`](super::check::check) runs them.
    pub const NAMES: [&'static str; 49] = [
        "kinds",
        "namespace",
        "envoy-config",
        "ingress-class",
        "ingress-host",
        "ingress-backend",
        "ingress-port",
        "ingress-cleartext",
        "container",
        "image-pinned",
        "config-hash",
        "service-selector",
        "config-mount",
        "ca-mount",
        "netpol-pods",
        "netpol-rule",
        "netpol-port",
        "netpol-from",
        "envoy-shape",
        "listener",
        "listener-port",
        "listener-address",
        "hcm",
        "listener-plaintext",
        "codec",
        "max-stream-duration",
        "request-timeout",
        "filters",
        "vhost",
        "no-catch-all",
        "route-match",
        "route-action",
        "route",
        "route-cluster",
        "route-timeout",
        "idle-timeout",
        "health-check",
        "health-check-grpc",
        "health-check-tls",
        "health-check-service",
        "panic-threshold",
        "upstream-h2",
        "upstream-tls",
        "upstream-ca",
        "upstream-alpn",
        "upstream-sni",
        "upstream-san",
        "endpoints",
        "endpoint-tls-match",
    ];

    /// The checks only the box topology runs ([`super::box_check`]), in the order it runs
    /// them.
    pub const BOX_NAMES: [&'static str; 17] = [
        "listeners",
        "listener-tls",
        "execute-idle-off",
        "retry-routes",
        "health-keeps-streams",
        "breakers",
        "upstream-keepalive",
        "upstream-streams",
        "instances",
        "instance-ports",
        "worker-listener",
        "worker-tls",
        "worker-idle",
        "worker-health",
        "admin-loopback",
        "parent-shutdown",
        "drain-time",
    ];

    /// The check's name, the word after `FAIL`.
    pub fn name(&self) -> &'static str {
        match self {
            CheckFailure::Kinds { .. } => "kinds",
            CheckFailure::Namespace { .. } => "namespace",
            CheckFailure::EnvoyConfig { .. } => "envoy-config",
            CheckFailure::EnvoyShape { .. } => "envoy-shape",
            CheckFailure::IngressClass { .. } => "ingress-class",
            CheckFailure::IngressHost { .. } => "ingress-host",
            CheckFailure::IngressBackend { .. } => "ingress-backend",
            CheckFailure::IngressPort { .. } => "ingress-port",
            CheckFailure::IngressCleartext => "ingress-cleartext",
            CheckFailure::Container { .. } => "container",
            CheckFailure::ImagePinned { .. } => "image-pinned",
            CheckFailure::ConfigHash => "config-hash",
            CheckFailure::ServiceSelector { .. } => "service-selector",
            CheckFailure::ConfigMount { .. } => "config-mount",
            CheckFailure::CaMount => "ca-mount",
            CheckFailure::NetpolPods { .. } => "netpol-pods",
            CheckFailure::NetpolRule { .. } => "netpol-rule",
            CheckFailure::NetpolPort { .. } => "netpol-port",
            CheckFailure::NetpolFrom { .. } => "netpol-from",
            CheckFailure::Listener { .. } => "listener",
            CheckFailure::ListenerPort { .. } => "listener-port",
            CheckFailure::ListenerAddress { .. } => "listener-address",
            CheckFailure::Hcm => "hcm",
            CheckFailure::ListenerPlaintext => "listener-plaintext",
            CheckFailure::Codec { .. } => "codec",
            CheckFailure::MaxStreamDuration { .. } => "max-stream-duration",
            CheckFailure::RequestTimeout { .. } => "request-timeout",
            CheckFailure::Filters { .. } => "filters",
            CheckFailure::Vhost => "vhost",
            CheckFailure::NoCatchAll => "no-catch-all",
            CheckFailure::RouteMatch { .. } => "route-match",
            CheckFailure::RouteAction { .. } => "route-action",
            CheckFailure::Route { .. } => "route",
            CheckFailure::RouteCluster { .. } => "route-cluster",
            CheckFailure::RouteTimeout { .. } => "route-timeout",
            CheckFailure::IdleTimeout { .. } => "idle-timeout",
            CheckFailure::HealthCheck { .. } => "health-check",
            CheckFailure::HealthCheckGrpc { .. } => "health-check-grpc",
            CheckFailure::HealthCheckTls { .. } => "health-check-tls",
            CheckFailure::HealthCheckService { .. } => "health-check-service",
            CheckFailure::PanicThreshold { .. } => "panic-threshold",
            CheckFailure::UpstreamH2 { .. } => "upstream-h2",
            CheckFailure::UpstreamTls { .. } => "upstream-tls",
            CheckFailure::UpstreamCa { .. } => "upstream-ca",
            CheckFailure::UpstreamAlpn { .. } => "upstream-alpn",
            CheckFailure::UpstreamSni { .. } => "upstream-sni",
            CheckFailure::UpstreamSan { .. } => "upstream-san",
            CheckFailure::Endpoints { .. } => "endpoints",
            CheckFailure::EndpointTlsMatch { .. } => "endpoint-tls-match",
            CheckFailure::Listeners { .. } => "listeners",
            CheckFailure::ListenerTls => "listener-tls",
            CheckFailure::ExecuteIdle { .. } => "execute-idle-off",
            CheckFailure::RetryRoutes { .. } => "retry-routes",
            CheckFailure::HealthKeepsStreams { .. } => "health-keeps-streams",
            CheckFailure::Breakers { .. } => "breakers",
            CheckFailure::UpstreamKeepalive { .. } => "upstream-keepalive",
            CheckFailure::UpstreamStreams { .. } => "upstream-streams",
            CheckFailure::Instances { .. } => "instances",
            CheckFailure::InstancePorts { .. } => "instance-ports",
            CheckFailure::WorkerListener { .. } => "worker-listener",
            CheckFailure::WorkerTls { .. } => "worker-tls",
            CheckFailure::WorkerIdle { .. } => "worker-idle",
            CheckFailure::WorkerHealth { .. } => "worker-health",
            CheckFailure::AdminLoopback { .. } => "admin-loopback",
            CheckFailure::ParentShutdown { .. } => "parent-shutdown",
            CheckFailure::DrainTime { .. } => "drain-time",
        }
    }

    /// The line `envoy check` prints: `FAIL <check>: <why>`.
    pub fn line(&self) -> String {
        format!("FAIL {}: {self}", self.name())
    }
}

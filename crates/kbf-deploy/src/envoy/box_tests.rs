//! The box topology: its config, its golden render, and `envoy check --topology box` on
//! the example render and on one planted defect at a time.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::box_check::{BoxSpec, check_box};
use super::box_config::{BOX_KEYS, BoxConfig, BoxError};
use super::box_render::{BoxRendered, render_box};
use super::check::CheckFailure;
use super::check_tests::{at, push, remove, set, set_new};

const EXAMPLE: &str = include_str!("../../templates/envoy/box.example");
const GOLDEN_ENVOY: &str = include_str!("../../tests/golden/envoy/box/envoy.json");
const GOLDEN_ARGS: &str = include_str!("../../tests/golden/envoy/box/envoy.args");

fn config(text: &str) -> BoxConfig {
    BoxConfig::parse(text).expect("valid config")
}

fn rendered(text: &str) -> BoxRendered {
    render_box(&config(text))
}

/// The example with `key`'s value replaced.
fn with(key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let lines: Vec<String> = EXAMPLE
        .lines()
        .map(|l| {
            if l.starts_with(&prefix) {
                format!("{prefix}{value}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    assert!(lines.iter().any(|l| *l == format!("{prefix}{value}")));
    lines.join("\n") + "\n"
}

fn refusal(text: &str) -> String {
    BoxConfig::parse(text).expect_err("refused").to_string()
}

const REAPI: &str = "/static_resources/listeners/0";
const WORKER: &str = "/static_resources/listeners/1";
const HCM: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config";
const ROUTES: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes";
const RC: &str = "/static_resources/clusters/0";
const WC: &str = "/static_resources/clusters/1";
const PROXY: &str = "/static_resources/listeners/1/filter_chains/0/filters/0/typed_config";

/// The check on `envoy` and `args` of the example's numbers.
fn run(envoy: &Value, args: &str) -> Vec<CheckFailure> {
    check_box(envoy, args, &BoxSpec::from(&config(EXAMPLE)))
}

fn parsed(r: &BoxRendered) -> Value {
    serde_json::from_str(&r.envoy).expect("JSON")
}

/// Catches drift in the box render: a key, a value or an order changed. The golden is
/// this renderer's own first output (the box has no earlier renderer), reviewed by hand
/// and validated by Envoy itself (`tests/envoy_validate.rs`), so it pins what was
/// reviewed rather than proves it right.
#[test]
fn the_example_renders_the_golden_bytes() {
    let r = rendered(EXAMPLE);
    assert_eq!(r.envoy, GOLDEN_ENVOY);
    assert_eq!(r.args, GOLDEN_ARGS);
}

/// Catches a render that hard-codes a number the config gives: every port, the SNI
/// name, the files, the breaker limits and the parent shutdown follow the settings.
#[test]
fn the_settings_reach_the_render() {
    let text = EXAMPLE
        .replace("kbf.example.internal", "farm.box.test")
        .replace("18980", "30000")
        .replace("8980", "7000")
        .replace("8981", "7001")
        .replace("9901", "7002")
        .replace("=10000", "=500")
        .replace("=65536", "=4000")
        .replace("=21600", "=100")
        .replace("/etc/kbf/tls", "/srv/pki");
    let r = rendered(&text);
    for want in [
        "\"farm.box.test\"",
        "\"port_value\": 7000",
        "\"port_value\": 7001",
        "\"port_value\": 7002",
        "\"port_value\": 30000",
        "\"port_value\": 30001",
        "\"port_value\": 30002",
        "\"port_value\": 30010",
        "\"port_value\": 30012",
        "/srv/pki/reapi.pem",
        "/srv/pki/reapi.key",
        "/srv/pki/ca.pem",
        "\"max_requests\": 4000",
        "\"max_connections\": 16",
    ] {
        assert!(r.envoy.contains(want), "{want}");
    }
    assert!(r.args.starts_with("--parent-shutdown-time-s 1000\n"));
    let spec = BoxSpec::from(&config(&text));
    assert_eq!(check_box(&parsed(&r), &r.args, &spec), vec![]);
}

/// Catches a check that fails a correct render (it would then catch every mutant for
/// nothing).
#[test]
fn the_example_passes_every_box_check() {
    let r = rendered(EXAMPLE);
    assert_eq!(run(&parsed(&r), &r.args), vec![]);
}

/// Catches a config that accepts what the render cannot use: each line is one refusal.
#[test]
fn the_config_refuses_what_the_render_cannot_use() {
    assert_eq!(config(EXAMPLE).instances()[1].api, 18992);
    let cases = [
        (with("KBF_BOX_REAPI_PORT", "0"), "KBF_BOX_REAPI_PORT='0'"),
        (with("KBF_BOX_REAPI_PORT", "65536"), "65536"),
        (with("KBF_BOX_REAPI_PORT", "80a"), "80a"),
        (
            with("KBF_BOX_REAPI_PORT", ""),
            "KBF_BOX_REAPI_PORT is not set",
        ),
        (with("KBF_BOX_REAPI_PORT", "99999999999"), "99999999999"),
        (with("KBF_BOX_REAPI_MAX_STREAMS", "32768"), "32768"),
        (with("KBF_BOX_HOST", "Not_A_Name"), "KBF_BOX_HOST"),
        (
            with("KBF_BOX_CERT_FILE", "relative/pem"),
            "KBF_BOX_CERT_FILE",
        ),
        (
            with("KBF_BOX_CA_FILE", "/has space/ca.pem"),
            "KBF_BOX_CA_FILE",
        ),
        (with("KBF_BOX_KEY_FILE", "/a\"b"), "KBF_BOX_KEY_FILE"),
        (
            with("KBF_BOX_ADMIN_PORT", "8980"),
            "KBF_BOX_REAPI_PORT and KBF_BOX_ADMIN_PORT are both port 8980",
        ),
        (with("KBF_BOX_BACKEND_PORT_BASE", "8978"), "instance a API"),
        (with("KBF_BOX_BACKEND_PORT_BASE", "65530"), "run past 65535"),
        (
            format!("{EXAMPLE}KBF_BOX_EXTRA=1\n"),
            "unknown key KBF_BOX_EXTRA",
        ),
    ];
    for (text, want) in cases {
        let got = refusal(&text);
        assert!(got.contains(want), "{got} lacks {want}");
    }
    for key in BOX_KEYS {
        let without: Vec<&str> = EXAMPLE
            .lines()
            .filter(|l| !l.starts_with(&format!("{key}=")))
            .collect();
        let got = refusal(&without.join("\n"));
        assert!(got.contains(key), "{got}");
    }
    let err = BoxConfig::parse(&with("KBF_BOX_BACKEND_PORT_BASE", "65530")).unwrap_err();
    assert!(matches!(err, BoxError::BasePastRange(65530)));
}

/// Catches an example that is not the one-host default the docs show: both ends of the
/// parent shutdown arithmetic.
#[test]
fn the_parent_shutdown_is_the_longest_action_plus_a_quarter_hour() {
    assert_eq!(config(EXAMPLE).parent_shutdown_secs(), 21600 + 900);
}

/// A change to the parsed `envoy.json` and the text of `envoy.args`.
type BoxPlant = fn(&mut Value, &mut String);

/// One planted defect and the checks that must catch it, exactly.
struct Mutant {
    check: &'static str,
    defect: &'static str,
    plant: BoxPlant,
}

const fn m(check: &'static str, defect: &'static str, plant: BoxPlant) -> Mutant {
    Mutant {
        check,
        defect,
        plant,
    }
}

const MUTANTS: &[Mutant] = &[
    m(
        "route-timeout",
        "Execute's route timeout back to Envoy's 15 s",
        |e, _| remove(e, &format!("{ROUTES}/0/route"), "timeout"),
    ),
    m(
        "max-stream-duration",
        "a 300 s cap on every stream's life",
        |e, _| {
            set_new(
                e,
                &format!("{HCM}/common_http_protocol_options"),
                json!({"max_stream_duration": "300s"}),
            )
        },
    ),
    m(
        "idle-timeout",
        "the stream idle timeout back to Envoy's 300 s",
        |e, _| remove(e, HCM, "stream_idle_timeout"),
    ),
    m(
        "execute-idle-off",
        "Execute's own idle timeout left to the connection manager's",
        |e, _| remove(e, &format!("{ROUTES}/0/route"), "idle_timeout"),
    ),
    m(
        "execute-idle-off",
        "WaitExecution's idle timeout 2 h",
        |e, _| set(e, &format!("{ROUTES}/1/route/idle_timeout"), json!("7200s")),
    ),
    m(
        "breakers",
        "no circuit breakers on the REAPI cluster: about a thousand actions",
        |e, _| remove(e, RC, "circuit_breakers"),
    ),
    m(
        "breakers",
        "max_connections under ceiling / max_concurrent_streams",
        |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/max_connections"),
                json!(1),
            )
        },
    ),
    m("breakers", "max_requests under the ceiling", |e, _| {
        set(
            e,
            &format!("{RC}/circuit_breakers/thresholds/0/max_requests"),
            json!(1024),
        )
    }),
    m(
        "breakers",
        "the limits on a HIGH priority threshold only, which does not apply",
        |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/priority"),
                json!("HIGH"),
            )
        },
    ),
    m(
        "breakers",
        "no limits on the worker cluster: 1024 connections",
        |e, _| remove(e, WC, "circuit_breakers"),
    ),
    m(
        "health-keeps-streams",
        "a failed check closes the REAPI cluster's connections",
        |e, _| {
            set(
                e,
                &format!("{RC}/close_connections_on_host_health_failure"),
                json!(true),
            )
        },
    ),
    m(
        "health-keeps-streams",
        "a failed check closes a daemon's session",
        |e, _| {
            set(
                e,
                &format!("{WC}/close_connections_on_host_health_failure"),
                json!(true),
            )
        },
    ),
    m(
        "worker-idle",
        "the worker proxy's idle timeout left at Envoy's 1 h",
        |e, _| remove(e, PROXY, "idle_timeout"),
    ),
    m(
        "worker-idle",
        "the worker proxy's idle timeout 1 h written out",
        |e, _| set(e, &format!("{PROXY}/idle_timeout"), json!("3600s")),
    ),
    m("retry-routes", "a retry policy on ByteStream", |e, _| {
        set_new(
            e,
            &format!("{ROUTES}/5/route/retry_policy"),
            json!({"retry_on": "connect-failure"}),
        )
    }),
    m("retry-routes", "a retry on unavailable", |e, _| {
        set(
            e,
            &format!("{ROUTES}/0/route/retry_policy/retry_on"),
            json!("connect-failure,unavailable"),
        )
    }),
    m(
        "retry-routes",
        "a retry policy with status codes the check does not model",
        |e, _| {
            set_new(
                e,
                &format!("{ROUTES}/0/route/retry_policy/retriable_status_codes"),
                json!([503]),
            )
        },
    ),
    m(
        "instances",
        "instance b missing from the REAPI cluster",
        |e, _| {
            at(e, &format!("{RC}/load_assignment/endpoints/0/lb_endpoints"))
                .as_array_mut()
                .expect("endpoints")
                .pop();
        },
    ),
    m(
        "instances",
        "instance b missing from the worker cluster",
        |e, _| {
            at(e, &format!("{WC}/load_assignment/endpoints/0/lb_endpoints"))
                .as_array_mut()
                .expect("endpoints")
                .pop();
        },
    ),
    m(
        "instance-ports",
        "instance b's worker port is instance a's REAPI port",
        |e, _| {
            set(
                e,
                &format!(
                    "{WC}/load_assignment/endpoints/0/lb_endpoints/1/endpoint/address/socket_address/port_value"
                ),
                json!(18980),
            )
        },
    ),
    m(
        "instance-ports",
        "the admin port is the public worker port",
        |e, _| set(e, "/admin/address/socket_address/port_value", json!(8981)),
    ),
    m(
        "parent-shutdown",
        "the default 900 s parent shutdown",
        |_, a| {
            *a = a.replace(
                "--parent-shutdown-time-s 22500\n",
                "--parent-shutdown-time-s 900\n",
            );
        },
    ),
    m("parent-shutdown", "no parent shutdown flag", |_, a| {
        *a = a.replace("--parent-shutdown-time-s 22500\n", "");
    }),
    m(
        "parent-shutdown",
        "a parent shutdown that is not a number",
        |_, a| {
            *a = a.replace("22500", "soon");
        },
    ),
    m(
        "drain-time",
        "a drain time over the parent shutdown",
        |_, a| {
            *a = a.replace("--drain-time-s 60\n", "--drain-time-s 30000\n");
        },
    ),
    m("drain-time", "a drain time that is not a number", |_, a| {
        *a = a.replace("--drain-time-s 60\n", "--drain-time-s soon\n");
    }),
    m(
        "admin-loopback",
        "the admin interface on every address",
        |e, _| set(e, "/admin/address/socket_address/address", json!("0.0.0.0")),
    ),
    m("listeners", "the worker listener missing", |e, _| {
        at(e, "/static_resources/listeners")
            .as_array_mut()
            .expect("listeners")
            .pop();
    }),
    m("listeners", "a third listener", |e, _| {
        let mut extra = at(e, WORKER).clone();
        set(&mut extra, "/name", json!("spare"));
        set(
            &mut extra,
            "/address/socket_address/port_value",
            json!(7777),
        );
        push(e, "/static_resources/listeners", extra);
    }),
    m(
        "listener-tls",
        "the REAPI listener in plain text",
        |e, _| remove(e, &format!("{REAPI}/filter_chains/0"), "transport_socket"),
    ),
    m(
        "listener-tls",
        "the REAPI listener with TLS but no certificate",
        |e, _| {
            set(
                e,
                &format!(
                    "{REAPI}/filter_chains/0/transport_socket/typed_config/common_tls_context/tls_certificates"
                ),
                json!([]),
            )
        },
    ),
    m(
        "worker-listener",
        "the worker listener proxies to the REAPI cluster",
        |e, _| set(e, &format!("{PROXY}/cluster"), json!("kbf_reapi")),
    ),
    m(
        "worker-listener",
        "the worker listener has two filters",
        |e, _| {
            let extra = at(e, &format!("{WORKER}/filter_chains/0/filters/0")).clone();
            push(e, &format!("{WORKER}/filter_chains/0/filters"), extra);
        },
    ),
    m(
        "worker-listener",
        "the worker listener has two chains",
        |e, _| {
            let extra = at(e, &format!("{WORKER}/filter_chains/0")).clone();
            push(e, &format!("{WORKER}/filter_chains"), extra);
        },
    ),
    m(
        "worker-tls",
        "Envoy ends the daemons' mutual TLS",
        |e, _| {
            set_new(
                e,
                &format!("{WORKER}/filter_chains/0/transport_socket"),
                json!({"name": "envoy.transport_sockets.tls"}),
            )
        },
    ),
    m("worker-tls", "the worker cluster dials with TLS", |e, _| {
        set_new(
            e,
            &format!("{WC}/transport_socket"),
            json!({"name": "envoy.transport_sockets.tls"}),
        )
    }),
    m(
        "worker-health",
        "no health check on the worker cluster",
        |e, _| set(e, &format!("{WC}/health_checks"), json!([])),
    ),
    m(
        "worker-health",
        "a TCP connect check of the worker port",
        |e, _| {
            set(
                e,
                &format!("{WC}/health_checks/0"),
                json!({"tcp_health_check": {}}),
            )
        },
    ),
    m(
        "worker-health",
        "an instance with no health port: its worker port would be checked",
        |e, _| {
            remove(
                e,
                &format!("{WC}/load_assignment/endpoints/0/lb_endpoints/0/endpoint"),
                "health_check_config",
            )
        },
    ),
    m(
        "worker-health",
        "the worker cluster sends to unhealthy instances",
        |e, _| {
            remove(
                e,
                &format!("{WC}/common_lb_config"),
                "healthy_panic_threshold",
            )
        },
    ),
    m(
        "upstream-keepalive",
        "no HTTP/2 keepalive upstream",
        |e, _| {
            remove(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options"
                ),
                "connection_keepalive",
            )
        },
    ),
    m(
        "upstream-streams",
        "no max_concurrent_streams upstream: Envoy assumes unlimited until SETTINGS arrive",
        |e, _| {
            remove(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options"
                ),
                "max_concurrent_streams",
            )
        },
    ),
    m(
        "upstream-streams",
        "max_concurrent_streams above kbf's limit",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options/max_concurrent_streams"
                ),
                json!(10001),
            )
        },
    ),
    m(
        "health-check",
        "no health check on the REAPI cluster",
        |e, _| set(e, &format!("{RC}/health_checks"), json!([])),
    ),
    m(
        "endpoint-tls-match",
        "an instance with no socket match: its name is not checked",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/load_assignment/endpoints/0/lb_endpoints/1/metadata/filter_metadata/envoy.transport_socket_match/server"
                ),
                json!("other"),
            )
        },
    ),
    m(
        "upstream-ca",
        "the upstream trusts a CA other than the config's",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/transport_socket_matches/0/transport_socket/typed_config/common_tls_context/validation_context/trusted_ca/filename"
                ),
                json!("/etc/ssl/other.pem"),
            )
        },
    ),
    m("route", "ByteStream not routed", |e, _| {
        at(e, ROUTES).as_array_mut().expect("routes").remove(5);
    }),
    m(
        "request-timeout",
        "a request timeout cuts a 64 MiB write",
        |e, _| set_new(e, &format!("{HCM}/request_timeout"), json!("60s")),
    ),
];

/// The box mutants: each planted defect makes exactly its check fail, and every check
/// only the box runs is reached by some row. Catches a check that cannot fail, a check
/// that fires on a neighbour's defect (the line would name the wrong cause), and a check
/// with no row.
#[test]
fn each_box_mutant_is_caught_by_exactly_its_check() {
    let mut wrong = Vec::new();
    let mut caught = BTreeSet::new();
    for mutant in MUTANTS {
        let r = rendered(EXAMPLE);
        let mut envoy = parsed(&r);
        let mut args = r.args.clone();
        (mutant.plant)(&mut envoy, &mut args);
        assert!(
            envoy != parsed(&r) || args != r.args,
            "{}: the plant changed nothing",
            mutant.defect
        );
        let failures = run(&envoy, &args);
        for f in &failures {
            let line = f.line();
            let why = line.strip_prefix(&format!("FAIL {}: ", f.name()));
            assert!(why.is_some_and(|w| !w.is_empty()), "{line}");
        }
        let got: BTreeSet<&str> = failures.iter().map(CheckFailure::name).collect();
        if got != BTreeSet::from([mutant.check]) {
            wrong.push(format!(
                "{} ({}): failed {got:?}",
                mutant.check, mutant.defect
            ));
        }
        caught.extend(got);
    }
    assert_eq!(wrong, Vec::<String>::new());
    let names: BTreeSet<&str> = CheckFailure::BOX_NAMES.into_iter().collect();
    assert_eq!(names.len(), CheckFailure::BOX_NAMES.len(), "a name twice");
    let box_caught: BTreeSet<&str> = caught.intersection(&names).copied().collect();
    assert_eq!(box_caught, names, "a box check no mutant reaches");
}

/// Renders Envoy reads the same way as the example: each must still pass. Catches a
/// check stricter than Envoy, which would refuse a correct box: an absent
/// close_connections flag (Envoy's default is false), an absent priority (DEFAULT), a
/// route idle timeout written with a fraction, a retry list in the other order, a
/// larger limit, a larger drain gap, extra whitespace-free flags.
#[test]
fn renders_envoy_reads_the_same_way_pass() {
    let controls: [(&str, BoxPlant); 7] = [
        ("close_connections flag absent", |e, _| {
            remove(e, RC, "close_connections_on_host_health_failure")
        }),
        ("breaker priority absent", |e, _| {
            remove(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0"),
                "priority",
            )
        }),
        ("a bigger request limit", |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/max_requests"),
                json!(1_000_000),
            )
        }),
        ("a retry list in the other order", |e, _| {
            set(
                e,
                &format!("{ROUTES}/0/route/retry_policy/retry_on"),
                json!("refused-stream,connect-failure"),
            )
        }),
        ("a route without a retry policy", |e, _| {
            remove(e, &format!("{ROUTES}/2/route"), "retry_policy")
        }),
        ("parent shutdown exactly the longest action", |_, a| {
            *a = a.replace("22500", "21600");
        }),
        ("a longer drain, still under the parent shutdown", |_, a| {
            *a = a.replace("--drain-time-s 60", "--drain-time-s 600");
        }),
    ];
    for (name, plant) in controls {
        let r = rendered(EXAMPLE);
        let (mut envoy, mut args) = (parsed(&r), r.args.clone());
        plant(&mut envoy, &mut args);
        assert_eq!(run(&envoy, &args), vec![], "{name}");
    }
}

/// Catches a check that reads the REAPI cluster's endpoints by address, as the front
/// does: the box's addresses are loopback for both instances, and the name checked is
/// the hostname.
#[test]
fn a_box_server_name_is_checked_by_hostname() {
    let r = rendered(EXAMPLE);
    let mut e = parsed(&r);
    set(
        &mut e,
        &format!("{RC}/transport_socket_matches/0/transport_socket/typed_config/sni"),
        json!("127.0.0.1"),
    );
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert_eq!(failures, ["upstream-sni", "upstream-sni"]);
}

/// Catches a duration the box checks cannot read stopping silently instead of being
/// reported as a malformed config.
#[test]
fn an_unreadable_duration_is_a_shape_failure() {
    let r = rendered(EXAMPLE);
    let mut e = parsed(&r);
    set(&mut e, &format!("{PROXY}/idle_timeout"), json!("forever"));
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert!(failures.contains(&"worker-idle"), "{failures:?}");
    set(
        &mut e,
        &format!(
            "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options/connection_keepalive/interval"
        ),
        json!("often"),
    );
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert!(failures.contains(&"envoy-shape"), "{failures:?}");
}

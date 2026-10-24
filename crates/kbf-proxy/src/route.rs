//! Which member port, auth class and idle timeout a request path has.
//!
//! The table is closed: a path that matches no prefix is [`Route::Unknown`] and is answered
//! by the proxy, never forwarded. A child therefore never sees a path the proxy did not
//! classify, and a new child service needs a line here and a test.

use std::time::Duration;

/// A listener of a member frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// REAPI and ByteStream.
    Reapi,
    /// Worker sessions, membership and the release service.
    Worker,
    /// The operator API (`/v1/*`).
    Operator,
    /// Enrollment (`/kbf.enroll.v1.Enroll/*`): a node that has no certificate yet. The child
    /// serves it on a listener of its own that authenticates the server only; the port is
    /// fixed here so the child's unit adds the listener without a proxy change.
    Enroll,
}

impl Target {
    /// The offset of this listener's port from the member's base port.
    #[must_use]
    pub const fn offset(self) -> u16 {
        match self {
            Self::Reapi => 0,
            Self::Worker => 1,
            Self::Operator => 2,
            Self::Enroll => 4,
        }
    }

    /// Whether the listener speaks gRPC (the operator API is plain HTTP).
    #[must_use]
    pub const fn grpc(self) -> bool {
        !matches!(self, Self::Operator)
    }
}

/// What the proxy does with a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Forward to a member's listener.
    Child(Target),
    /// The read-only web UI: `/ui` and `/ui/...`, forwarded to a member's operator listener
    /// ([`Target::Operator`]). Open: [`AuthClass::Ui`] asks for no credential.
    Ui,
    /// Served by the proxy: `/v1/update/frontend*`.
    Update,
    /// `/healthz`.
    Health,
    /// `/readyz`.
    Ready,
    /// `/metrics`: the proxy's own series, then those of each member.
    Metrics,
    /// Not in the table: the proxy answers 404 (HTTP) or `UNIMPLEMENTED` (gRPC).
    Unknown,
}

impl Route {
    /// Whether the route speaks gRPC, so a refusal is a gRPC status and not an HTTP one.
    #[must_use]
    pub const fn grpc(self) -> bool {
        match self {
            Self::Child(target) => target.grpc(),
            _ => false,
        }
    }
}

/// Who may call a path (see [`crate::auth`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthClass {
    /// Needs a verified worker or storage certificate.
    Worker,
    /// Follows `buck2_auth`.
    Reapi,
    /// Needs the operator token.
    Operator,
    /// Needs the update credential.
    Update,
    /// Open to a caller with no certificate: the child checks the enrollment token.
    Enroll,
    /// Open: health and readiness.
    Probe,
    /// Open to anyone who can reach the port, for the read-only UI (`/ui`). It is its own
    /// class and never the operator's: nothing under `/v1` takes it, and the proxy admits
    /// only GET and HEAD on it (see [`ui_method_allowed`]).
    Ui,
}

/// How long a stream may sit silent before the proxy closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// Never: Execute and WaitExecution hold open for the whole action.
    Never,
    /// After this long.
    After(Duration),
}

/// The silent-stream bound of every path but the two above (7200 s).
pub const IDLE_DEFAULT: Duration = Duration::from_secs(7200);

const REAPI_PREFIXES: [&str; 6] = [
    "/build.bazel.remote.execution.v2.Execution/",
    "/build.bazel.remote.execution.v2.ActionCache/",
    "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
    "/build.bazel.remote.execution.v2.Capabilities/",
    "/google.bytestream.ByteStream/",
    "/google.longrunning.Operations/",
];
// `ColdStore` is the worker listener's (storage role only, which the child's role table
// enforces; the proxy checks only that the caller is a worker or storage node).
const WORKER_PREFIXES: [&str; 4] = [
    "/kbf.worker.",
    "/kbf.map.v1.Membership/",
    "/kbf.update.",
    "/kbf.enroll.v1.ColdStore/",
];
const ENROLL_PREFIX: &str = "/kbf.enroll.v1.Enroll/";
const UPDATE_PREFIX: &str = "/v1/update/frontend";
const BYTESTREAM_PREFIX: &str = "/google.bytestream.ByteStream/";
const UI_PREFIX: &str = "/ui";
const WAIT_EXECUTION: &str = "/build.bazel.remote.execution.v2.Execution/WaitExecution";

/// The path with any query string removed.
fn bare(path: &str) -> &str {
    path.split_once('?').map_or(path, |(p, _)| p)
}

/// Whether `path` is `prefix` itself or `prefix` followed by `/`, so `/v1/update/frontendx`
/// is not an update path.
fn at_boundary(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `path` (under `/ui`) names a place by plain segments. The UI is open, so the
/// proxy does not let a path walk out of it: a `.` or `..` segment, an empty segment, a
/// backslash and a percent-encoded dot, slash or backslash are refused here, where a client
/// that sends them raw (a browser normalizes them away) gets the 404 of any unknown path.
fn ui_path_is_plain(path: &str) -> bool {
    let rest = path.strip_prefix(UI_PREFIX).unwrap_or_default();
    if rest.contains('\\') {
        return false;
    }
    let lower = rest.to_ascii_lowercase();
    if ["%2e", "%2f", "%5c"].iter().any(|bad| lower.contains(bad)) {
        return false;
    }
    // `/ui` and `/ui/` are the index; below it no segment is empty, `.` or `..`.
    let below = rest.strip_prefix('/').unwrap_or_default();
    below.is_empty()
        || below
            .strip_suffix('/')
            .unwrap_or(below)
            .split('/')
            .all(|s| !matches!(s, "" | "." | ".."))
}

/// Whether the proxy lets `method` through to the UI: `GET` and `HEAD` only. The UI is
/// read-only, and the proxy says so before the child is asked.
#[must_use]
pub fn ui_method_allowed(method: &http::Method) -> bool {
    matches!(*method, http::Method::GET | http::Method::HEAD)
}

/// The route of `path`.
#[must_use]
pub fn route_of(path: &str) -> Route {
    let path = bare(path);
    if at_boundary(path, UI_PREFIX) {
        return if ui_path_is_plain(path) {
            Route::Ui
        } else {
            Route::Unknown
        };
    }
    if REAPI_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Route::Child(Target::Reapi);
    }
    if WORKER_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Route::Child(Target::Worker);
    }
    if path.starts_with(ENROLL_PREFIX) {
        return Route::Child(Target::Enroll);
    }
    if at_boundary(path, UPDATE_PREFIX) {
        return Route::Update;
    }
    if path.starts_with("/v1/") {
        return Route::Child(Target::Operator);
    }
    match path {
        "/healthz" => Route::Health,
        "/readyz" => Route::Ready,
        "/metrics" => Route::Metrics,
        _ => Route::Unknown,
    }
}

/// The auth class of a route; `None` for [`Route::Unknown`], which is refused before auth.
#[must_use]
pub const fn auth_class(route: Route) -> Option<AuthClass> {
    match route {
        Route::Child(Target::Reapi) => Some(AuthClass::Reapi),
        Route::Child(Target::Worker) => Some(AuthClass::Worker),
        Route::Child(Target::Operator) => Some(AuthClass::Operator),
        Route::Child(Target::Enroll) => Some(AuthClass::Enroll),
        Route::Ui => Some(AuthClass::Ui),
        Route::Update => Some(AuthClass::Update),
        Route::Health | Route::Ready => Some(AuthClass::Probe),
        Route::Metrics => Some(AuthClass::Operator),
        Route::Unknown => None,
    }
}

/// The idle timeout of `path`. Execute and WaitExecution are matched by their full method
/// path, so a generic route table needs no per-stream state.
#[must_use]
pub fn idle_of(path: &str) -> Idle {
    match bare(path) {
        "/build.bazel.remote.execution.v2.Execution/Execute" | WAIT_EXECUTION => Idle::Never,
        _ => Idle::After(IDLE_DEFAULT),
    }
}

/// Whether `path` is a ByteStream call, which is never retried.
#[must_use]
pub fn is_bytestream(path: &str) -> bool {
    bare(path).starts_with(BYTESTREAM_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listener_has_its_paths() {
        let reapi = "/build.bazel.remote.execution.v2.Execution/Execute";
        assert_eq!(route_of(reapi), Route::Child(Target::Reapi));
        assert_eq!(
            route_of("/google.bytestream.ByteStream/Write"),
            Route::Child(Target::Reapi)
        );
        for p in [
            "/kbf.worker.v1.Worker/Session",
            "/kbf.map.v1.Membership/Join",
            "/kbf.update.v1.ReleaseService/Fetch",
        ] {
            assert_eq!(route_of(p), Route::Child(Target::Worker), "{p}");
        }
        assert_eq!(route_of("/v1/nodes"), Route::Child(Target::Operator));
        assert_eq!(
            route_of("/v1/update/desired"),
            Route::Child(Target::Operator)
        );
        assert_eq!(route_of("/v1/update"), Route::Child(Target::Operator));
        assert_eq!(
            Target::Reapi.offset() + Target::Worker.offset() + Target::Operator.offset(),
            3
        );
        assert_eq!(Target::Enroll.offset(), 4, "past the admin port at 3");
        for target in [Target::Reapi, Target::Worker, Target::Enroll] {
            assert!(target.grpc(), "{target:?}");
        }
        assert!(!Target::Operator.grpc());
        assert!(route_of(reapi).grpc());
        for route in [
            Route::Ui,
            Route::Update,
            Route::Health,
            Route::Ready,
            Route::Metrics,
            Route::Unknown,
        ] {
            assert!(!route.grpc(), "{route:?}");
        }
    }

    /// Asserts that a call of the `Operations` service (GetOperation reads an operation by
    /// the name Execute returned) is routed to the REAPI listener of the child, with the
    /// REAPI authentication, and not left to the table's unknown paths.
    #[test]
    fn operations_calls_go_to_the_reapi_listener() {
        let get = "/google.longrunning.Operations/GetOperation";
        assert_eq!(route_of(get), Route::Child(Target::Reapi));
        assert_eq!(auth_class(route_of(get)), Some(AuthClass::Reapi));
        assert!(route_of(get).grpc());
        assert_eq!(
            route_of("/google.longrunning.OperationsX/Get"),
            Route::Unknown
        );
    }

    #[test]
    fn enrollment_is_its_own_listener_and_cold_store_is_the_workers() {
        let enroll = route_of("/kbf.enroll.v1.Enroll/Enroll");
        assert_eq!(enroll, Route::Child(Target::Enroll));
        assert_eq!(route_of("/kbf.enroll.v1.Enroll/Renew"), enroll);
        assert_eq!(auth_class(enroll), Some(AuthClass::Enroll));
        let cold = route_of("/kbf.enroll.v1.ColdStore/GetKeys");
        assert_eq!(cold, Route::Child(Target::Worker));
        assert_eq!(auth_class(cold), Some(AuthClass::Worker));
        assert_eq!(route_of("/kbf.enroll.v1.Other/X"), Route::Unknown);
    }

    #[test]
    fn the_update_api_is_the_proxys_own_and_matches_on_a_boundary() {
        for p in [
            "/v1/update/frontend",
            "/v1/update/frontend/abort",
            "/v1/update/frontend?x=1",
        ] {
            assert_eq!(route_of(p), Route::Update, "{p}");
        }
        assert_eq!(
            route_of("/v1/update/frontendx"),
            Route::Child(Target::Operator)
        );
    }

    #[test]
    fn a_path_outside_the_table_is_never_forwarded() {
        for p in [
            "/",
            "/kbf.enroll.v1/Enroll",
            "/grpc.health.v1.Health/Check",
            "/x",
        ] {
            assert_eq!(route_of(p), Route::Unknown, "{p}");
            assert_eq!(auth_class(route_of(p)), None);
        }
    }

    #[test]
    fn auth_classes() {
        assert_eq!(
            auth_class(Route::Child(Target::Reapi)),
            Some(AuthClass::Reapi)
        );
        assert_eq!(
            auth_class(Route::Child(Target::Worker)),
            Some(AuthClass::Worker)
        );
        assert_eq!(
            auth_class(Route::Child(Target::Operator)),
            Some(AuthClass::Operator)
        );
        assert_eq!(auth_class(Route::Update), Some(AuthClass::Update));
        assert_eq!(auth_class(Route::Health), Some(AuthClass::Probe));
        assert_eq!(auth_class(Route::Ready), Some(AuthClass::Probe));
        assert_eq!(auth_class(Route::Metrics), Some(AuthClass::Operator));
    }

    /// `/ui` and what is under it is the UI route, with the UI's own auth class; a look-alike
    /// is not, and nothing under `/v1` is.
    ///
    /// Kills: a prefix match that takes `/uix` or `/v1/ui` for the UI; the UI given the
    /// operator class (or an operator route the UI class); the UI answered by the proxy.
    #[test]
    fn the_ui_is_its_own_route_and_class() {
        for p in [
            "/ui",
            "/ui/",
            "/ui/runs/r1",
            "/ui/static/ui.css",
            "/ui?x=1",
            "/ui/a?x=/../y",
        ] {
            assert_eq!(route_of(p), Route::Ui, "{p}");
            assert_eq!(auth_class(route_of(p)), Some(AuthClass::Ui), "{p}");
        }
        for p in [
            "/uix",
            "/ui.css",
            "/UI",
            "/u",
            "/x/ui",
            "/v1/ui",
            "/v1/ui/runs",
            "/v1/uix",
        ] {
            assert_ne!(route_of(p), Route::Ui, "{p}");
        }
        // Everything under `/v1`, the update API and `/metrics` keeps a credential class.
        for p in [
            "/v1/ui",
            "/v1/status",
            "/v1/nodes",
            "/v1/blobs/x",
            "/v1/update/desired",
        ] {
            assert_eq!(auth_class(route_of(p)), Some(AuthClass::Operator), "{p}");
        }
        assert_eq!(
            auth_class(route_of("/v1/update/frontend")),
            Some(AuthClass::Update)
        );
        assert_eq!(auth_class(route_of("/metrics")), Some(AuthClass::Operator));
    }

    /// A UI path is plain segments. The UI is open, so a way out of it is a path outside the
    /// table.
    ///
    /// Kills: a `..`, `.`, empty segment, backslash or encoded dot or slash let through.
    #[test]
    fn a_ui_path_that_is_not_plain_segments_is_outside_the_table() {
        for p in [
            "/ui/..",
            "/ui/../v1/nodes",
            "/ui/a/../b",
            "/ui/.",
            "/ui/./a",
            "/ui//a",
            "/ui/a//b",
            "/ui/a\\b",
            "/ui/%2e%2e/v1",
            "/ui/%2E/a",
            "/ui/a%2Fb",
            "/ui/a%5cb",
        ] {
            assert_eq!(route_of(p), Route::Unknown, "{p}");
        }
        // Dots inside a name, and a percent-encoding that is neither dot nor slash, are fine.
        for p in [
            "/ui/a.b",
            "/ui/..a",
            "/ui/a..",
            "/ui/runs/r%201",
            "/ui/runs/",
        ] {
            assert_eq!(route_of(p), Route::Ui, "{p}");
        }
    }

    #[test]
    fn the_ui_takes_get_and_head_only() {
        use http::Method;
        for m in [Method::GET, Method::HEAD] {
            assert!(ui_method_allowed(&m), "{m}");
        }
        for m in [
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
            Method::CONNECT,
            Method::TRACE,
        ] {
            assert!(!ui_method_allowed(&m), "{m}");
        }
    }

    #[test]
    fn only_execute_and_wait_execution_are_never_idle() {
        let exec = "/build.bazel.remote.execution.v2.Execution/";
        assert_eq!(idle_of(&format!("{exec}Execute")), Idle::Never);
        assert_eq!(idle_of(&format!("{exec}WaitExecution")), Idle::Never);
        assert_eq!(
            idle_of("/google.bytestream.ByteStream/Write"),
            Idle::After(IDLE_DEFAULT)
        );
        assert_eq!(
            idle_of("/kbf.worker.v1.Worker/Session"),
            Idle::After(IDLE_DEFAULT)
        );
        assert!(is_bytestream("/google.bytestream.ByteStream/Write"));
        assert!(!is_bytestream(&format!("{exec}Execute")));
    }
}

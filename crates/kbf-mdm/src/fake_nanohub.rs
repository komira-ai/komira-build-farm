//! A fake NanoHUB for tests: an HTTP server that checks NanoHUB's basic
//! authentication, records every request, and answers the KMFDDM and NanoMDM paths the
//! gate uses the way NanoHUB documents them.

use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// What the fake has seen, and what it answers.
#[derive(Debug, Default)]
pub struct Hub {
    /// Every authenticated request, as `METHOD /path?query`.
    pub requests: Vec<String>,
    /// The body of each request, in the same order.
    pub bodies: Vec<Vec<u8>>,
    /// `GET /api/v1/ddm/status-values/{id}` answers this.
    pub status_values: serde_json::Value,
    /// `GET /api/v1/ddm/declarations` answers this.
    pub declarations: serde_json::Value,
    /// Answer every request 500.
    pub fail: bool,
    /// Answer DELETEs 404, as for a declaration that is already gone.
    pub missing: bool,
}

pub struct FakeHub {
    pub url: String,
    pub hub: Arc<Mutex<Hub>>,
}

impl FakeHub {
    pub fn requests(&self) -> Vec<String> {
        self.hub.lock().unwrap().requests.clone()
    }

    pub fn body(&self, index: usize) -> String {
        String::from_utf8(self.hub.lock().unwrap().bodies[index].clone()).unwrap()
    }
}

async fn handle(State((key, hub)): State<(String, Arc<Mutex<Hub>>)>, request: Request) -> Response {
    let expected = format!("Basic {}", STANDARD.encode(format!("nanohub:{key}")));
    let authorized = request
        .headers()
        .get("authorization")
        .is_some_and(|v| v.as_bytes() == expected.as_bytes());
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let method = request.method().clone();
    let target = request
        .uri()
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_default();
    let path = request.uri().path().to_owned();
    let body = to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec();
    let mut hub = hub.lock().unwrap();
    if hub.fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    hub.requests.push(format!("{method} {target}"));
    hub.bodies.push(body);
    let json = |v: &serde_json::Value| Response::new(Body::from(serde_json::to_vec(v).unwrap()));
    match (method.as_str(), path.as_str()) {
        ("GET", "/api/v1/ddm/declarations") => json(&hub.declarations),
        ("GET", p) if p.starts_with("/api/v1/ddm/status-values/") => json(&hub.status_values),
        ("PUT", "/api/v1/ddm/declarations") => StatusCode::NOT_MODIFIED.into_response(),
        ("PUT", p) if p.starts_with("/api/v1/nanomdm/enqueue/") => {
            json(&serde_json::json!({"status": {}}))
        }
        ("DELETE", _) if hub.missing => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Starts a fake NanoHUB on a loopback port that accepts `api_key`.
pub async fn start(api_key: &str) -> FakeHub {
    let hub = Arc::new(Mutex::new(Hub::default()));
    let app = axum::Router::new()
        .fallback(handle)
        .with_state((api_key.to_owned(), Arc::clone(&hub)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeHub { url, hub }
}

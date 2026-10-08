//! The API over the real listener: every verb, and what it refuses at the door.

use std::net::SocketAddr;
use std::time::Duration;

use crate::gate::fixture::Fixture;
use crate::request::Purpose;
use crate::tlskit::{Pki, listen, pki, request};

struct Api {
    f: Fixture,
    pki: Pki,
    addr: SocketAddr,
}

impl Api {
    async fn new(name: &str) -> Self {
        let f = Fixture::new(name);
        let pki = pki();
        let addr = listen(super::router(f.gate.clone()), &pki, Duration::from_secs(5)).await;
        Self { f, pki, addr }
    }

    async fn call(&self, method: &str, path: &str, body: &[u8]) -> (u16, serde_json::Value) {
        let (status, text) = request(
            self.addr,
            &self.pki.ca,
            Some(&self.pki.server),
            method,
            path,
            body,
        )
        .await
        .unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    async fn post(&self, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
        self.call("POST", path, &serde_json::to_vec(body).unwrap())
            .await
    }
}

fn signed(f: &mut Fixture, serial: &str, purpose: Purpose) -> serde_json::Value {
    let s = f.signed(serial, purpose);
    serde_json::json!({"message": s.message, "signature": s.signature})
}

#[tokio::test]
async fn reads_answer_with_json() {
    let api = Api::new("api-reads").await;
    let (status, fleet) = api.call("GET", "/v1/macs", b"").await;
    assert_eq!(status, 200);
    assert_eq!(fleet["daily_erase_cap"], 2);
    let (status, mac) = api.call("GET", "/v1/macs/MAC0", b"").await;
    assert_eq!(
        (status, &mac["pool"]),
        (200, &serde_json::json!("mac-arm64"))
    );
    let (status, refused) = api.call("GET", "/v1/macs/NOPE", b"").await;
    assert_eq!(
        (status, &refused["error"]),
        (
            404,
            &serde_json::json!("NOPE is not in the gate's inventory")
        )
    );
}

#[tokio::test]
async fn the_erase_lease_and_grant_verbs() {
    let mut api = Api::new("api-erase").await;
    let held = signed(&mut api.f, "MAC0", Fixture::lease("L1"));
    let (status, body) = api.post("/v1/macs/MAC0/erase", &held).await;
    assert_eq!(
        (status, body),
        (200, serde_json::json!({"outcome": "held", "lease": "L1"}))
    );
    let (status, body) = api
        .post(
            "/v1/macs/MAC0/bring-forward",
            &serde_json::json!({"lease": "L1"}),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, grant) = api
        .post(
            "/v1/macs/MAC0/grant-admin",
            &serde_json::json!({"lease": "L1"}),
        )
        .await;
    assert_eq!(status, 200, "{grant}");
    assert!(
        grant["grant"]
            .as_str()
            .unwrap()
            .starts_with("kbf-grant-v1\n")
    );
    assert!(
        grant["signature"].is_string() && grant["key"].is_string() && grant["erase_at"].is_string()
    );
    let (status, body) = api
        .post(
            "/v1/macs/MAC0/bring-forward",
            &serde_json::json!({"lease": "L1"}),
        )
        .await;
    assert_eq!(
        (status, body),
        (200, serde_json::json!({"outcome": "erased"}))
    );
    let (status, _) = api
        .post(
            "/v1/macs/MAC0/grant-admin",
            &serde_json::json!({"lease": "bad lease"}),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn the_erase_relay_takes_only_the_signed_request() {
    // Catches: an erase verb that takes a target from the server: a body naming a
    // serial, or anything but the message and its signature, is refused.
    let mut api = Api::new("api-erase-relay").await;
    let mut body = signed(&mut api.f, "MAC0", Purpose::EraseNow);
    body["serial"] = "MAC1".into();
    let (status, refused) = api.post("/v1/macs/MAC1/erase", &body).await;
    assert_eq!(status, 400, "{refused}");
    body.as_object_mut().unwrap().remove("serial");
    let (status, _) = api.post("/v1/macs/MAC1/erase", &body).await;
    assert_eq!(status, 403);
    let good = signed(&mut api.f, "MAC0", Purpose::EraseNow);
    let (status, ok) = api.post("/v1/macs/MAC0/erase", &good).await;
    assert_eq!(
        (status, ok),
        (200, serde_json::json!({"outcome": "erased"}))
    );
    assert_eq!(api.f.mdm.erases(), ["erase UDID-0"]);
}

#[tokio::test]
async fn a_profile_is_named_by_digest_and_never_sent() {
    // Catches: accepting profile bytes from the server (M10 mutant).
    let api = Api::new("api-profile").await;
    let digest = "a".repeat(64);
    let (status, _) = api
        .post(
            "/v1/macs/MAC0/profile",
            &serde_json::json!({"digest": digest, "payload": "PHBsaXN0Lz4="}),
        )
        .await;
    assert_eq!(status, 400);
    let (status, refused) = api
        .post(
            "/v1/macs/MAC0/profile",
            &serde_json::json!({"digest": digest}),
        )
        .await;
    assert_eq!(status, 403, "{refused}");
    assert!(api.f.mdm.calls().is_empty());
    // The gate's own copy of an allowlisted profile is what gets installed.
    let profile = b"<plist>gate copy</plist>";
    let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(profile));
    api.f
        .write_file(&format!("profiles/{digest}.mobileconfig"), profile);
    api.f.write_file("allowlist", digest.as_bytes());
    let (status, body) = api
        .post(
            "/v1/macs/MAC0/profile",
            &serde_json::json!({"digest": digest}),
        )
        .await;
    assert_eq!(
        (status, body),
        (200, serde_json::json!({"outcome": "installed"}))
    );
    assert_eq!(
        api.f.mdm.calls(),
        ["profile UDID-0 <plist>gate copy</plist>"]
    );
    let (status, _) = api
        .post(
            "/v1/macs/MAC0/bring-forward",
            &serde_json::json!({"leases": "L"}),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn enforce_and_withdraw() {
    let api = Api::new("api-enforce").await;
    let set = crate::sets::fixture::set("mac-arm64", 12, 10, "27B5");
    let statement = crate::sets::fixture::statement(4, "2030-01-01T00:00:00Z");
    let body = serde_json::json!({"key_statement": statement, "set": set, "deadline": "2027-01-16T02:00:00"});
    let (status, enforced) = api.post("/v1/macs/MAC0/enforce", &body).await;
    assert_eq!(status, 200, "{enforced}");
    assert_eq!(enforced["build"], "27B5");
    let (status, body) = api.call("POST", "/v1/macs/MAC0/withdraw", b"").await;
    assert_eq!(
        (status, body),
        (200, serde_json::json!({"outcome": "withdrawn"}))
    );
    let (status, _) = api.call("POST", "/v1/macs/MAC0/enforce", b"{").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn nothing_outside_the_verbs_is_served() {
    // Catches: a generic pass-through to the MDM (M2.2: the gate has none).
    let api = Api::new("api-closed").await;
    for (method, path) in [
        ("POST", "/v1/macs/MAC0/lock"),
        ("POST", "/v1/mdm/enqueue/UDID-0"),
        ("GET", "/api/v1/ddm/declarations"),
    ] {
        let (status, body) = api.call(method, path, b"{}").await;
        assert_eq!(
            (status, body),
            (404, serde_json::json!({"error": "no such verb"})),
            "{method} {path}"
        );
    }
    let (status, _) = api.call("GET", "/v1/macs/MAC0/erase", b"").await;
    assert_eq!(status, 405);
    let big = vec![b' '; super::MAX_BODY + 1];
    let (status, _) = api.call("POST", "/v1/macs/MAC0/erase", &big).await;
    assert_eq!(status, 413);
}

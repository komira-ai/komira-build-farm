//! The NanoHUB backend (M5.1): KMFDDM's declarations, sets and status values for DDM,
//! and NanoMDM's command queue for the two raw MDM commands the gate sends
//! (`InstallProfile`, `EraseDevice`).
//!
//! NanoHUB serves the KMFDDM API under `/api/v1/ddm/` and NanoMDM's under
//! `/api/v1/nanomdm/`, both behind HTTP basic authentication with the user `nanohub`
//! and the one API key given on its command line (its operations guide). Each Mac gets
//! its own declaration set, `kbf.set.<serial>`, holding only `kbf.` declarations: the
//! status subscription and, while one is outstanding, its enforcement.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use sha2::Digest;

use crate::backend::{BackendError, Device, DeviceStatus, Enforcement, KbfDeclaration, MdmBackend};
use crate::clock::parse_rfc3339;

/// The DDM status items the gate subscribes every Mac to (M3).
pub const STATUS_ITEMS: &[&str] = &[
    "softwareupdate.install-state",
    "softwareupdate.pending-version",
    "softwareupdate.failure-reason",
    "softwareupdate.install-reason",
    "device.operating-system.build-version",
    "device.operating-system.supplemental.build-version",
];

/// The status subscription declaration's identifier.
pub const SUBSCRIPTION: &str = "kbf.status-subscriptions";

/// How long one call to NanoHUB may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A NanoHUB API client.
#[derive(Clone, Debug)]
pub struct NanoHub {
    client: reqwest::Client,
    base: String,
    api_key: String,
}

fn set_name(serial: &str) -> String {
    format!("kbf.set.{serial}")
}

fn err(context: &str, e: impl std::fmt::Display) -> BackendError {
    BackendError(format!("{context}: {e}"))
}

/// Escapes text for a plist `<string>`.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// An MDM command plist: `RequestType`, an optional `Payload` of bytes, and a command
/// UUID.
fn command_plist(request_type: &str, payload: Option<&[u8]>, uuid: &str) -> Vec<u8> {
    let payload = payload.map_or(String::new(), |bytes| {
        format!("<key>Payload</key><data>{}</data>", STANDARD.encode(bytes))
    });
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Command</key><dict><key>RequestType</key><string>{}</string>{payload}</dict>\
         <key>CommandUUID</key><string>{}</string>\
         </dict></plist>\n",
        xml_escape(request_type),
        xml_escape(uuid),
    )
    .into_bytes()
}

/// A unique command UUID, uppercase, in the version 4 layout: the hash of this
/// process, the time and a counter. A command UUID needs to be unique, not secret.
fn command_uuid() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = format!("{}/{nanos}/{count}", std::process::id());
    let mut b: [u8; 16] = sha2::Sha256::digest(seed.as_bytes())[..16]
        .try_into()
        .unwrap_or_default();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode_upper(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[derive(Deserialize)]
struct StatusValue {
    path: String,
    value: String,
    timestamp: Option<String>,
}

impl NanoHub {
    /// A client for the NanoHUB at `base_url` (for example `http://localhost:9004`).
    pub fn new(base_url: &str, api_key: String) -> Self {
        // reqwest fails to build a client only when a TLS backend or the system proxy
        // configuration cannot load; this one has neither (plain HTTP to loopback).
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("a plain-HTTP client builds");
        Self {
            client,
            base: base_url.trim_end_matches('/').to_owned(),
            api_key,
        }
    }

    /// Sends one request; `ok` lists the statuses that mean success besides 2xx.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(&'static str, Vec<u8>)>,
        ok: &[StatusCode],
    ) -> Result<Vec<u8>, BackendError> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.base))
            .basic_auth("nanohub", Some(&self.api_key));
        if let Some((content_type, bytes)) = body {
            request = request.header("content-type", content_type).body(bytes);
        }
        let context = format!("{method} {path}");
        let response = request.send().await.map_err(|e| err(&context, e))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| err(&context, e))?;
        if status.is_success() || ok.contains(&status) {
            Ok(bytes.to_vec())
        } else {
            Err(err(
                &context,
                format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)),
            ))
        }
    }

    /// Stores a declaration, puts it in the Mac's set, and the set on the Mac.
    async fn declare(
        &self,
        device: Device<'_>,
        declaration: serde_json::Value,
    ) -> Result<(), BackendError> {
        let id = declaration["Identifier"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let set = set_name(device.serial);
        let unchanged = [StatusCode::NOT_MODIFIED];
        let body = declaration.to_string().into_bytes();
        self.call(
            Method::PUT,
            "/api/v1/ddm/declarations",
            Some(("application/json", body)),
            &unchanged,
        )
        .await?;
        self.call(
            Method::PUT,
            &format!("/api/v1/ddm/set-declarations/{set}?declaration={id}"),
            None,
            &unchanged,
        )
        .await?;
        self.call(
            Method::PUT,
            &format!(
                "/api/v1/ddm/enrollment-sets/{}?set={set}",
                device.enrollment
            ),
            None,
            &unchanged,
        )
        .await?;
        Ok(())
    }

    async fn enqueue(
        &self,
        device: Device<'_>,
        request_type: &str,
        payload: Option<&[u8]>,
    ) -> Result<(), BackendError> {
        let plist = command_plist(request_type, payload, &command_uuid());
        self.call(
            Method::PUT,
            &format!("/api/v1/nanomdm/enqueue/{}", device.enrollment),
            Some(("application/xml", plist)),
            &[],
        )
        .await
        .map(drop)
    }
}

impl MdmBackend for NanoHub {
    async fn status(&self, device: Device<'_>) -> Result<DeviceStatus, BackendError> {
        let path = format!("/api/v1/ddm/status-values/{}", device.enrollment);
        let body = self.call(Method::GET, &path, None, &[]).await?;
        // A map of enrollment id to values; `null` when the Mac has reported nothing.
        let by_id: Option<std::collections::BTreeMap<String, Vec<StatusValue>>> =
            serde_json::from_slice(&body).map_err(|e| err(&path, e))?;
        let mut status = DeviceStatus::default();
        let values = by_id.unwrap_or_default().remove(device.enrollment);
        for value in values.unwrap_or_default() {
            let seen = value.timestamp.as_deref().and_then(parse_rfc3339);
            status.last_seen = status.last_seen.max(seen);
            let item = value
                .path
                .strip_prefix(".StatusItems.")
                .unwrap_or(&value.path);
            status.items.insert(item.to_owned(), value.value);
        }
        Ok(status)
    }

    async fn subscribe(&self, device: Device<'_>) -> Result<(), BackendError> {
        let items: Vec<_> = STATUS_ITEMS
            .iter()
            .map(|name| serde_json::json!({"Name": name}))
            .collect();
        self.declare(
            device,
            serde_json::json!({
                "Type": "com.apple.configuration.management.status-subscriptions",
                "Identifier": SUBSCRIPTION,
                "Payload": {"StatusItems": items},
            }),
        )
        .await
    }

    async fn enforce(
        &self,
        device: Device<'_>,
        enforcement: &Enforcement,
    ) -> Result<(), BackendError> {
        self.declare(
            device,
            serde_json::json!({
                "Type": "com.apple.configuration.softwareupdate.enforcement.specific",
                "Identifier": KbfDeclaration::enforcement(device.serial).as_str(),
                "Payload": {
                    "TargetOSVersion": enforcement.target_os_version,
                    "TargetBuildVersion": enforcement.target_build_version,
                    "TargetLocalDateTime": enforcement.target_local_date_time,
                },
            }),
        )
        .await
    }

    async fn withdraw(
        &self,
        serial: &str,
        declaration: &KbfDeclaration,
    ) -> Result<(), BackendError> {
        let id = declaration.as_str();
        let gone = [StatusCode::NOT_MODIFIED, StatusCode::NOT_FOUND];
        let set = set_name(serial);
        self.call(
            Method::DELETE,
            &format!("/api/v1/ddm/set-declarations/{set}?declaration={id}"),
            None,
            &gone,
        )
        .await?;
        self.call(
            Method::DELETE,
            &format!("/api/v1/ddm/declarations/{id}"),
            None,
            &gone,
        )
        .await
        .map(drop)
    }

    async fn declarations(&self) -> Result<Vec<String>, BackendError> {
        let path = "/api/v1/ddm/declarations";
        let body = self.call(Method::GET, path, None, &[]).await?;
        // A list of identifiers; `null` (Go's empty slice) when there are none.
        let ids: Option<Vec<String>> = serde_json::from_slice(&body).map_err(|e| err(path, e))?;
        Ok(ids.unwrap_or_default())
    }

    async fn install_profile(
        &self,
        device: Device<'_>,
        profile: &[u8],
    ) -> Result<(), BackendError> {
        self.enqueue(device, "InstallProfile", Some(profile)).await
    }

    async fn erase(&self, device: Device<'_>) -> Result<(), BackendError> {
        self.enqueue(device, "EraseDevice", None).await
    }
}

#[cfg(test)]
#[path = "nanohub_tests.rs"]
mod tests;

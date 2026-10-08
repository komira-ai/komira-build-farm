//! [`GateClient`]: [`MdmGate`] over `kbf.mdmgate.v1`, to a gate that accepts only this
//! server's client certificate (`fleet-updates-security.md` section S5.2).
//!
//! The connection is always mutual TLS: the URL must be `https://`, the gate's
//! certificate must chain to the configured CA and name the configured domain, and
//! the server presents its own certificate. The channel connects on first use and
//! reconnects by itself; every call has a deadline. An answer is checked before it is
//! returned ([`super::inventory`], [`super::enforcement`]): a malformed answer is an
//! error, never a guess.

use std::time::Duration;

use kbf_mdm_api::names::{Serial, Sha256Hex};
use kbf_mdm_api::pb::{
    self, enforce_response, mdm_gate_client::MdmGateClient, profile_response, withdraw_response,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use super::{
    EnforceOrder, Enforcement, GateError, GateFuture, Inventory, MdmGate, enforcement, inventory,
    refused,
};

/// Where the gate is and how to reach it.
#[derive(Clone)]
pub struct GateEndpoint {
    /// `https://<host>:<port>`.
    pub url: String,
    /// The name the gate's certificate must carry.
    pub domain: String,
    /// The CA the gate's certificate chains to (PEM).
    pub ca_pem: Vec<u8>,
    /// This server's client certificate (PEM), which the gate pins.
    pub cert_pem: Vec<u8>,
    /// Its private key (PEM).
    pub key_pem: Vec<u8>,
    /// The longest a connection attempt or a call may take.
    pub timeout: Duration,
}

impl std::fmt::Debug for GateEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateEndpoint")
            .field("url", &self.url)
            .field("domain", &self.domain)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// A connection to `kbf-mdm-gate`.
#[derive(Clone, Debug)]
pub struct GateClient {
    inner: MdmGateClient<Channel>,
}

impl GateClient {
    /// A client for `endpoint`. Nothing is sent until the first call.
    ///
    /// # Errors
    /// [`GateError::Unavailable`] if the URL is not `https://` or the TLS settings are
    /// refused.
    pub fn new(endpoint: &GateEndpoint) -> Result<Self, GateError> {
        if !endpoint.url.starts_with("https://") {
            return Err(GateError::Unavailable(format!(
                "the gate URL {:?} is not https://: the gate speaks only mutual TLS",
                endpoint.url
            )));
        }
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&endpoint.ca_pem))
            .identity(Identity::from_pem(&endpoint.cert_pem, &endpoint.key_pem))
            .domain_name(endpoint.domain.clone());
        let unavailable = |e: tonic::transport::Error| GateError::Unavailable(e.to_string());
        let channel = Endpoint::from_shared(endpoint.url.clone())
            .map_err(unavailable)?
            .tls_config(tls)
            .map_err(unavailable)?
            .connect_timeout(endpoint.timeout)
            .timeout(endpoint.timeout)
            .connect_lazy();
        Ok(Self {
            inner: MdmGateClient::new(channel),
        })
    }
}

fn unavailable(status: tonic::Status) -> GateError {
    GateError::Unavailable(status.to_string())
}

fn no_outcome(verb: &str) -> GateError {
    GateError::Malformed(format!("the {verb} answer has no outcome"))
}

impl MdmGate for GateClient {
    fn status<'a>(&'a self, serials: &'a [Serial]) -> GateFuture<'a, Inventory> {
        let mut client = self.inner.clone();
        let request = pb::StatusRequest {
            serials: serials.iter().map(|s| s.as_str().to_owned()).collect(),
        };
        Box::pin(async move {
            let answer = client.status(request).await.map_err(unavailable)?;
            inventory(answer.into_inner(), serials)
        })
    }

    fn enforce<'a>(&'a self, order: &'a EnforceOrder) -> GateFuture<'a, Enforcement> {
        let mut client = self.inner.clone();
        let request = pb::EnforceRequest {
            serial: order.serial.as_str().to_owned(),
            signed_set: order.signed_set.clone(),
            target_local_date_time: order.by.to_string(),
        };
        Box::pin(async move {
            let answer = client.enforce(request).await.map_err(unavailable)?;
            match answer.into_inner().outcome {
                Some(enforce_response::Outcome::Enforced(e)) => {
                    let e = enforcement(&order.serial, e)?;
                    if e.target_local_date_time != order.by {
                        return Err(GateError::Malformed(format!(
                            "enforcement for {} is due {}, not the {} asked for",
                            order.serial, e.target_local_date_time, order.by
                        )));
                    }
                    Ok(e)
                }
                Some(enforce_response::Outcome::Refused(r)) => Err(refused(&r)),
                None => Err(no_outcome("enforce")),
            }
        })
    }

    fn withdraw<'a>(&'a self, serial: &'a Serial) -> GateFuture<'a, Option<Enforcement>> {
        let mut client = self.inner.clone();
        let request = pb::WithdrawRequest {
            serial: serial.as_str().to_owned(),
        };
        Box::pin(async move {
            let answer = client.withdraw(request).await.map_err(unavailable)?;
            match answer.into_inner().outcome {
                Some(withdraw_response::Outcome::Withdrawn(w)) => {
                    w.removed.map(|e| enforcement(serial, e)).transpose()
                }
                Some(withdraw_response::Outcome::Refused(r)) => Err(refused(&r)),
                None => Err(no_outcome("withdraw")),
            }
        })
    }

    fn install_profile<'a>(
        &'a self,
        serial: &'a Serial,
        digest: &'a Sha256Hex,
    ) -> GateFuture<'a, String> {
        let mut client = self.inner.clone();
        let request = pb::ProfileRequest {
            serial: serial.as_str().to_owned(),
            sha256: digest.as_str().to_owned(),
        };
        Box::pin(async move {
            let answer = client.profile(request).await.map_err(unavailable)?;
            match answer.into_inner().outcome {
                Some(profile_response::Outcome::Installed(p)) if p.identifier.is_empty() => {
                    Err(GateError::Malformed(format!(
                        "the profile answer for {serial} has no identifier"
                    )))
                }
                Some(profile_response::Outcome::Installed(p)) => Ok(p.identifier),
                Some(profile_response::Outcome::Refused(r)) => Err(refused(&r)),
                None => Err(no_outcome("profile")),
            }
        })
    }
}

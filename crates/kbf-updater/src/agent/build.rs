//! The real ports of an agent, and the process around it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_enroll::store::Store;
use kbf_types::HostRole;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::coordinator::{HttpsCoordinator, TlsFiles};
use super::enroll::frontend_url;
use super::flags::Config;
use super::local::{Limits, LocalInstaller, SocketLocal};
use super::renew::{FrontendRenewer, Renewer};
use super::run::{Agent, Stop, World};
use super::status::{Status, host_platform};
use crate::identity::{Identity, identity_of};
use crate::inventory::{Host, Role};
use crate::node_drain::NodeDrain;
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Drain, Health, HealthCheck, PortError, SystemClock};
use crate::version::Version;

/// The ports of an agent that cannot reach what it needs of the frontend: it refuses every
/// update before it fetches anything, and drains nothing. The text says why.
pub struct NoCredential(pub &'static str);

/// Why an agent given no way to the frontend refuses.
pub const NO_CREDENTIAL: &str = "this agent was given neither --frontend (to drain its node with \
     its own certificate) nor an operator API credential, so it cannot drain its node and \
     updates nothing";

impl HealthCheck for NoCredential {
    fn refusal(&self, _host: &Host) -> Option<String> {
        Some(self.0.to_owned())
    }

    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(self.0))
    }

    fn check_held(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(self.0))
    }
}

impl Drain for NoCredential {
    fn drain(&self, _host: &Host, _deadline: Duration) -> Result<(), PortError> {
        Err(PortError::other(self.0))
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Err(PortError::other(self.0))
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        Err(PortError::other(self.0))
    }
}

/// The drain and the health of a host.
type NodePorts = (Arc<dyn Drain>, Arc<dyn HealthCheck>);

/// The drain and the health of the host `identity` names, by the way the agent was given to the
/// frontend:
///
/// - the operator token (`--operator-api`, `--token-file`), the opt-in fallback, if it was given;
/// - else the host's own certificate on the frontend's worker listener (`--frontend`): the drain of
///   the node, and the node's status for a daemon's health or its own storage view for a
///   storage host's ([`NodeHealth::for_updater`]);
/// - else the refusals of [`NoCredential`].
pub(super) fn node_ports(
    cfg: &Config,
    identity: &Identity,
    role: Role,
    local: &Arc<SocketLocal>,
) -> Result<NodePorts, String> {
    let installer = || Arc::new(LocalInstaller(local.clone()));
    if let Some(api) = &cfg.node_api {
        let api = OperatorApi::new(api.addr, api.token_file.clone())?;
        let drain = drain_for(role, &api);
        let health = Arc::new(NodeHealth::with_api(api, installer()));
        return Ok((drain, health));
    }
    let Some(frontend) = &cfg.frontend else {
        return Ok((
            Arc::new(NoCredential(NO_CREDENTIAL)),
            Arc::new(NoCredential(NO_CREDENTIAL)),
        ));
    };
    let api = OperatorApi::as_updater(frontend.clone(), cfg.files.clone(), identity.clone());
    let drain = drain_for(role, &api);
    let health = Arc::new(NodeHealth::for_updater(api, installer()));
    Ok((drain, health))
}

/// Who the updater certificate in `files` says this host is.
///
/// # Errors
/// The file cannot be read, holds no certificate, or the certificate is no updater's.
pub fn identity_of_files(files: &TlsFiles) -> Result<Identity, String> {
    let pem = std::fs::read(&files.cert).map_err(|e| format!("{}: {e}", files.cert.display()))?;
    let leaf = CertificateDer::pem_slice_iter(&pem)
        .next()
        .ok_or_else(|| format!("{} holds no certificate", files.cert.display()))?
        .map_err(|e| format!("{}: {e}", files.cert.display()))?;
    identity_of(leaf.as_ref()).map_err(|e| format!("{}: {e}", files.cert.display()))
}

/// The drain of a host of `role`: the frontend's cordon, drain and uncordon of its node. A
/// storage host may run a daemon too (`storage-and-daemon`), which its restart would cut off
/// from its leases, so it drains that if the frontend knows the node, and has nothing to drain
/// if it does not ([`NodeDrain::if_present`]).
pub fn drain_for(role: Role, api: &OperatorApi) -> Arc<dyn Drain> {
    if role == Role::Storage {
        Arc::new(NodeDrain::if_present(api.clone()))
    } else {
        Arc::new(NodeDrain::with_api(api.clone()))
    }
}

/// The renewer of the certificate in the state directory, if the agent was given one
/// (`--state-dir`, which comes with `--enroll-frontend`).
fn renewer_for(cfg: &Config) -> Result<Option<Arc<dyn Renewer>>, String> {
    let (Some(dir), Some(frontend)) = (&cfg.state_dir, &cfg.enroll_frontend) else {
        return Ok(None);
    };
    let store = Store::open(dir).map_err(|e| e.to_string())?;
    let renewer = FrontendRenewer::new(
        frontend_url(frontend),
        store,
        Arc::new(|| i64::try_from(unix_now()).unwrap_or(i64::MAX)),
        Arc::new(|line| eprintln!("kbf-updater: {line}")),
        jitter(),
    )?;
    Ok(Some(Arc::new(renewer)))
}

/// A random permille from -100 to 100: the spread of the renewal schedule, so that hosts
/// enrolled together do not renew together.
fn jitter() -> i32 {
    use std::hash::BuildHasher as _;
    let draw = std::collections::hash_map::RandomState::new().hash_one(0_u8) % 201;
    i32::try_from(draw).map_or(0, |d| d - 100)
}

/// Unix seconds now.
pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The agent of this host, with the real ports.
///
/// # Errors
/// Why not: no releases for this platform, a certificate that is no updater's or is a
/// frontend's (a frontend is updated by the roll, not yet by the agent), files that do not make
/// a TLS configuration, or an operator API client whose token files are not usable.
pub fn build(cfg: &Config) -> Result<Agent, String> {
    let platform = host_platform().ok_or("kbf has no release for this platform")?;
    let identity = identity_of_files(&cfg.files)?;
    let role = match identity.role {
        HostRole::Frontend => {
            return Err(
                "this certificate is a frontend's: a frontend is updated by the roll, \
                        not by the agent"
                    .to_owned(),
            );
        }
        HostRole::Storage => Role::Storage,
        HostRole::Daemon => Role::Daemon,
        HostRole::MacDaemon => Role::MacDaemon,
    };
    super::coordinator::client_config(&cfg.files).map_err(|e| e.to_string())?;
    let node_id = identity.node_id.to_string();
    let host = Host {
        alias: node_id.clone(),
        address: "localhost".to_owned(),
        role,
        host_key: String::new(),
        node_id: Some(node_id),
    };
    let local = Arc::new(SocketLocal::new(&cfg.helper_socket, Limits::default()));
    let (drain, health) = node_ports(cfg, &identity, host.role, &local)?;
    let renewer = renewer_for(cfg)?;
    let coordinator = Arc::new(
        HttpsCoordinator::new(cfg.coordinator.clone(), cfg.files.clone()).bound_to(identity),
    );
    let world = World {
        coordinator,
        local,
        drain,
        health,
        clock: Arc::new(SystemClock::new()),
        unix_now: Arc::new(unix_now),
        renewer,
        log: Arc::new(|line| eprintln!("kbf-updater: {line}")),
    };
    let unknown = Version::parse("0").map_err(|e| e.to_string())?;
    Ok(Agent::new(
        host,
        cfg.channel.clone(),
        cfg.timing,
        world,
        Status::unknown(platform, unknown),
    ))
}

/// Run `agent` until the handle given to `ready` stops it. The exit code and text of the
/// process.
pub fn run_agent(agent: &Agent, ready: impl FnOnce(Stop)) -> (u8, String) {
    let stop = Stop::default();
    ready(stop.clone());
    agent.run(&stop);
    (0, String::new())
}

/// [`build`], then [`run_agent`].
pub fn run_config(cfg: &Config, ready: impl FnOnce(Stop)) -> (u8, String) {
    match build(cfg) {
        Err(why) => (1, format!("kbf-updater: {why}\n")),
        Ok(agent) => run_agent(&agent, ready),
    }
}

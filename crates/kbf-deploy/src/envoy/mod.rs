//! The `envoy` kit: a client front for a kbf farm, a Tailscale operator Ingress in
//! front of one Envoy that routes REAPI calls by gRPC method to the kbf-servers over
//! TLS to the farm's CA.
//!
//! - `kbf-deploy envoy render --config FILE --out DIR` writes two files into `DIR`
//!   ([`render`]): `manifests.json`, a v1 List of the Kubernetes objects (the Envoy
//!   ConfigMap, Deployment, Service and NetworkPolicy, and the Ingress), and
//!   `envoy.json`, the Envoy bootstrap the ConfigMap carries, the same bytes, for a
//!   reviewer. It writes nothing else and touches no cluster, so it needs no
//!   `--apply`.
//! - `kbf-deploy envoy check --config FILE [--dir DIR]` checks `DIR/manifests.json`
//!   (what `render` wrote there), or without `--dir` the render of `FILE`, for the
//!   properties the REAPI path through the front depends on ([`check`]). The
//!   operator's namespace the NetworkPolicy must admit is the config's
//!   `KBF_TS_OPERATOR_NAMESPACE`. It reads only.
//! - `kbf-deploy envoy apply --config FILE --kubectl PATH --kubeconfig PATH
//!   [--refuse-example FILE] [--apply]` checks the render and what the cluster must
//!   already hold, has the API server validate the render (a server-side dry run), and
//!   only with `--apply` applies it and waits for Envoy and the Ingress's address
//!   ([`apply`]).
//!
//! The same two verbs take `--topology box`: one host's Envoy in front of two kbf
//! instances, `a` and `b`, both listed for ever in a static config
//! ([`box_render`], settings in [`box_config`], `templates/envoy/box.example`).
//! `render` writes `envoy.json` and `envoy.args` (Envoy's hot-restart flags); `check`
//! reads those two ([`box_check`]). The default topology is the front, unchanged.
//!
//! Both files are JSON, which kubectl and Envoy read, printed as [`json`] describes:
//! the Deployment's pod template carries the sha256 of `envoy.json`, so the same
//! settings must always give the same bytes, or every apply would restart Envoy.
//!
//! Exit codes: `render` 0 rendered; `check` 0 every check passed, 1 one `FAIL <check>:
//! <why>` line on stdout per failure; `apply` 0 done (or, without `--apply`, planned), 1
//! a step or the check of the Ingress's address failed (`FAIL ...` on stderr). All three:
//! 2 refused (a bad setting, a file that cannot be read, parsed or written; for `apply`
//! also a render that fails a check, something the cluster lacks, or the server-side
//! dry run refusing), with a `REFUSED:` line on stderr.
//!
//! The settings are a closed set of `KEY=VALUE` lines ([`config::KEYS`],
//! `templates/envoy/config.example`); every key must be set.

pub mod apply;
pub mod box_check;
pub mod box_config;
pub mod box_render;
pub mod check;
mod check_failure;
pub mod config;
pub mod json;
pub mod render;

#[cfg(test)]
mod box_tests;
#[cfg(test)]
mod check_mutant_tests;
#[cfg(test)]
mod check_tests;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use box_config::{BoxConfig, BoxError};
pub use config::{EnvoyConfig, EnvoyError, Server};
pub use render::Rendered;

/// The Kubernetes objects file `render` writes.
pub const MANIFESTS: &str = "manifests.json";
/// The Envoy bootstrap file `render` writes.
pub const ENVOY: &str = "envoy.json";
/// Envoy's hot-restart flags, which the box topology's `render` writes beside [`ENVOY`].
pub const ENVOY_ARGS: &str = "envoy.args";

/// Why `render` or `check` was refused (exit 2).
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error(transparent)]
    Config(#[from] EnvoyError),
    #[error(transparent)]
    BoxConfig(#[from] BoxError),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Write each `(name, text)` into `out`, creating it.
fn write_files(out: &Path, files: [(&str, &String); 2]) -> Result<(), Refusal> {
    std::fs::create_dir_all(out).map_err(|source| Refusal::Write {
        path: out.to_owned(),
        source,
    })?;
    for (name, text) in files {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(|source| Refusal::Write { path, source })?;
    }
    Ok(())
}

/// `envoy render`: render the settings in `config` into `out`, and return the line to
/// print.
pub fn render_files(config: &Path, out: &Path) -> Result<String, Refusal> {
    let c = EnvoyConfig::load(config)?;
    let r = render::render(&c);
    write_files(out, [(MANIFESTS, &r.manifests), (ENVOY, &r.envoy)])?;
    Ok(format!(
        "rendered {MANIFESTS} {ENVOY} into {}",
        out.display()
    ))
}

/// `envoy render --topology box`: the box settings in `config` rendered into `out`.
pub fn render_box_files(config: &Path, out: &Path) -> Result<String, Refusal> {
    let c = BoxConfig::load(config)?;
    let r = box_render::render_box(&c);
    write_files(out, [(ENVOY, &r.envoy), (ENVOY_ARGS, &r.args)])?;
    Ok(format!(
        "rendered {ENVOY} {ENVOY_ARGS} into {}",
        out.display()
    ))
}

/// `envoy check --topology box`: check `dir`'s `envoy.json` and `envoy.args`, or without
/// `dir` the render of `config`, against the config's numbers.
pub fn check_box_files(
    config: &Path,
    dir: Option<&Path>,
) -> Result<Vec<check::CheckFailure>, Refusal> {
    let c = BoxConfig::load(config)?;
    let read = |name: &str| -> Result<(PathBuf, String), Refusal> {
        let path = dir.map_or_else(|| config.to_owned(), |d| d.join(name));
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok((path, text)),
            Err(source) => Err(Refusal::Read { path, source }),
        }
    };
    let (envoy, args) = match dir {
        Some(_) => (read(ENVOY)?, read(ENVOY_ARGS)?.1),
        None => {
            let r = box_render::render_box(&c);
            ((config.to_owned(), r.envoy), r.args)
        }
    };
    let doc = serde_json::from_str(&envoy.1).map_err(|source| Refusal::Parse {
        path: envoy.0,
        source,
    })?;
    Ok(box_check::check_box(&doc, &args, &(&c).into()))
}

/// `envoy check`: check `dir`'s `manifests.json`, or without `dir` the render of
/// `config`, against the config's operator namespace. Every failed check, none when
/// the render passes.
pub fn check_files(config: &Path, dir: Option<&Path>) -> Result<Vec<check::CheckFailure>, Refusal> {
    let c = EnvoyConfig::load(config)?;
    let (path, text) = match dir {
        Some(dir) => {
            let path = dir.join(MANIFESTS);
            match std::fs::read_to_string(&path) {
                Ok(text) => (path, text),
                Err(source) => return Err(Refusal::Read { path, source }),
            }
        }
        None => (config.to_owned(), render::render(&c).manifests),
    };
    let doc = serde_json::from_str(&text).map_err(|source| Refusal::Parse { path, source })?;
    Ok(check::check(&doc, &c.operator_namespace))
}

/// The line `envoy check` prints when every check passes.
pub fn check_passed() -> String {
    format!(
        "{} methods routed, every check passed",
        check::METHODS.len()
    )
}

/// The line `envoy check --topology box` prints when every check passed.
pub fn check_box_passed() -> String {
    format!(
        "{} methods routed, every box check passed",
        check::METHODS.len()
    )
}

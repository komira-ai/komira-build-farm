//! Where a host keeps kbf's releases and kbf-deploy's own state and settings.
//!
//! ```text
//! /usr/local/kbf/releases/<commit>/   one release: kbf, units/, manifest.json
//! /usr/local/kbf/current              link to releases/<commit>, the release that runs
//! /usr/local/kbf/previous             link to releases/<commit>, the one before it
//! /var/lib/kbf-deploy/incoming/       releases being received, not yet verified
//! /var/lib/kbf-deploy/staged/         releases verified and ready to swap in
//! /var/lib/kbf-deploy/deployed/       a record of each release swapped in
//! /var/lib/kbf-deploy/state.json      the deploy state (crate::host::state)
//! /etc/kbf-deploy/policy.toml         what this host accepts
//! /etc/kbf-deploy/pins/               one <commit>.sha256 per pinned release
//! /etc/kbf-deploy/inventory.allow     the hosts a deploy may name
//! /run/kbf-deploy.lock                the lock the state is read and written under
//! ```
//!
//! This layout is Linux-only: `/run`, the lock's directory, does not exist on macOS, so
//! the macOS install path will choose a layout of its own (docs/deploy/host-releases.md).
//!
//! The paths are [`HostPath`]s, joined to a [`kbf_host::Root`] (`/` on a host, a
//! scratch directory in tests). The links name their release relative to their own
//! directory (`releases/<commit>`), so a tree copied or mounted elsewhere still points
//! into itself.

use std::path::{Path, PathBuf};

use kbf_host::path::HostPath;

use super::CommitId;

/// The directory the releases and the `current` and `previous` links are in.
pub const KBF_DIR: &str = "/usr/local/kbf";
pub const RELEASES: &str = "/usr/local/kbf/releases";
pub const CURRENT: &str = "/usr/local/kbf/current";
pub const PREVIOUS: &str = "/usr/local/kbf/previous";
pub const STATE_DIR: &str = "/var/lib/kbf-deploy";
pub const INCOMING: &str = "/var/lib/kbf-deploy/incoming";
pub const STAGED: &str = "/var/lib/kbf-deploy/staged";
pub const DEPLOYED: &str = "/var/lib/kbf-deploy/deployed";
pub const STATE_FILE: &str = "/var/lib/kbf-deploy/state.json";
pub const CONFIG_DIR: &str = "/etc/kbf-deploy";
pub const POLICY: &str = "/etc/kbf-deploy/policy.toml";
pub const PINS: &str = "/etc/kbf-deploy/pins";
pub const INVENTORY_ALLOW: &str = "/etc/kbf-deploy/inventory.allow";
pub const LOCK: &str = "/run/kbf-deploy.lock";

/// The names inside a release directory: the kbf binary, its systemd units and the
/// manifest that lists them.
pub const RELEASE_BINARY: &str = "kbf";
pub const RELEASE_UNITS: &str = "units";
pub const RELEASE_MANIFEST: &str = "manifest.json";

/// One of the constants above as a [`HostPath`].
///
/// # Panics
///
/// When `path` is not a valid host path; every constant here is one (a test checks).
pub fn path(path: &'static str) -> HostPath {
    HostPath::new(path).expect("a layout constant is an absolute host path")
}

/// `/usr/local/kbf/releases/<commit>`.
pub fn release_dir(commit: &CommitId) -> HostPath {
    path_in(RELEASES, commit.as_str())
}

/// `/usr/local/kbf/releases/<commit>/<name>`, for a name of [`RELEASE_BINARY`],
/// [`RELEASE_UNITS`] or [`RELEASE_MANIFEST`].
pub fn release_file(commit: &CommitId, name: &'static str) -> HostPath {
    path_in(&format!("{RELEASES}/{commit}"), name)
}

/// `/etc/kbf-deploy/pins/<commit>.sha256`.
pub fn pin_file(commit: &CommitId) -> HostPath {
    path_in(PINS, &format!("{commit}.sha256"))
}

/// What `current` and `previous` point to for `commit`: `releases/<commit>`.
pub fn release_link(commit: &CommitId) -> PathBuf {
    Path::new("releases").join(commit.as_str())
}

/// `dir/name`; `name` is a commit id or a constant, so it has no `/` or `..`.
fn path_in(dir: &str, name: &str) -> HostPath {
    HostPath::new(&format!("{dir}/{name}")).expect("a layout directory and a plain name")
}

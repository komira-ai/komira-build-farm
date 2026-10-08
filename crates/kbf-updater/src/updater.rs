//! The three verbs (S4.1): `status`, `stage` and `apply`, over the state file and an
//! [`Applier`].
//!
//! Both `stage` and `apply` verify the set in full first: the newest key statement
//! ([`newest_statement`], which is stored as soon as it verifies, so a revocation is
//! remembered even when the set it came with is refused), the signature and role
//! ([`open_set`]) and the checks of S3.1 ([`check`]).
//!
//! `stage` then raises the floor to the set's `min_serial` and copies each artifact the
//! set changes from the artifacts directory, where the daemon left it named by its
//! SHA-256, into the root-only staging directory, hashing the bytes it copies. The
//! artifacts directory is the daemon's, so the copy never follows a symbolic link and
//! takes only a regular file, and the digest is checked on the copy, not the original.
//!
//! `apply` installs only the staged set: it records the apply as in progress, calls the
//! applier, records the set as installed, and reboots if the applier says the step
//! needs it. After a crash mid-apply the in-progress record stays; `status` reports it,
//! `apply` of the same set resumes it, and every other set is refused until it finishes.

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::Refusal;
use crate::apply::Applier;
use crate::set::{NodeView, Pin, Verdict, VerifiedSet, check, open_set};
use crate::signed::{Envelope, PublicKey, newest_statement};
use crate::state::{Held, State};

/// What the updater was provisioned with.
#[derive(Clone, Debug)]
pub struct Config {
    /// The offline root key, the only key pinned on the node.
    pub root_key: PublicKey,
    /// The pool and platform the node accepts.
    pub pin: Pin,
    /// Root-only: the state file and the staging directory.
    pub state_dir: PathBuf,
    /// Where the daemon leaves artifacts, each named by its SHA-256.
    pub artifacts_dir: PathBuf,
}

/// A set as `status` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetRef {
    /// Its serial.
    pub serial: u64,
    /// Its digest.
    pub digest: String,
}

/// The answer to `status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The pinned pool.
    pub pool: String,
    /// The installed set.
    pub installed: Option<SetRef>,
    /// The staged set.
    pub staged: Option<SetRef>,
    /// The digest of an apply that started and has not finished.
    pub in_progress: Option<String>,
    /// The serial floor.
    pub floor: u64,
}

/// What `stage` or `apply` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The set is the installed one; nothing changed.
    AlreadyInstalled,
    /// The set is staged.
    Staged,
    /// The set is installed; `reboot` says whether the node is rebooting.
    Applied {
        /// A reboot was started.
        reboot: bool,
    },
}

/// The updater: its configuration, its state and its applier.
pub struct Updater<A> {
    cfg: Config,
    state: State,
    applier: A,
}

impl<A: Applier> Updater<A> {
    /// Opens the updater over its state file.
    ///
    /// # Errors
    /// The state file exists and cannot be read.
    pub fn open(cfg: Config, applier: A) -> Result<Self, Refusal> {
        let state = State::load(&cfg.state_dir.join("state.json"))?;
        Ok(Updater {
            cfg,
            state,
            applier,
        })
    }

    /// The applier, for tests to inspect.
    pub fn applier(&self) -> &A {
        &self.applier
    }

    /// The applier, for tests to program.
    pub fn applier_mut(&mut self) -> &mut A {
        &mut self.applier
    }

    fn staging(&self) -> PathBuf {
        self.cfg.state_dir.join("staged")
    }

    fn save(&self) -> Result<(), Refusal> {
        self.state.save(&self.cfg.state_dir.join("state.json"))
    }

    /// `status`: what is installed, staged and in progress, and the floor.
    #[must_use]
    pub fn status(&self) -> Status {
        let r = |h: &Held| SetRef {
            serial: h.set.serial,
            digest: h.digest.clone(),
        };
        Status {
            pool: self.cfg.pin.pool.clone(),
            installed: self.state.installed.as_ref().map(r),
            staged: self.state.staged.as_ref().map(r),
            in_progress: self.state.in_progress.clone(),
            floor: self.state.floor,
        }
    }

    /// Every check of S3.1 but the artifact digests, and the newest statement stored.
    fn verify(
        &mut self,
        set: &Envelope,
        statement: Option<&Envelope>,
        now: u64,
    ) -> Result<(VerifiedSet, Verdict), Refusal> {
        let trusted = newest_statement(
            &self.cfg.root_key,
            self.state.statement.as_ref(),
            statement,
            now,
        )?;
        if self.state.statement.as_ref() != Some(&trusted.envelope) {
            self.state.statement = Some(trusted.envelope.clone());
            self.save()?;
        }
        let verified = open_set(set, &trusted.statement)?;
        let node = NodeView {
            pin: &self.cfg.pin,
            installed: self
                .state
                .installed
                .as_ref()
                .map(|h| (&h.set, h.digest.as_str())),
            floor: self.state.floor,
            now,
        };
        let verdict = check(&verified, &node)?;
        if let Some(digest) = &self.state.in_progress
            && *digest != verified.digest
        {
            return Err(Refusal::InProgress(digest.clone()));
        }
        Ok((verified, verdict))
    }

    /// `stage`: verify the set, raise the floor, and copy and hash the artifacts it
    /// changes. Nothing installed changes.
    ///
    /// # Errors
    /// The set fails a check, an artifact is missing or does not match, or the state
    /// cannot be written.
    pub fn stage(
        &mut self,
        set: &Envelope,
        statement: Option<&Envelope>,
        now: u64,
    ) -> Result<Outcome, Refusal> {
        let (v, verdict) = self.verify(set, statement, now)?;
        if verdict == Verdict::AlreadyInstalled {
            return Ok(Outcome::AlreadyInstalled);
        }
        self.state.floor = self.state.floor.max(v.set.min_serial);
        self.state.staged = None;
        self.save()?;
        let staging = self.staging();
        let io = |e: std::io::Error| Refusal::State(format!("{}: {e}", staging.display()));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(io)?;
        }
        fs::create_dir_all(&staging).map_err(io)?;
        let installed = self.state.installed.as_ref().map(|h| &h.set);
        for name in v.set.changes(installed) {
            if let Some(artifact) = v.set.artifacts.get(&name) {
                self.fetch(&name, &artifact.sha256, &staging)?;
            }
        }
        self.state.staged = Some(Held {
            digest: v.digest,
            set: v.set,
        });
        self.save()?;
        Ok(Outcome::Staged)
    }

    /// Copies the artifact named by `sha256` into `staging/name`, hashing what it copies.
    fn fetch(&self, name: &str, sha256: &str, staging: &Path) -> Result<(), Refusal> {
        let source = self.cfg.artifacts_dir.join(sha256);
        let err = |e: std::io::Error| Refusal::Artifact(format!("{name}: {e}"));
        let mut from = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&source)
            .map_err(err)?;
        if !from.metadata().map_err(err)?.is_file() {
            return Err(Refusal::Artifact(format!("{name}: not a regular file")));
        }
        let to = staging.join(name);
        let mut out = fs::File::create(&to).map_err(err)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = from.read(&mut buf).map_err(err)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(err)?;
        }
        out.sync_all().map_err(err)?;
        if hex::encode(hasher.finalize()) != sha256 {
            fs::remove_file(&to).map_err(err)?;
            return Err(Refusal::DigestMismatch(name.to_owned()));
        }
        Ok(())
    }

    /// `apply`: verify the set again, then install it if it is the staged set, and
    /// reboot if the install says so.
    ///
    /// # Errors
    /// The set fails a check, is not the staged set, the applier fails (the apply stays
    /// in progress), or the state cannot be written.
    pub fn apply(
        &mut self,
        set: &Envelope,
        statement: Option<&Envelope>,
        now: u64,
    ) -> Result<Outcome, Refusal> {
        let (v, verdict) = self.verify(set, statement, now)?;
        if verdict == Verdict::AlreadyInstalled {
            return Ok(Outcome::AlreadyInstalled);
        }
        if self.state.staged.as_ref().map(|h| h.digest.as_str()) != Some(v.digest.as_str()) {
            return Err(Refusal::NotStaged(v.digest));
        }
        self.state.in_progress = Some(v.digest.clone());
        self.save()?;
        let staging = self.staging();
        let installed = self
            .applier
            .install(&v.set, &staging)
            .map_err(Refusal::Apply)?;
        self.state.installed = Some(Held {
            digest: v.digest,
            set: v.set,
        });
        self.state.staged = None;
        self.state.in_progress = None;
        self.save()?;
        // The set is installed; a staging directory left behind is cleared by the next
        // stage.
        let _ = fs::remove_dir_all(&staging);
        if installed.reboot {
            self.applier.reboot().map_err(Refusal::Apply)?;
        }
        Ok(Outcome::Applied {
            reboot: installed.reboot,
        })
    }
}

#[cfg(test)]
#[path = "updater_tests.rs"]
mod tests;

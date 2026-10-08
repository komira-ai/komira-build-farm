//! The software set (section 3.2 of `docs/design/fleet-updates.md`) and the checks a set
//! must pass before it is staged or installed (S3.1 of `fleet-updates-security.md`).
//!
//! [`open_set`] verifies the signature and finds the signer's role under the key
//! statement; [`check`] applies the rest against what the node has pinned and installed:
//! pool and platform, expiry, the pool's serial floor, the installed serial, and that the
//! signer's role covers every artifact the set changes. Every refusal is a
//! [`Refusal`] naming the check.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::Refusal;
use crate::signed::{Envelope, KeyStatement, PublicKey, Role, SET_CONTEXT};

/// The artifacts a component key may change: kbf's unprivileged parts. Everything else,
/// `kbf-updater` and `kbf-mac-session` included, needs the platform key (S2.2).
pub const COMPONENT_ARTIFACTS: &[&str] = &["kbf-daemon"];

/// The name [`SoftwareSet::changes`] gives a change of the package snapshot.
pub const SNAPSHOT: &str = "snapshot";

/// A node's platform, spelled as the capability keys spell it (`os`: `linux`, `macos`;
/// `arch`: `x86_64`, `arm64`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    /// The operating system.
    pub os: String,
    /// The CPU architecture.
    pub arch: String,
}

/// What a node's updater was provisioned with: the only pool and platform it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The pool.
    pub pool: String,
    /// The platform.
    pub platform: Platform,
}

/// One artifact a set names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
}

/// A Linux pool's package target (section 8.1): an archive snapshot and the kernel
/// package expected after the upgrade. apt verifies what it downloads against the
/// archive's signature; the set pins which snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AptPin {
    /// The snapshot, `YYYYMMDDTHHMMSSZ`.
    pub snapshot: String,
    /// The kernel package, a Debian package name.
    pub kernel: String,
}

/// A software set: everything one pool's nodes should run, signed as one document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftwareSet {
    /// The pool it is for.
    pub pool: String,
    /// The platform it is for.
    pub platform: Platform,
    /// Monotonic per pool. A rollback is a higher serial naming old artifacts.
    pub serial: u64,
    /// The lowest serial of this pool still allowed; the node keeps the highest it has
    /// seen.
    pub min_serial: u64,
    /// Unix seconds after which it is refused.
    pub expires: u64,
    /// Artifacts by name (`kbf-daemon`, `kbf-updater`, ...).
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
    /// The package snapshot, on Linux pools that pin packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AptPin>,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An artifact name doubles as a file name in the staging directory: lowercase letters,
/// digits, `-`, `_`, `.`, starting with a letter or digit.
fn is_artifact_name(s: &str) -> bool {
    s.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// `YYYYMMDDTHHMMSSZ`, the form `apt --snapshot` takes.
fn is_snapshot_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16
        && b[8] == b'T'
        && b[15] == b'Z'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
}

/// A Debian package name (policy 5.6.1): lowercase alphanumerics and `+ - .`, at least
/// two characters, starting with an alphanumeric. It can never read as an option.
fn is_package_name(s: &str) -> bool {
    s.len() >= 2
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')
        })
}

impl SoftwareSet {
    /// Parses a set's payload and checks its fields' shapes.
    ///
    /// # Errors
    /// Not a set, an artifact name or digest of the wrong shape, a snapshot or kernel
    /// name apt could misread, or `min_serial` above its own serial.
    pub fn parse(payload: &[u8]) -> Result<SoftwareSet, Refusal> {
        let set: SoftwareSet = serde_json::from_slice(payload)
            .map_err(|e| Refusal::Malformed(format!("software set: {e}")))?;
        for (name, artifact) in &set.artifacts {
            if !is_artifact_name(name) || !is_sha256(&artifact.sha256) {
                return Err(Refusal::Malformed(format!("artifact {name:?}")));
            }
        }
        if let Some(pin) = &set.snapshot
            && !(is_snapshot_id(&pin.snapshot) && is_package_name(&pin.kernel))
        {
            return Err(Refusal::Malformed(format!("snapshot {pin:?}")));
        }
        if set.min_serial > set.serial {
            return Err(Refusal::Malformed(format!(
                "min_serial {} above serial {}",
                set.min_serial, set.serial
            )));
        }
        Ok(set)
    }

    /// What this set changes relative to `installed` (everything, with nothing
    /// installed): artifacts it adds, replaces or drops, and [`SNAPSHOT`] when the
    /// package target differs.
    #[must_use]
    pub fn changes(&self, installed: Option<&SoftwareSet>) -> Vec<String> {
        let empty = BTreeMap::new();
        let (old, old_snapshot) =
            installed.map_or((&empty, None), |s| (&s.artifacts, s.snapshot.as_ref()));
        let mut changed: Vec<String> = self
            .artifacts
            .iter()
            .filter(|(name, a)| old.get(*name) != Some(*a))
            .chain(
                old.iter()
                    .filter(|(name, _)| !self.artifacts.contains_key(*name)),
            )
            .map(|(name, _)| name.clone())
            .collect();
        if self.snapshot.as_ref() != old_snapshot {
            changed.push(SNAPSHOT.to_owned());
        }
        changed
    }
}

/// A set whose signature verified, with its signer's role and its digest (the SHA-256
/// of the signed payload, which the rollout record names as `target`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSet {
    /// The set.
    pub set: SoftwareSet,
    /// The key that signed it.
    pub signer: PublicKey,
    /// The role of the key that signed it.
    pub role: Role,
    /// Lowercase hex SHA-256 of the payload.
    pub digest: String,
}

/// Verifies a set's signature and finds its signer under `statement`.
///
/// # Errors
/// A bad signature, a signer the statement does not name ([`Refusal::UnknownKey`]), or a
/// payload [`SoftwareSet::parse`] refuses.
pub fn open_set(envelope: &Envelope, statement: &KeyStatement) -> Result<VerifiedSet, Refusal> {
    let opened = envelope.open(SET_CONTEXT)?;
    let role = statement
        .role_of(&opened.signer)
        .ok_or(Refusal::UnknownKey)?;
    let set = SoftwareSet::parse(&opened.payload)?;
    Ok(VerifiedSet {
        set,
        signer: opened.signer,
        role,
        digest: hex::encode(Sha256::digest(&opened.payload)),
    })
}

/// What the node holds that a set is checked against.
#[derive(Clone, Copy, Debug)]
pub struct NodeView<'a> {
    /// The pinned pool and platform.
    pub pin: &'a Pin,
    /// The installed set and its digest.
    pub installed: Option<(&'a SoftwareSet, &'a str)>,
    /// The highest `min_serial` the node has seen.
    pub floor: u64,
    /// Unix seconds now.
    pub now: u64,
}

/// What to do with a set that passed [`check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Newer than the installed set: stage or install it.
    Install,
    /// It is the installed set: nothing to do.
    AlreadyInstalled,
}

/// The checks of S3.1 after the signature: pool and platform, expiry, the floor, the
/// installed serial, and key coverage.
///
/// # Errors
/// The first check that fails, as its [`Refusal`].
pub fn check(v: &VerifiedSet, node: &NodeView<'_>) -> Result<Verdict, Refusal> {
    let set = &v.set;
    if set.pool != node.pin.pool {
        return Err(Refusal::WrongPool(set.pool.clone()));
    }
    if set.platform != node.pin.platform {
        return Err(Refusal::WrongPlatform(format!(
            "{}/{}",
            set.platform.os, set.platform.arch
        )));
    }
    if node.now >= set.expires {
        return Err(Refusal::Expired(set.serial));
    }
    if set.serial < node.floor {
        return Err(Refusal::BelowFloor {
            serial: set.serial,
            floor: node.floor,
        });
    }
    let installed = match node.installed {
        Some((installed, digest)) if set.serial == installed.serial && v.digest == digest => {
            return Ok(Verdict::AlreadyInstalled);
        }
        Some((installed, _)) if set.serial <= installed.serial => {
            return Err(Refusal::NotNewer {
                serial: set.serial,
                installed: installed.serial,
            });
        }
        Some((installed, _)) => Some(installed),
        None => None,
    };
    if v.role == Role::Component
        && let Some(item) = set
            .changes(installed)
            .into_iter()
            .find(|item| !COMPONENT_ARTIFACTS.contains(&item.as_str()))
    {
        return Err(Refusal::NotCovered(item));
    }
    Ok(Verdict::Install)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{self, PLATFORM_SEED, sha};

    fn pin() -> Pin {
        testkit::pin()
    }

    fn verified(set: &SoftwareSet, role: Role) -> VerifiedSet {
        let payload = serde_json::to_vec(set).unwrap();
        VerifiedSet {
            set: set.clone(),
            signer: [0; 32],
            role,
            digest: hex::encode(Sha256::digest(&payload)),
        }
    }

    fn view<'a>(pin: &'a Pin, installed: Option<(&'a SoftwareSet, &'a str)>) -> NodeView<'a> {
        NodeView {
            pin,
            installed,
            floor: 0,
            now: 1_000,
        }
    }

    /// Catches: a malformed artifact name or digest, an option-shaped snapshot or kernel,
    /// or a set whose floor is above itself accepted.
    #[test]
    fn parse_refuses_misshapen_fields() {
        let good = testkit::set(5);
        assert_eq!(
            SoftwareSet::parse(&serde_json::to_vec(&good).unwrap()),
            Ok(good.clone())
        );
        let mut bad = Vec::new();
        for name in ["", "-x", "Kbf", "a/b", ".hidden"] {
            let mut s = good.clone();
            s.artifacts
                .insert(name.into(), Artifact { sha256: sha("x") });
            bad.push(s);
        }
        for digest in ["00", &"A".repeat(64), &"g".repeat(64)] {
            let mut s = good.clone();
            s.artifacts.insert(
                "kbf-daemon".into(),
                Artifact {
                    sha256: digest.to_string(),
                },
            );
            bad.push(s);
        }
        for (snapshot, kernel) in [
            ("20261001T000000", "linux-image"),
            ("20261001X000000Z", "linux-image"),
            ("2026100aT000000Z", "linux-image"),
            ("20261001T00000aZ", "linux-image"),
            ("20261001T0000000", "linux-image"),
            ("20261001T000000Z", "-o"),
            ("20261001T000000Z", "x"),
            ("20261001T000000Z", "Linux"),
            ("20261001T000000Z", "linux image"),
        ] {
            let mut s = good.clone();
            s.snapshot = Some(AptPin {
                snapshot: snapshot.into(),
                kernel: kernel.into(),
            });
            bad.push(s);
        }
        let mut floor = good.clone();
        floor.min_serial = 6;
        bad.push(floor);
        for s in bad {
            let r = SoftwareSet::parse(&serde_json::to_vec(&s).unwrap());
            assert!(matches!(r, Err(Refusal::Malformed(_))), "{s:?}: {r:?}");
        }
        assert!(matches!(
            SoftwareSet::parse(b"[]"),
            Err(Refusal::Malformed(_))
        ));
        let mut ok = good;
        ok.snapshot = Some(AptPin {
            snapshot: "20261001T000000Z".into(),
            kernel: "linux-image-6.8.0-45-generic".into(),
        });
        ok.artifacts
            .insert("0profile_1.2".into(), Artifact { sha256: sha("p") });
        assert!(SoftwareSet::parse(&serde_json::to_vec(&ok).unwrap()).is_ok());
    }

    /// Catches: a changed, added or dropped artifact or snapshot missed by the change
    /// list, or an unchanged one listed.
    #[test]
    fn changes_list_added_replaced_and_dropped_items() {
        let old = testkit::set(5);
        assert_eq!(old.changes(None), vec!["kbf-daemon", "kbf-updater"]);
        assert!(old.changes(Some(&old)).is_empty());
        let mut new = old.clone();
        new.artifacts
            .insert("kbf-daemon".into(), Artifact { sha256: sha("d2") });
        new.artifacts.remove("kbf-updater");
        new.artifacts
            .insert("profile".into(), Artifact { sha256: sha("p") });
        new.snapshot = Some(AptPin {
            snapshot: "20261001T000000Z".into(),
            kernel: "linux-image".into(),
        });
        assert_eq!(
            new.changes(Some(&old)),
            vec!["kbf-daemon", "profile", "kbf-updater", SNAPSHOT]
        );
    }

    /// Catches: skipping the pool check (a Mac set applies on a Linux test node), the
    /// platform check (an arm64 set on x86-64), or comparing only `os`.
    #[test]
    fn a_set_for_another_pool_or_platform_is_refused() {
        let pin = pin();
        let mut s = testkit::set(5);
        s.pool = "macos-arm64".into();
        assert_eq!(
            check(&verified(&s, Role::Platform), &view(&pin, None)),
            Err(Refusal::WrongPool("macos-arm64".into()))
        );
        let mut s = testkit::set(5);
        s.platform.os = "macos".into();
        assert_eq!(
            check(&verified(&s, Role::Platform), &view(&pin, None)),
            Err(Refusal::WrongPlatform("macos/x86_64".into()))
        );
        let mut s = testkit::set(5);
        s.platform.arch = "arm64".into();
        assert_eq!(
            check(&verified(&s, Role::Platform), &view(&pin, None)),
            Err(Refusal::WrongPlatform("linux/arm64".into()))
        );
    }

    /// Catches: skipping the expiry check, or an off-by-one that accepts a set at its
    /// expiry second.
    #[test]
    fn an_expired_set_is_refused() {
        let pin = pin();
        let s = verified(&testkit::set(5), Role::Platform);
        let mut node = view(&pin, None);
        node.now = s.set.expires - 1;
        assert_eq!(check(&s, &node), Ok(Verdict::Install));
        node.now = s.set.expires;
        assert_eq!(check(&s, &node), Err(Refusal::Expired(5)));
    }

    /// Catches: skipping the floor check, or refusing a set exactly at the floor.
    #[test]
    fn a_set_below_the_floor_is_refused() {
        let pin = pin();
        let s = verified(&testkit::set(5), Role::Platform);
        let mut node = view(&pin, None);
        node.floor = 6;
        assert_eq!(
            check(&s, &node),
            Err(Refusal::BelowFloor {
                serial: 5,
                floor: 6
            })
        );
        node.floor = 5;
        assert_eq!(check(&s, &node), Ok(Verdict::Install));
    }

    /// Catches: a replayed older set accepted, an equal serial with other contents
    /// accepted, or the installed set reinstalled instead of a no-op.
    #[test]
    fn only_a_newer_serial_installs_and_the_installed_set_is_a_no_op() {
        let pin = pin();
        let installed = verified(&testkit::set(5), Role::Platform);
        let at = Some((&installed.set, installed.digest.as_str()));
        assert_eq!(
            check(&installed, &view(&pin, at)),
            Ok(Verdict::AlreadyInstalled)
        );
        let older = verified(&testkit::set(4), Role::Platform);
        assert_eq!(
            check(&older, &view(&pin, at)),
            Err(Refusal::NotNewer {
                serial: 4,
                installed: 5
            })
        );
        let mut twin = testkit::set(5);
        twin.expires += 1;
        let twin = verified(&twin, Role::Platform);
        assert_eq!(
            check(&twin, &view(&pin, at)),
            Err(Refusal::NotNewer {
                serial: 5,
                installed: 5
            })
        );
        let newer = verified(&testkit::set(6), Role::Platform);
        assert_eq!(check(&newer, &view(&pin, at)), Ok(Verdict::Install));
    }

    /// Catches: skipping key coverage (a component-key set changes the updater, the OS
    /// snapshot or another platform artifact), or refusing a component-key set that
    /// changes only `kbf-daemon`.
    #[test]
    fn a_component_key_covers_only_the_daemon() {
        let pin = pin();
        let base = testkit::set(5);
        let base_v = verified(&base, Role::Platform);
        let at = Some((&base_v.set, base_v.digest.as_str()));
        let with = |f: &dyn Fn(&mut SoftwareSet)| {
            let mut s = testkit::set(6);
            f(&mut s);
            s
        };
        let daemon_only = with(&|s| {
            s.artifacts
                .insert("kbf-daemon".into(), Artifact { sha256: sha("d2") });
        });
        assert_eq!(
            check(&verified(&daemon_only, Role::Component), &view(&pin, at)),
            Ok(Verdict::Install)
        );
        let cases: [(SoftwareSet, &str); 3] = [
            (
                with(&|s| {
                    s.artifacts
                        .insert("kbf-updater".into(), Artifact { sha256: sha("u2") });
                }),
                "kbf-updater",
            ),
            (
                with(&|s| {
                    s.artifacts
                        .insert("kbf-mac-session".into(), Artifact { sha256: sha("m") });
                }),
                "kbf-mac-session",
            ),
            (
                with(&|s| {
                    s.snapshot = Some(AptPin {
                        snapshot: "20261001T000000Z".into(),
                        kernel: "linux-image".into(),
                    });
                }),
                SNAPSHOT,
            ),
        ];
        for (s, item) in cases {
            assert_eq!(
                check(&verified(&s, Role::Component), &view(&pin, at)),
                Err(Refusal::NotCovered(item.into()))
            );
            assert_eq!(
                check(&verified(&s, Role::Platform), &view(&pin, at)),
                Ok(Verdict::Install)
            );
        }
        // With nothing installed, every named artifact is a change.
        assert_eq!(
            check(&verified(&base, Role::Component), &view(&pin, None)),
            Err(Refusal::NotCovered("kbf-updater".into()))
        );
    }

    /// Catches: a set signed by a key the statement does not name accepted, or its role
    /// misread.
    #[test]
    fn open_set_finds_the_signers_role() {
        let statement = testkit::statement(1, &[testkit::COMPONENT_SEED]);
        let payload = serde_json::to_vec(&testkit::set(5)).unwrap();
        let platform = Envelope::seal(SET_CONTEXT, &payload, &PLATFORM_SEED);
        let v = open_set(&platform, &statement).unwrap();
        assert_eq!(v.role, Role::Platform);
        assert_eq!(v.signer, crate::signed::public_key(&PLATFORM_SEED));
        assert_eq!(v.digest, hex::encode(Sha256::digest(&payload)));
        let component = Envelope::seal(SET_CONTEXT, &payload, &testkit::COMPONENT_SEED);
        assert_eq!(
            open_set(&component, &statement).unwrap().role,
            Role::Component
        );
        let stranger = Envelope::seal(SET_CONTEXT, &payload, &[7; 32]);
        assert_eq!(open_set(&stranger, &statement), Err(Refusal::UnknownKey));
        let garbage = Envelope::seal(SET_CONTEXT, b"{}", &PLATFORM_SEED);
        assert!(matches!(
            open_set(&garbage, &statement),
            Err(Refusal::Malformed(_))
        ));
    }
}

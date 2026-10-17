//! What the client checks of an offer before it fetches anything. The checks bound what
//! a hostile or confused frontend can make the node download or write; they are not a
//! trust decision (the helper verifies the build attestation).

use std::collections::BTreeSet;

use crate::types::{COMPONENT_NAMES, MAX_ATTESTATION_BYTES, MAX_COMPONENT_BYTES, Offer, is_hex};

/// Why an offer is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("the offer is for platform {offered:?}; this node is {ours:?}")]
    Platform { offered: String, ours: String },
    #[error("commit {0:?} is not 40 lowercase hex digits")]
    Commit(String),
    #[error("the offer names no component")]
    NoComponents,
    #[error("component {0:?} is not one this node installs")]
    UnknownComponent(String),
    #[error("component {0:?} is offered twice")]
    DuplicateComponent(String),
    #[error("component {name}: sha256 {sha256:?} is not 64 lowercase hex digits")]
    Digest { name: String, sha256: String },
    #[error("component {name}: size {size} is not between 1 and {MAX_COMPONENT_BYTES} bytes")]
    Size { name: String, size: u64 },
    #[error(
        "component {name}: attestation of {len} bytes is not between 1 and \
         {MAX_ATTESTATION_BYTES} bytes"
    )]
    Attestation { name: String, len: usize },
}

impl Offer {
    /// Checks the offer against this node's `platform`.
    ///
    /// # Errors
    /// The first thing wrong with it: the platform, the commit, a component's name
    /// (unknown or repeated), digest, size or attestation, or no component at all.
    pub fn check(&self, platform: &str) -> Result<(), Refusal> {
        if self.platform != platform {
            return Err(Refusal::Platform {
                offered: self.platform.clone(),
                ours: platform.to_owned(),
            });
        }
        if !is_hex(&self.commit, 40) {
            return Err(Refusal::Commit(self.commit.clone()));
        }
        if self.components.is_empty() {
            return Err(Refusal::NoComponents);
        }
        let mut names = BTreeSet::new();
        for c in &self.components {
            if !COMPONENT_NAMES.contains(&c.name.as_str()) {
                return Err(Refusal::UnknownComponent(c.name.clone()));
            }
            if !names.insert(c.name.as_str()) {
                return Err(Refusal::DuplicateComponent(c.name.clone()));
            }
            if !is_hex(&c.sha256, 64) {
                return Err(Refusal::Digest {
                    name: c.name.clone(),
                    sha256: c.sha256.clone(),
                });
            }
            if c.size == 0 || c.size > MAX_COMPONENT_BYTES {
                return Err(Refusal::Size {
                    name: c.name.clone(),
                    size: c.size,
                });
            }
            if c.attestation.is_empty() || c.attestation.len() > MAX_ATTESTATION_BYTES {
                return Err(Refusal::Attestation {
                    name: c.name.clone(),
                    len: c.attestation.len(),
                });
            }
        }
        Ok(())
    }

    /// The bytes of every tarball together.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.components.iter().map(|c| c.size).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{PLATFORM, component, offer};
    use crate::types::Component;

    /// Catches an offer check that refuses a good offer (every later test of a refusal
    /// would pass for the wrong reason) and one that refuses the daemon's own component.
    #[test]
    fn a_good_offer_passes() {
        assert_eq!(offer().check(PLATFORM), Ok(()));
        let mut both = offer();
        both.components.push(component("kbf-daemon"));
        assert_eq!(both.check(PLATFORM), Ok(()));
        assert_eq!(both.total_size(), 2000);
    }

    /// Catches (each case on its own) a check that is missing: every refusal must
    /// happen before a byte is fetched.
    #[test]
    fn each_defect_has_its_own_refusal() {
        let cases: Vec<(Offer, Refusal)> = vec![
            (
                Offer {
                    platform: "darwin-arm64".into(),
                    ..offer()
                },
                Refusal::Platform {
                    offered: "darwin-arm64".into(),
                    ours: PLATFORM.into(),
                },
            ),
            (
                Offer {
                    commit: "0123456789ABCDEF0123456789abcdef01234567".into(),
                    ..offer()
                },
                Refusal::Commit("0123456789ABCDEF0123456789abcdef01234567".into()),
            ),
            (
                Offer {
                    commit: "0123456".into(),
                    ..offer()
                },
                Refusal::Commit("0123456".into()),
            ),
            (
                Offer {
                    components: vec![],
                    ..offer()
                },
                Refusal::NoComponents,
            ),
            (
                Offer {
                    components: vec![component("kbf-server")],
                    ..offer()
                },
                Refusal::UnknownComponent("kbf-server".into()),
            ),
            (
                Offer {
                    components: vec![component("kbf"), component("kbf")],
                    ..offer()
                },
                Refusal::DuplicateComponent("kbf".into()),
            ),
            (
                Offer {
                    components: vec![Component {
                        sha256: "AB".repeat(32),
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Digest {
                    name: "kbf".into(),
                    sha256: "AB".repeat(32),
                },
            ),
            (
                Offer {
                    components: vec![Component {
                        sha256: "ab".repeat(31),
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Digest {
                    name: "kbf".into(),
                    sha256: "ab".repeat(31),
                },
            ),
            (
                Offer {
                    components: vec![Component {
                        size: MAX_COMPONENT_BYTES + 1,
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Size {
                    name: "kbf".into(),
                    size: MAX_COMPONENT_BYTES + 1,
                },
            ),
            (
                Offer {
                    components: vec![Component {
                        size: 0,
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Size {
                    name: "kbf".into(),
                    size: 0,
                },
            ),
            (
                Offer {
                    components: vec![Component {
                        attestation: vec![b'x'; MAX_ATTESTATION_BYTES + 1],
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Attestation {
                    name: "kbf".into(),
                    len: MAX_ATTESTATION_BYTES + 1,
                },
            ),
            (
                Offer {
                    components: vec![Component {
                        attestation: vec![],
                        ..component("kbf")
                    }],
                    ..offer()
                },
                Refusal::Attestation {
                    name: "kbf".into(),
                    len: 0,
                },
            ),
        ];
        for (offer, expected) in cases {
            assert_eq!(offer.check(PLATFORM), Err(expected), "{offer:?}");
        }
    }

    /// Catches a size or attestation limit that is off by one at the cap: the cap itself
    /// is allowed, one more is not (the other side is in the table above).
    #[test]
    fn the_caps_are_inclusive() {
        let mut at_cap = offer();
        at_cap.components[0].size = MAX_COMPONENT_BYTES;
        at_cap.components[0].attestation = vec![b'x'; MAX_ATTESTATION_BYTES];
        assert_eq!(at_cap.check(PLATFORM), Ok(()));
    }

    /// The messages carry the numbers an operator needs.
    #[test]
    fn the_reasons_read_plainly() {
        let text = Refusal::Platform {
            offered: "a".into(),
            ours: "b".into(),
        }
        .to_string();
        assert!(text.contains("\"a\""), "{text}");
        assert!(text.contains("\"b\""), "{text}");
    }
}

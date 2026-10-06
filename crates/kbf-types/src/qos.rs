//! QoS levels: how urgent the user says a piece of work is.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A QoS level, chosen by the user (never a platform property: it must not enter the
/// action digest).
///
/// Three levels are built in, from most to least urgent: `interactive` (a human is
/// waiting), `ci`, and `batch` (jobs; the only level that is preempted). A deployment
/// may define further named levels with [`Qos::custom`], each with an urgency that
/// places it among the built-in ones.
///
/// Levels order by urgency: `a > b` means `a` is more urgent and starts first. Two
/// distinct levels never compare equal, because no two levels share an urgency and
/// name together (see [`Qos::custom`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Qos {
    /// A human is waiting. Never preempted.
    Interactive,
    /// Continuous integration and automated clients. Never preempted.
    Ci,
    /// Jobs and background work. Preempted when more urgent work has no room.
    Batch,
    /// A level defined by the deployment; built only through [`Qos::custom`].
    Custom(CustomQos),
}

/// A deployment-defined QoS level. Its fields are private so that every value has
/// passed the checks in [`Qos::custom`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CustomQos {
    name: String,
    urgency: u16,
}

/// Why a QoS level could not be built or parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QosError {
    /// A custom name is empty or has a character outside `a-z`, `0-9`, `-`, `_`.
    #[error("QoS name {0:?} must be non-empty and use only a-z, 0-9, '-' and '_'")]
    InvalidName(String),
    /// A custom level reuses a built-in name.
    #[error("QoS name {0:?} is a built-in level")]
    BuiltinName(String),
    /// A custom level reuses a built-in level's urgency, which would make its order
    /// against that level depend on the name.
    #[error("QoS urgency {0} belongs to a built-in level")]
    BuiltinUrgency(u16),
    /// The name is not a built-in level.
    #[error("unknown QoS level {0:?}")]
    Unknown(String),
}

impl Qos {
    /// Urgency of `interactive`.
    pub const INTERACTIVE_URGENCY: u16 = 300;
    /// Urgency of `ci`.
    pub const CI_URGENCY: u16 = 200;
    /// Urgency of `batch`.
    pub const BATCH_URGENCY: u16 = 100;

    /// Defines a custom level. `urgency` places it: for example 150 sits between
    /// `batch` and `ci`, and 50 below `batch`.
    pub fn custom(name: impl Into<String>, urgency: u16) -> Result<Self, QosError> {
        let name = name.into();
        let valid = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !valid {
            return Err(QosError::InvalidName(name));
        }
        if name.parse::<Self>().is_ok() {
            return Err(QosError::BuiltinName(name));
        }
        if [
            Self::INTERACTIVE_URGENCY,
            Self::CI_URGENCY,
            Self::BATCH_URGENCY,
        ]
        .contains(&urgency)
        {
            return Err(QosError::BuiltinUrgency(urgency));
        }
        Ok(Self::Custom(CustomQos { name, urgency }))
    }

    /// The level's name, as a client sends it.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Interactive => "interactive",
            Self::Ci => "ci",
            Self::Batch => "batch",
            Self::Custom(c) => &c.name,
        }
    }

    /// The level's urgency: higher starts first.
    #[must_use]
    pub fn urgency(&self) -> u16 {
        match self {
            Self::Interactive => Self::INTERACTIVE_URGENCY,
            Self::Ci => Self::CI_URGENCY,
            Self::Batch => Self::BATCH_URGENCY,
            Self::Custom(c) => c.urgency,
        }
    }

    /// Whether work at this level may be preempted: `batch` and every level below it.
    #[must_use]
    pub fn is_preemptible(&self) -> bool {
        self.urgency() <= Self::BATCH_URGENCY
    }
}

impl Ord for Qos {
    fn cmp(&self, other: &Self) -> Ordering {
        // The name breaks ties between custom levels of equal urgency, keeping the
        // order total and consistent with `Eq`.
        self.urgency()
            .cmp(&other.urgency())
            .then_with(|| self.name().cmp(other.name()))
    }
}

impl PartialOrd for Qos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Qos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Parses a built-in level by name. Custom levels come from configuration through
/// [`Qos::custom`], since a name alone does not carry an urgency.
impl FromStr for Qos {
    type Err = QosError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "interactive" => Ok(Self::Interactive),
            "ci" => Ok(Self::Ci),
            "batch" => Ok(Self::Batch),
            _ => Err(QosError::Unknown(s.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: built-in levels in the wrong order (for example the enum's declaration
    /// order, which would put `interactive` lowest), or a custom level ordered by name
    /// instead of urgency.
    #[test]
    fn orders_by_urgency() {
        let nightly = Qos::custom("nightly", 50).unwrap();
        let release = Qos::custom("release", 150).unwrap();
        let mut levels = vec![
            Qos::Batch,
            release.clone(),
            Qos::Interactive,
            nightly.clone(),
            Qos::Ci,
        ];
        levels.sort();
        assert_eq!(
            levels,
            [nightly, Qos::Batch, release, Qos::Ci, Qos::Interactive]
        );
    }

    /// Catches: two different custom levels of equal urgency comparing equal, which
    /// would break `Ord`'s agreement with `Eq` and lose one in a `BTreeSet`.
    #[test]
    fn equal_urgency_is_ordered_by_name() {
        let a = Qos::custom("alpha", 150).unwrap();
        let b = Qos::custom("beta", 150).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Less);
    }

    /// Catches: preemption allowed above `batch` (a human's build killed) or refused
    /// at `batch` (jobs never yield).
    #[test]
    fn only_batch_and_below_are_preemptible() {
        assert!(!Qos::Interactive.is_preemptible());
        assert!(!Qos::Ci.is_preemptible());
        assert!(Qos::Batch.is_preemptible());
        assert!(Qos::custom("nightly", 50).unwrap().is_preemptible());
        assert!(!Qos::custom("release", 150).unwrap().is_preemptible());
    }

    /// Catches: a custom level that shadows a built-in name or urgency, or a name a
    /// header cannot carry cleanly; and a built-in name that does not round-trip.
    #[test]
    fn validates_custom_levels_and_parses_builtins() {
        assert_eq!(
            Qos::custom("ci", 150),
            Err(QosError::BuiltinName("ci".to_owned()))
        );
        assert_eq!(
            Qos::custom("release", 200),
            Err(QosError::BuiltinUrgency(200))
        );
        for bad in ["", "Release", "re lease", "réle"] {
            assert_eq!(
                Qos::custom(bad, 150),
                Err(QosError::InvalidName(bad.to_owned()))
            );
        }
        for level in [Qos::Interactive, Qos::Ci, Qos::Batch] {
            assert_eq!(level.to_string().parse::<Qos>(), Ok(level));
        }
        assert_eq!(
            "nightly".parse::<Qos>(),
            Err(QosError::Unknown("nightly".to_owned()))
        );
    }
}

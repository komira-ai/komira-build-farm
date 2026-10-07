//! Whether an action may use the network, and how the driver keeps it off.
//!
//! An action asks for the network with the platform property `network`: absent,
//! `off` or `none` keeps it off (loopback still works); `on` or `standard` turns it
//! on. Anything else is the client's error.
//!
//! On macOS the driver keeps the network off with `sandbox-exec` and a profile that
//! denies every network operation except loopback and Unix sockets (the profile Bazel's
//! macOS sandbox uses). Where `sandbox-exec` is missing, and on Linux, the network is
//! **not enforced**: the action runs with the node's network. Which of the two a node
//! has is reported as the capability `network_isolation` (`sandbox-exec` or `none`),
//! so nothing about it is hidden.

use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

/// The platform property an action asks for the network with.
pub const PROPERTY: &str = "network";

/// The node report key that says how the network is kept off.
pub const CAPABILITY: &str = "network_isolation";

/// Where macOS keeps `sandbox-exec`.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The sandbox profile for an action without the network: everything allowed but
/// network operations, which are allowed only to loopback and Unix sockets.
pub const NO_NETWORK_PROFILE: &str = "(version 1)\n\
(allow default)\n\
(deny network*)\n\
(allow network-inbound (local ip \"localhost:*\"))\n\
(allow network* (remote ip \"localhost:*\"))\n\
(allow network* (remote unix-socket))\n";

/// What an action asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Off,
    On,
}

/// The action's `network` property, from the Action's platform or, for clients older
/// than REAPI 2.2, the Command's.
///
/// # Errors
/// [`RuntimeError::Invalid`] for a value other than those the module lists.
#[allow(deprecated)]
pub fn network_of(action: &Action, command: &Command) -> Result<Network, RuntimeError> {
    let platform: Option<&Platform> = action.platform.as_ref().or(command.platform.as_ref());
    let value = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == PROPERTY))
        .map(|p| p.value.as_str());
    match value {
        None | Some("off" | "none") => Ok(Network::Off),
        Some("on" | "standard") => Ok(Network::On),
        Some(other) => Err(RuntimeError::Invalid(format!(
            "platform property {PROPERTY}={other:?}: use on, standard, off or none"
        ))),
    }
}

/// How this node keeps the network off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// Through `sandbox-exec` at this path.
    Sandbox(PathBuf),
    /// Not at all: an action that asks for no network still has it.
    None,
}

impl Isolation {
    /// `sandbox-exec` at `program` if it is there, otherwise none.
    #[must_use]
    pub fn detect_at(program: &Path) -> Self {
        if program.is_file() {
            Self::Sandbox(program.to_owned())
        } else {
            Self::None
        }
    }

    /// This node's isolation: `sandbox-exec` on a Mac that has it, none elsewhere.
    #[must_use]
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Self::detect_at(Path::new(SANDBOX_EXEC))
        } else {
            Self::None
        }
    }

    /// The value of the [`CAPABILITY`] entry in the node report.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sandbox(_) => "sandbox-exec",
            Self::None => "none",
        }
    }

    /// The program and arguments that run `program` with `args` under `network`.
    #[must_use]
    pub fn wrap(
        &self,
        network: Network,
        program: PathBuf,
        args: &[String],
    ) -> (PathBuf, Vec<String>) {
        match (self, network) {
            (Self::Sandbox(sandbox), Network::Off) => {
                let mut wrapped = vec![
                    "-p".to_owned(),
                    NO_NETWORK_PROFILE.to_owned(),
                    program.to_string_lossy().into_owned(),
                ];
                wrapped.extend(args.iter().cloned());
                (sandbox.clone(), wrapped)
            }
            _ => (program, args.to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn platform(value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: PROPERTY.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the network on by default, an unknown value taken as on or off
    /// instead of refused, and the Command's deprecated platform overriding the
    /// Action's.
    #[test]
    #[allow(deprecated)]
    fn the_network_is_off_unless_asked_for() {
        let none = (Action::default(), Command::default());
        assert_eq!(network_of(&none.0, &none.1).ok(), Some(Network::Off));
        for (value, want) in [
            ("off", Network::Off),
            ("none", Network::Off),
            ("on", Network::On),
            ("standard", Network::On),
        ] {
            let action = Action {
                platform: platform(value),
                ..Action::default()
            };
            assert_eq!(network_of(&action, &none.1).ok(), Some(want), "{value}");
        }
        let bad = Action {
            platform: platform("yes"),
            ..Action::default()
        };
        assert!(matches!(
            network_of(&bad, &none.1),
            Err(RuntimeError::Invalid(_))
        ));
        let command = Command {
            platform: platform("on"),
            ..Command::default()
        };
        assert_eq!(
            network_of(&Action::default(), &command).ok(),
            Some(Network::On)
        );
        let action = Action {
            platform: platform("off"),
            ..Action::default()
        };
        assert_eq!(network_of(&action, &command).ok(), Some(Network::Off));
    }

    /// Catches: a sandbox applied to an action that asked for the network, or left
    /// off one that did not; the program or its arguments lost or reordered inside
    /// the wrapper; and a node without `sandbox-exec` claiming isolation.
    #[test]
    fn only_an_action_without_network_is_wrapped() {
        let args = vec!["-c".to_owned(), "echo hi".to_owned()];
        let program = PathBuf::from("/bin/sh");
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        assert_eq!(sandbox.name(), "sandbox-exec");
        let (wrapped, wrapped_args) = sandbox.wrap(Network::Off, program.clone(), &args);
        assert_eq!(wrapped, PathBuf::from(SANDBOX_EXEC));
        assert_eq!(
            wrapped_args,
            ["-p", NO_NETWORK_PROFILE, "/bin/sh", "-c", "echo hi"]
        );
        assert_eq!(
            sandbox.wrap(Network::On, program.clone(), &args),
            (program.clone(), args.clone())
        );
        assert_eq!(Isolation::None.name(), "none");
        assert_eq!(
            Isolation::None.wrap(Network::Off, program.clone(), &args),
            (program, args)
        );
        assert_eq!(
            Isolation::detect_at(Path::new("/nonexistent/sandbox-exec")),
            Isolation::None
        );
        let here = std::env::current_exe().expect("test binary");
        assert_eq!(
            Isolation::detect_at(&here),
            Isolation::Sandbox(here.clone())
        );
        if cfg!(target_os = "linux") {
            assert_eq!(Isolation::detect(), Isolation::None);
        }
    }
}

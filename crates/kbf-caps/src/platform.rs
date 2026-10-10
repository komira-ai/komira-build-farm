//! Reading an action's REAPI platform properties as a capability [`Request`].
//!
//! Build tools name the machine an action needs with the property names REAPI
//! standardises, besides kbf's own keys. Buck2's and Bazel's remote platforms commonly
//! send `OSFamily` (Buck2's `remote_execution_properties`, Bazel's `exec_properties`);
//! REAPI names the architecture `ISA`, and some services read `Arch`. They become:
//!
//! | Property | Value (any case) | Request |
//! |---|---|---|
//! | `OSFamily` | `linux` | `os=linux` |
//! | `OSFamily` | `darwin`, `macos`, `macosx`, `osx` | `os=macos` |
//! | `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64` | `arch=x86_64` |
//! | `ISA`, `Arch` | `arm-a64`, `arm64`, `aarch64` | `arch=arm64` |
//! | `ISA`, `Arch` | an ISA level: `x86-64-v3`, `armv8.2-a`, ... | its `arch` and `isa_level` |
//!
//! Every key [`Request::parse`] reads as a capability passes through unchanged, except
//! `gpu`, which the scheduler books rather than matches. Every other property is not
//! a capability and is left to whoever reads it (`kbf-lease`, `container-image`, ...).
//! A report-only key (`vm.slots`, `vm.max_cpus`, `vm.max_mem_gib`, in any case) is
//! refused as [`FromPlatformError::Invalid`]: an action cannot ask for a count of VM
//! slots.
//!
//! Property names are read without regard to ASCII case ([`property_name`]): a client
//! that sends `osfamily=darwin` or `OS=macos` asks for a Mac exactly as one that sends
//! `OSFamily=darwin`, instead of having the property ignored and the action run on any
//! worker. One name in two spellings is one key given twice.
//!
//! A platform no kbf daemon can ever serve is [`FromPlatformError::NeverServed`]:
//! an `os` other than `linux` or `macos` (a daemon refuses to start anywhere else), or
//! an `ISA`/`Arch` naming an architecture kbf does not run on (`x86-32`, `power-isa-le`).

use std::borrow::Cow;
use std::str::FromStr;

use crate::cpu::Arch;
use crate::level::IsaLevel;
use crate::matching::{Request, RequestError, is_own_key};

/// The operating systems a kbf daemon runs on, as its node report names them.
pub const DAEMON_OSES: [&str; 2] = ["linux", "macos"];

/// The property names REAPI standardises that kbf reads, in REAPI's spelling.
pub const REAPI_KEYS: [&str; 3] = ["OSFamily", "ISA", "Arch"];

/// The property that is booked, not matched.
const GPU_KEY: &str = "gpu";

/// The prefix of a `label.<k>` key.
const LABEL_PREFIX: &str = "label.";

/// The name kbf reads platform property `name` by, or `None` for a property kbf does
/// not read (`container-image`, `Pool`, ...).
///
/// Names are matched without regard to ASCII case against the names kbf reads:
/// [`REAPI_KEYS`], and kbf's own keys (the capability keys [`Request::parse`] reads,
/// `gpu` among them, the reserved keys, [`crate::RESERVED_KEYS`], and the report-only
/// keys it refuses, [`crate::REPORT_ONLY_KEYS`]). A name
/// kbf reads comes back in its canonical spelling: `osfamily` is `OSFamily`, `OS` is
/// `os`, `GPU` is `gpu`, and `Label.Pool` is `label.Pool` (a label's own name keeps its
/// case: labels are compared exactly). kbf's own spelling of one of its keys is that
/// key, so `arch` is kbf's `arch`, and every other spelling of it is REAPI's `Arch`.
#[must_use]
pub fn property_name(name: &str) -> Option<Cow<'_, str>> {
    if is_own_key(name) {
        return Some(Cow::Borrowed(name));
    }
    if let Some(reapi) = REAPI_KEYS.iter().find(|k| k.eq_ignore_ascii_case(name)) {
        return Some(Cow::Borrowed(reapi));
    }
    let label = name
        .get(..LABEL_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(LABEL_PREFIX))
        .map(|_| &name[LABEL_PREFIX.len()..]);
    if let Some(label) = label.filter(|label| !label.is_empty()) {
        return Some(Cow::Owned(format!("{LABEL_PREFIX}{label}")));
    }
    let lower = name.to_ascii_lowercase();
    is_own_key(&lower).then_some(Cow::Owned(lower))
}

/// Why a platform is not a request kbf can serve.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FromPlatformError {
    /// The platform is malformed: an unparsable value, or one key given twice (also
    /// through two names, such as `OSFamily` and `os`).
    #[error(transparent)]
    Invalid(#[from] RequestError),
    /// The platform is well formed, but no kbf daemon can ever run it.
    #[error("{0}")]
    NeverServed(String),
}

impl Request {
    /// The request an action's platform `properties` make; see the module table.
    ///
    /// # Errors
    /// [`FromPlatformError::Invalid`] for a malformed platform,
    /// [`FromPlatformError::NeverServed`] for one no kbf daemon can run.
    pub fn from_platform<'p, I>(properties: I) -> Result<Self, FromPlatformError>
    where
        I: IntoIterator<Item = (&'p str, &'p str)>,
    {
        let mut translated: Vec<(Cow<'_, str>, String)> = Vec::new();
        for (sent, value) in properties {
            let Some(key) = property_name(sent) else {
                continue;
            };
            match &*key {
                "OSFamily" => translated.push(("os".into(), os_family(value))),
                "ISA" | "Arch" => {
                    let (arch, level) = isa(value).ok_or_else(|| {
                        FromPlatformError::NeverServed(format!(
                            "platform property {sent}={value:?} names an architecture no kbf \
                             daemon runs on (x86_64 and arm64)"
                        ))
                    })?;
                    translated.push(("arch".into(), arch.name().to_owned()));
                    translated.extend(level.map(|l| ("isa_level".into(), l.name().to_owned())));
                }
                GPU_KEY => {}
                _ => translated.push((key, value.to_owned())),
            }
        }
        let request = Self::parse(translated.iter().map(|(k, v)| (&**k, v.as_str())))?;
        if let Some(os) = request.exact.get("os")
            && !DAEMON_OSES.contains(&os.as_str())
        {
            return Err(FromPlatformError::NeverServed(format!(
                "the platform asks for os {os:?}; kbf daemons run on {DAEMON_OSES:?} only"
            )));
        }
        Ok(request)
    }
}

/// The node report's `os` for an `OSFamily` value. An unknown family is passed on in
/// lower case, and refused as never served.
fn os_family(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "darwin" | "macos" | "macosx" | "osx" => "macos".to_owned(),
        _ => lower,
    }
}

/// The architecture, and the ISA level if the value names one, of an `ISA` value.
fn isa(value: &str) -> Option<(Arch, Option<IsaLevel>)> {
    let lower = value.to_ascii_lowercase();
    let arch = match lower.as_str() {
        "x86-64" | "x86_64" | "amd64" => Some(Arch::X86_64),
        "arm-a64" | "arm64" | "aarch64" => Some(Arch::Arm64),
        _ => None,
    };
    if let Some(arch) = arch {
        return Some((arch, None));
    }
    let level = IsaLevel::from_str(&lower).ok()?;
    let arch = match level {
        IsaLevel::X86_64(_) => Arch::X86_64,
        IsaLevel::Arm64(_) => Arch::Arm64,
    };
    Some((arch, Some(level)))
}

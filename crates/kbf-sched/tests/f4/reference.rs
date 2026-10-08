//! The reference rules the checker holds the scheduler to: the unservable verdict
//! (I10) and resource arithmetic. The arithmetic is written here, not taken from
//! `kbf-types`, so that a defect planted in `Resources` is not shared by the reference.

use kbf_caps::NodeCaps;
use kbf_types::{ControlRecord, Effect, Resources};

/// Whether `request` fits in `room` on every axis.
pub fn fits(room: Resources, request: Resources) -> bool {
    request.cpu_millis <= room.cpu_millis
        && request.memory_bytes <= room.memory_bytes
        && request.gpus <= room.gpus
}

/// `a + b` on every axis.
pub fn add(a: Resources, b: Resources) -> Resources {
    Resources::new(a.cpu_millis + b.cpu_millis, a.memory_bytes + b.memory_bytes)
        .with_gpus(a.gpus + b.gpus)
}

/// `a - b` on every axis, at least zero.
pub fn sub(a: Resources, b: Resources) -> Resources {
    Resources::new(
        a.cpu_millis.saturating_sub(b.cpu_millis),
        a.memory_bytes.saturating_sub(b.memory_bytes),
    )
    .with_gpus(a.gpus.saturating_sub(b.gpus))
}

/// Why no live worker can run a request now, as the reference sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Servable,
    /// No live worker placement may use satisfies it, but a cordoned one does: it
    /// waits for the cordon and is never refused for it.
    Cordoned,
    /// No live worker placement may use; `cordoned_live` says whether any is cordoned.
    NoneLive {
        cordoned_live: bool,
    },
    /// `live` workers placement may use; none satisfies the platform.
    Platform {
        live: usize,
    },
    /// `matching` workers satisfy the platform; none is large enough.
    Size {
        matching: usize,
    },
}

impl Verdict {
    pub fn refusable(self) -> bool {
        !matches!(self, Self::Servable | Self::Cordoned)
    }

    /// Whether `reason` is how the scheduler states this verdict.
    pub fn states(self, reason: &str) -> bool {
        match self {
            Self::Servable => false,
            Self::Cordoned => reason.starts_with("every live worker that can run it is cordoned: "),
            Self::NoneLive { cordoned_live } => {
                reason
                    == if cordoned_live {
                        "every connected worker is cordoned"
                    } else {
                        "no worker is connected"
                    }
            }
            Self::Platform { live } => reason.starts_with(&format!(
                "none of the {live} live worker(s) satisfies the action's platform"
            )),
            Self::Size { matching } => reason.starts_with(&format!(
                "the {matching} live worker(s) that satisfy the action's platform are all smaller"
            )),
        }
    }
}

/// The reference verdict: can some live worker run a request with `resources` and
/// platform `needs`, ignoring what is booked; if not, why (I10). `live` are the live
/// workers placement may use and `cordoned` the live cordoned ones, as their caps and
/// whole capacity.
pub fn verdict(
    live: &[(&NodeCaps, Resources)],
    cordoned: &[(&NodeCaps, Resources)],
    needs: &kbf_caps::Request,
    resources: Resources,
) -> Verdict {
    let uncordoned = if live.is_empty() {
        Verdict::NoneLive {
            cordoned_live: !cordoned.is_empty(),
        }
    } else {
        let matching: Vec<Resources> = live
            .iter()
            .filter(|(caps, _)| needs.matches(caps))
            .map(|(_, capacity)| *capacity)
            .collect();
        if matching.is_empty() {
            Verdict::Platform { live: live.len() }
        } else if matching.iter().any(|c| fits(*c, resources)) {
            return Verdict::Servable;
        } else {
            Verdict::Size {
                matching: matching.len(),
            }
        }
    };
    if cordoned
        .iter()
        .any(|(caps, capacity)| needs.matches(caps) && fits(*capacity, resources))
    {
        Verdict::Cordoned
    } else {
        uncordoned
    }
}

/// Which invariant a difference in effects breaks, by the first effect that differs.
pub fn blame(expected: &[Effect], got: &[Effect]) -> &'static str {
    let first = expected
        .iter()
        .zip(got)
        .find(|(a, b)| a != b)
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((expected.get(got.len()), got.get(expected.len())));
    match first.1.or(first.0) {
        Some(Effect::Start(_)) => "I2",
        Some(Effect::Answer(_) | Effect::Commit(ControlRecord::Result(_))) => "I4/I5",
        Some(Effect::Refuse(_) | Effect::Commit(ControlRecord::Refusal(_))) => "I4/I10",
        Some(Effect::Waiting(_)) => "I10/I13",
        Some(Effect::Commit(_)) => "I11",
        None => "effects",
    }
}

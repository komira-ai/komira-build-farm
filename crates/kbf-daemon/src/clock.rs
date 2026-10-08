//! The clock the fence and the Start window read: one that keeps counting while the
//! machine is suspended (issue #78).
//!
//! Rust's `Instant`, and tokio's, read `CLOCK_MONOTONIC` on Linux and
//! `CLOCK_UPTIME_RAW` on macOS. Neither advances while the machine sleeps, so a node
//! that suspends past T would wake up believing almost no time had passed and keep
//! running leases the scheduler re-placed after G. [`SystemClock`] reads
//! `CLOCK_BOOTTIME` on Linux and `CLOCK_MONOTONIC_RAW` on macOS (`mach_continuous_time`
//! underneath), which count suspended time.
//!
//! Timers still run on tokio's clock, which stops during suspend. So the daemon never
//! sleeps until a deadline: it sleeps at most a short tick, then compares the deadline
//! with [`Clock::now`] (see `daemon`). A [`Clock`] can be injected, so a test can
//! jump it forward as a resume does.

use std::io;
use std::ops::{Add, Sub};
use std::time::Duration;

/// A reading of a [`Clock`]: the time since the clock's own origin. Readings of
/// different clocks are not comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Moment(Duration);

impl Moment {
    /// The reading `since` after the clock's origin.
    #[must_use]
    pub const fn from_origin(since: Duration) -> Self {
        Self(since)
    }

    /// The time since the clock's origin.
    #[must_use]
    pub const fn since_origin(self) -> Duration {
        self.0
    }

    /// How long after `earlier` this reading is; zero if it is not later.
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<Duration> for Moment {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<Duration> for Moment {
    type Output = Self;

    fn sub(self, rhs: Duration) -> Self {
        Self(self.0 - rhs)
    }
}

/// A monotonic clock that counts the time the machine spends suspended.
pub trait Clock: Send + Sync + 'static {
    /// The current reading.
    fn now(&self) -> Moment;
}

/// The host's suspend-counting clock: `CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC_RAW`
/// on macOS.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

/// The `clock_gettime` clock that counts suspend on this OS.
#[cfg(target_os = "linux")]
const SUSPEND_CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
/// On macOS, `CLOCK_MONOTONIC_RAW` is `mach_continuous_time`, which counts sleep;
/// `CLOCK_UPTIME_RAW` (what `Instant` reads) does not.
#[cfg(target_os = "macos")]
const SUSPEND_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("kbf-daemon knows no suspend-counting clock for this OS (issue #78)");

impl Clock for SystemClock {
    fn now(&self) -> Moment {
        // A supported clock id and a valid pointer: clock_gettime cannot fail.
        read(SUSPEND_CLOCK).expect("the suspend-counting clock is readable")
    }
}

/// Reads clock `id` through `clock_gettime`.
fn read(id: libc::clockid_t) -> io::Result<Moment> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call, and
    // clock_gettime writes nothing else.
    let rc = unsafe { libc::clock_gettime(id, &raw mut ts) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // A monotonic clock reads no negative time; tv_nsec is below one second.
    Ok(Moment(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: the fence read on the clock `Instant` reads (`CLOCK_MONOTONIC` on
    /// Linux, `CLOCK_UPTIME_RAW` on macOS), which stops during suspend. A CI machine
    /// has seldom slept, so the two clocks' readings cannot tell them apart there; the
    /// clock id can.
    #[test]
    fn the_system_clock_is_the_one_that_counts_suspend() {
        #[cfg(target_os = "linux")]
        assert_eq!(SUSPEND_CLOCK, libc::CLOCK_BOOTTIME);
        #[cfg(target_os = "macos")]
        assert_eq!(SUSPEND_CLOCK, libc::CLOCK_MONOTONIC_RAW);
    }

    /// The suspend-counting clock read here, independently of `read`.
    fn direct() -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: as in `read`.
        assert_eq!(
            unsafe { libc::clock_gettime(SUSPEND_CLOCK, &raw mut ts) },
            0
        );
        Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64)
    }

    /// Catches: a reading in the wrong unit or from the wrong clock (nanoseconds
    /// dropped or mixed up with seconds, a clock with another origin): the system
    /// clock must read between two direct reads of the suspend-counting clock.
    #[test]
    fn the_system_clock_reads_the_suspend_counting_clock() {
        let before = direct();
        let now = SystemClock.now().since_origin();
        let after = direct();
        assert!(before <= now, "{before:?} {now:?}");
        assert!(now <= after, "{now:?} {after:?}");
    }

    /// Catches: a failed clock_gettime read as time zero, which would never fence.
    #[test]
    fn an_unknown_clock_is_an_error() {
        assert!(read(9999).is_err());
    }

    /// Catches: arithmetic that does not keep a reading's origin.
    #[test]
    fn moments_add_subtract_and_compare() {
        let t = Moment::from_origin(Duration::from_secs(10));
        let s = Duration::from_secs(3);
        assert_eq!((t + s).since_origin(), Duration::from_secs(13));
        assert_eq!((t - s).since_origin(), Duration::from_secs(7));
        assert_eq!((t + s).saturating_duration_since(t), s);
        assert_eq!(t.saturating_duration_since(t + s), Duration::ZERO);
        assert!(t < t + s);
    }
}

//! The most HTTP/2 streams one connection may have open on each listener: the
//! `--reapi-max-streams` and `--worker-max-streams` flags and the limits they become.
//!
//! The two listeners differ. A REAPI client (buck2) holds one `Execute` stream open per
//! outstanding action, idle almost the whole time, so its backlog must not wait in the
//! client behind a small cap. A daemon holds one persistent session stream and a few
//! short, busy transfer streams. The flow-control windows follow the limit
//! ([`flow_windows`]): the connection's window always holds every stream's window.

use kbf_front::{DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS, STREAM_WINDOW_BYTES};

/// The largest flow-control window HTTP/2 allows (RFC 9113, section 6.9.1), which
/// bounds the connection window.
const MAX_WINDOW: u32 = i32::MAX as u32;

/// The most streams a listener may allow: with more, a stream's window would be under
/// 64 KiB, HTTP/2's default window.
pub const MAX_STREAMS: u32 = MAX_WINDOW / 65_536;

/// The flow-control windows of one connection to a listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowWindows {
    /// The streams the connection may have open: the limit given, clamped to 1 and
    /// [`MAX_STREAMS`]. The listener's `max_concurrent_streams` is this, so the limit and
    /// the windows cannot disagree.
    pub streams: u32,
    /// The window of each stream.
    pub stream: u32,
    /// The window of the connection.
    pub connection: u32,
}

/// The windows of a listener that allows `max_streams` streams per connection (at
/// least 1, at most [`MAX_STREAMS`]): `connection == max_streams * stream`, and the
/// stream window is [`STREAM_WINDOW_BYTES`] unless that would put the connection past
/// HTTP/2's largest window, which is so at about 2000 streams.
///
/// A `Write` waiting for admission is not read, so its stream's window stays full of
/// unread data, which counts against the connection's window until the server reads it.
/// The admitted `Write`s, which must finish before anyone else is admitted, need
/// that window to move. With the connection window at least every stream's window, the
/// streams together can never hold all of it unread, whatever the number of
/// waiting `Write`s: this is the invariant (`docs/design/write-path.md`, section 3.3),
/// and why one limit's windows are not another's.
pub fn flow_windows(max_streams: u32) -> FlowWindows {
    let streams = max_streams.clamp(1, MAX_STREAMS);
    let stream = STREAM_WINDOW_BYTES.min(MAX_WINDOW / streams);
    FlowWindows {
        streams,
        stream,
        connection: streams * stream,
    }
}

/// The stream limits of the two listeners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// The most streams one connection to the REAPI listener may have open.
    pub reapi: u32,
    /// The most streams one connection to the worker listener may have open.
    pub worker: u32,
}

impl StreamLimits {
    /// The defaults of `--reapi-max-streams` and `--worker-max-streams`.
    pub const DEFAULT: Self = Self {
        reapi: DEFAULT_REAPI_STREAMS,
        worker: DEFAULT_WORKER_STREAMS,
    };
}

/// The stream-limit flags, part of the server's [`Args`](crate::config::Args).
#[derive(Clone, Copy, Debug, clap::Args)]
pub struct StreamArgs {
    /// The most HTTP/2 streams one connection to the REAPI listener may have open.
    /// buck2 keeps one Execute stream open per outstanding action, idle until the
    /// action finishes, so this bounds how large a backlog one buck2 invocation can
    /// show the farm. The connection's flow-control window is this many streams' windows;
    /// above about 2000 streams each stream's window is smaller than 1 MiB so that the
    /// product stays under HTTP/2's largest window. At least 1, at most 32767.
    #[arg(
        long,
        default_value_t = DEFAULT_REAPI_STREAMS,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_STREAMS))
    )]
    pub reapi_max_streams: u32,
    /// The most HTTP/2 streams one connection to the worker listener may have open: a
    /// daemon's persistent session stream and its short transfer streams. At least 1, at
    /// most 32767.
    #[arg(
        long,
        default_value_t = DEFAULT_WORKER_STREAMS,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_STREAMS))
    )]
    pub worker_max_streams: u32,
}

impl StreamArgs {
    /// The limits these flags name.
    pub fn limits(&self) -> StreamLimits {
        StreamLimits {
            reapi: self.reapi_max_streams,
            worker: self.worker_max_streams,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use kbf_front::STREAM_WINDOW_BYTES;

    use super::{FlowWindows, MAX_STREAMS, StreamLimits, flow_windows};
    use crate::config::Args;

    /// Catches: a connection window under the streams' windows together (the deadlock of
    /// `docs/design/write-path.md`: waiting Writes' unread data takes the window and
    /// the admitted ones starve) at any limit, a window past HTTP/2's largest, a
    /// stream window under 64 KiB, or the stream window cut where 1 MiB fits (the
    /// worker's 200 streams stay at 1 MiB and 200 MiB). Mutant: a fixed connection
    /// window.
    #[test]
    fn the_connection_window_holds_every_streams_window() {
        for streams in [1, 3, 200, 2047, 2048, 10_000, 32_767, MAX_STREAMS] {
            let w = flow_windows(streams);
            assert!(
                u64::from(w.connection) >= u64::from(streams) * u64::from(w.stream),
                "{streams}: {w:?}"
            );
            assert!(w.connection <= i32::MAX as u32, "{streams}: {w:?}");
            assert!(
                (65_536..=STREAM_WINDOW_BYTES).contains(&w.stream),
                "{streams}: {w:?}"
            );
        }
        assert_eq!(
            flow_windows(200),
            FlowWindows {
                streams: 200,
                stream: 1 << 20,
                connection: 200 << 20
            }
        );
        assert_eq!(flow_windows(2047).stream, 1 << 20);
        assert!(flow_windows(2048).stream < 1 << 20);
    }

    /// Catches: a limit outside 1..=`MAX_STREAMS` (a `Listeners` built in code, not
    /// from the flags) reaching the transport unclamped while the windows are clamped, so
    /// the connection window no longer holds every stream's window. Mutant: `streams`
    /// holding the raw limit.
    #[test]
    fn a_limit_out_of_range_is_clamped_with_its_windows() {
        let none = flow_windows(0);
        assert_eq!(none.streams, 1);
        let many = flow_windows(u32::MAX);
        assert_eq!(many.streams, MAX_STREAMS);
        for w in [none, many] {
            assert!(u64::from(w.connection) >= u64::from(w.streams) * u64::from(w.stream));
        }
    }

    /// Catches: a default other than 10000 for the REAPI listener or 200 for the
    /// worker listener; a flag that does not reach `Listeners` (mutant: `limits()`
    /// swapping the two, or returning the defaults); the flags crossed.
    #[test]
    fn the_flags_reach_the_listeners() {
        let defaults = Args::parse_from(["kbf-server"])
            .listeners()
            .expect("listeners");
        assert_eq!(defaults.stream_limits, StreamLimits::DEFAULT);
        assert_eq!(defaults.stream_limits.reapi, 10_000);
        assert_eq!(defaults.stream_limits.worker, 200);
        let set = Args::parse_from([
            "kbf-server",
            "--reapi-max-streams=30000",
            "--worker-max-streams=17",
        ])
        .listeners()
        .expect("listeners");
        assert_eq!(
            set.stream_limits,
            StreamLimits {
                reapi: 30_000,
                worker: 17
            }
        );
        let one = Args::parse_from(["kbf-server", "--worker-max-streams=3"])
            .listeners()
            .expect("listeners");
        assert_eq!(
            one.stream_limits.reapi, 10_000,
            "an unset flag keeps its default"
        );
        assert_eq!(one.stream_limits.worker, 3);
    }

    /// Catches: a limit of 0 (no stream could ever open) or a value that is not a
    /// number accepted, on either flag.
    #[test]
    fn a_limit_below_one_is_refused() {
        for refused in [
            "--reapi-max-streams=0",
            "--worker-max-streams=0",
            "--reapi-max-streams=-1",
            "--worker-max-streams=many",
            "--reapi-max-streams=4294967296",
            "--reapi-max-streams=32768",
            "--worker-max-streams=32768",
        ] {
            assert!(
                Args::try_parse_from(["kbf-server", refused]).is_err(),
                "{refused} accepted"
            );
        }
    }
}

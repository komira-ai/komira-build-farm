//! The flags of the frontend's drain sequence ([`crate::sequence`]): `--drain-lb-wait` and
//! `--drain-deadline`, and the [`DrainTiming`] they make.
//!
//! Both take whole seconds, as `--durable-wait` and `--ram-drain-deadline` do. They are the
//! frontend's own and not `kbf-server`'s: the server drains at once, with no other
//! frontend to hand its work to. Neither needs another flag, since the config template
//! writes every flag with a default as a live line.

use std::time::Duration;

use clap::{Arg, ArgMatches};

use crate::sequence::DrainTiming;

const LB_WAIT: &str = "drain_lb_wait";
const DEADLINE: &str = "drain_deadline";

/// The defaults, in seconds: [`DrainTiming::default`]'s.
const DEFAULT_LB_WAIT: &str = "10";
const DEFAULT_DEADLINE: &str = "900";

/// The two flags.
pub(super) fn args() -> [Arg; 2] {
    [
        Arg::new(LB_WAIT)
            .long("drain-lb-wait")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64))
            .default_value(DEFAULT_LB_WAIT)
            .help(
                "How long, in seconds, a drain (a stop signal or `POST /v1/frontend/drain`) \
                 keeps serving everything after `/readyz` has begun to answer 503, so that \
                 the load balancer's health check sees it before any call is refused: at \
                 least its check interval times its unhealthy threshold, and the time its \
                 routes take to change. Then new Execute calls are refused UNAVAILABLE and \
                 every listener is sent GOAWAY. 0 goes on at once. The container's stop \
                 timeout must exceed this and --drain-deadline.",
            ),
        Arg::new(DEADLINE)
            .long("drain-deadline")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(DEFAULT_DEADLINE)
            .help(
                "The longest, in seconds, a drain waits for the open executions to finish \
                 after it has refused new ones. Those still open then are ended UNAVAILABLE, \
                 the daemons' worker streams end and the process exits. Buck2 does not \
                 recover a stream that breaks mid-action, so an action that runs longer than \
                 this fails: update in a quiet window, or set it above the longest action. \
                 At least 1.",
            ),
    ]
}

/// The setting the flags in `m` make.
pub(super) fn from_matches(m: &ArgMatches) -> DrainTiming {
    let seconds = |id: &str| {
        let given = m.get_one::<u64>(id);
        Duration::from_secs(*given.expect("the flag has a default"))
    };
    DrainTiming {
        lb_wait: seconds(LB_WAIT),
        deadline: seconds(DEADLINE),
    }
}

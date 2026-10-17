//! The flag sets of the modes that take part of `kbf-server`'s own ([`crate::storage_node`],
//! [`crate::frontend`]): a mode names the flags of [`Args`] it takes, and every other
//! flag of [`Args`] is shown to nobody and refused by name. The refused set is computed
//! from [`Args`], so a flag added there that no mode lists is refused by each with no edit.

use std::collections::BTreeSet;

use clap::parser::ValueSource;
use clap::{ArgMatches, Command, CommandFactory as _};

use crate::Args;

/// The ids of the thread flags, which `kbf-server` flattens into [`Args`]; every mode
/// takes them.
fn thread_flags() -> BTreeSet<clap::Id> {
    let pool = <kbf_threads::PoolArgs as clap::Args>::augment_args(Command::new("pool"));
    pool.get_arguments().map(|a| a.get_id().clone()).collect()
}

/// Whether a mode that lists `allowed` takes `id` of [`Args`].
fn takes(id: &clap::Id, allowed: &[&str], threads: &BTreeSet<clap::Id>) -> bool {
    allowed.contains(&id.as_str()) || threads.contains(id)
}

/// The command `name`: every flag of [`Args`], those in `allowed` (and the thread flags)
/// shown and the rest hidden so [`refusal`] can name them.
pub(crate) fn command(name: &'static str, about: &'static str, allowed: &[&str]) -> Command {
    let threads = thread_flags();
    let mut cmd = Command::new(name).about(about);
    for arg in Args::command().get_arguments() {
        let shown = takes(arg.get_id(), allowed, &threads);
        cmd = cmd.arg(arg.clone().hide(!shown));
    }
    cmd
}

/// The first flag of [`Args`] outside `allowed` that `m` holds from the command line,
/// by its long name.
pub(crate) fn refused(m: &ArgMatches, allowed: &[&str]) -> Option<String> {
    let threads = thread_flags();
    Args::command().get_arguments().find_map(|arg| {
        let id = arg.get_id();
        (!takes(id, allowed, &threads)
            && m.value_source(id.as_str()) == Some(ValueSource::CommandLine))
        .then(|| arg.get_long().map_or(id.as_str(), |long| long).to_owned())
    })
}

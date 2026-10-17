//! Programs for kbf's tests to run in place of shell scripts.
//!
//! - [`act`]: `kbf-act STEP [; STEP]...`, a fixed set of verbs (exit, sleep, echo,
//!   append, env, pwd, id, kill-self, spawn-child, run, setsid, alloc, upper,
//!   rename, symlink, chmod, wait-for, sleep-once, mkdir, cat, mtime, pgid, leads, write-lines,
//!   copy, remove, mkfifo, truncate, zeros, ignore-signal, test, points-to, holds,
//!   inode-links) that a test puts in an action's or a child's argv where it would
//!   have written `sh -c`.
//! - [`fake`]: the same binary run through a link named `xcrun`, `xcodebuild` or
//!   `sandbox-exec` answers as that tool, from a rules file the test writes.
//! - [`m2`]: the verbs of `kbf-m2-act`, the program every action of the M2 sample
//!   project runs (the M2 image has no shell).
//!
//! A test runs [`act`] and the fakes in one of two ways:
//! - through a bin of its own package, because Cargo sets `CARGO_BIN_EXE_<name>` only
//!   for the bins of the package under test, and only in its integration tests. The
//!   bin is three lines that call [`act::main`], declared with `test = false` under a
//!   name of its own (two packages with a bin of the same name collide in `target/`).
//!   That package then has this crate as a normal dependency (Cargo has no per-bin
//!   dependencies);
//! - through the test binary itself, which expands [`act_test_hook!`] and is run
//!   through a link that [`act::link_self`] makes, named [`act::NAME`] or for a fake's
//!   tool. This crate then stays a dev-dependency, out of the dependencies (and the
//!   SBOM) of a binary the package ships, and unit tests can run it too.
//!
//! Every program here exits 0 when it did what it was asked, 1 when a step failed and
//! 2 on a usage error, each failure with one line on stderr, except where a verb sets
//! the status (`exit`, or the fakes' answers).

use std::io::Write;

pub mod act;
pub mod fake;
pub mod m2;

/// Why a program here stopped short.
#[derive(Debug, PartialEq, Eq)]
enum Error {
    /// The arguments do not say a known thing to do: exit status 2.
    Usage(String),
    /// A step was asked for properly but failed: exit status 1.
    Failed(String),
}

/// Exit status `code`, or the status `error` calls for with its reason on stderr,
/// after `program: `.
fn status(program: &str, result: Result<u8, Error>) -> u8 {
    let (code, why) = match result {
        Ok(code) => return code,
        Err(Error::Usage(why)) => (2, why),
        Err(Error::Failed(why)) => (1, why),
    };
    eprintln!("{program}: {why}");
    code
}

/// Makes the test binary it is expanded in answer as `kbf-act`, or as a fake, when it
/// is run through a link named for one (see [`act::link_self`]). It runs
/// [`act::hook`] before the test harness reads its arguments, from the platform's list
/// of start-up functions. Expand it once per test binary.
#[macro_export]
macro_rules! act_test_hook {
    () => {
        const _: () = {
            extern "C" fn kbf_act_hook() {
                $crate::act::hook();
            }
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(
                target_vendor = "apple",
                unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")
            )]
            static KBF_ACT_HOOK: extern "C" fn() = kbf_act_hook;
        };
    };
}

/// The process's arguments after argv0, refusing one that is not UTF-8, and argv0.
fn arguments() -> (String, Result<Vec<String>, Error>) {
    let mut all = std::env::args_os();
    let argv0 = all
        .next()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let rest = all
        .map(|a| {
            a.into_string()
                .map_err(|a| Error::Usage(format!("{a:?} is not UTF-8")))
        })
        .collect();
    (argv0, rest)
}

/// Maps an error of `what` to [`Error::Failed`], naming `what`. One mapping for every
/// failure keeps the failure paths few.
fn failed<E: std::fmt::Display>(what: impl std::fmt::Display) -> impl FnOnce(E) -> Error {
    move |e| Error::Failed(format!("{what}: {e}"))
}

/// Writes `line` and a newline to `to`.
fn write_line(mut to: impl Write, line: &str) -> Result<(), Error> {
    writeln!(to, "{line}")
        .and_then(|()| to.flush())
        .map_err(failed("write"))
}

/// Reads the file at `path`.
fn read(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(failed(format!("read {path}")))
}

/// Creates (or truncates) the file at `path` and writes `bytes` to it.
fn create(path: &str, bytes: &[u8]) -> Result<(), Error> {
    let mut file = std::fs::File::create(path).map_err(failed(format!("create {path}")))?;
    file.write_all(bytes)
        .map_err(failed(format!("write {path}")))
}

/// `text` as a whole number.
fn number(text: &str) -> Result<u64, Error> {
    text.parse()
        .map_err(|_| Error::Usage(format!("{text:?} is not a whole number")))
}

/// Touches `mib` MiB, 16 MiB at a time, every word a different value from a xorshift
/// sequence, so no page is a zero page, a duplicate or compressible, and returns it
/// all, still allocated. Sleeps `pause` after each chunk.
fn touch(mib: u64, pause: std::time::Duration) -> Vec<Vec<u64>> {
    const CHUNK_MIB: u64 = 16;
    let words_per_chunk = usize::try_from((CHUNK_MIB << 20) / 8).unwrap_or(usize::MAX);
    let mut held: Vec<Vec<u64>> = Vec::new();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut touched = 0;
    while touched < mib {
        let mut chunk = vec![0u64; words_per_chunk];
        for word in &mut chunk {
            state = xorshift(state);
            *word = state;
        }
        held.push(chunk);
        touched += CHUNK_MIB;
        std::thread::sleep(pause);
    }
    // Read it back, so no write above can be optimised away.
    let sum = held
        .iter()
        .flatten()
        .fold(0u64, |acc, w| acc.wrapping_add(*w));
    std::hint::black_box(sum);
    held
}

const fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// The time between chunks that touches memory at `mib_per_sec`.
const fn pace(mib_per_sec: u64) -> std::time::Duration {
    std::time::Duration::from_millis(1000 * 16 / mib_per_sec)
}

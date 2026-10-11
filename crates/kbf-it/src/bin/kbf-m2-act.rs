//! `kbf-m2-act`: the one program the M2 sample project's actions run
//! (`m2/buck2/defs.bzl`). The M2 image, distroless `base-debian12`, holds no shell and
//! no coreutils, so every action runs this binary, linked statically, from its input
//! root (CI builds it with `-C target-feature=+crt-static`; `m2/run.sh` refuses a
//! dynamically linked one). An action that runs it at all proves the farm kept the
//! input file's executable bit.
//!
//! Usage: `kbf-m2-act <verb> <out> <args...>`, writing `<out>`:
//! - `sort OUT IN`: the lines of `IN`, sorted;
//! - `upper OUT IN`: `IN`, ASCII upper-cased;
//! - `join OUT IN...`: the files, one after the other;
//! - `pause OUT SECS SALT`: sleeps `SECS` seconds, then writes `SALT`;
//! - `hog OUT MIB`: touches `MIB` MiB of distinct, incompressible memory at
//!   [`HOG_MIB_PER_SEC`] and holds all of it until done, then writes `MIB`. Slow on
//!   purpose: a lease over its cap keeps pushing into swap for several of the swap
//!   watch's one-second samples, so the watch sees it (see `m2/run.sh`).
//!
//! Exits 0 on success, 1 when the verb fails, 2 on a usage error.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

/// The hog's touch rate, in MiB per second.
const HOG_MIB_PER_SEC: u64 = 256;
/// The hog's step, in MiB: it touches one chunk, then pauses.
const HOG_CHUNK_MIB: u64 = 16;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(why)) => {
            eprintln!("kbf-m2-act: {why}");
            ExitCode::from(2)
        }
        Err(Error::Failed(why)) => {
            eprintln!("kbf-m2-act: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Usage(String),
    Failed(String),
}

fn run(args: &[String]) -> Result<(), Error> {
    let [verb, out, rest @ ..] = args else {
        return Err(Error::Usage(
            "usage: kbf-m2-act <verb> <out> <args...>".to_owned(),
        ));
    };
    let bytes = match (verb.as_str(), rest) {
        ("sort", [input]) => sort(&read(input)?),
        ("upper", [input]) => read(input)?.to_ascii_uppercase(),
        ("join", inputs) if !inputs.is_empty() => {
            let mut all = Vec::new();
            for input in inputs {
                all.extend(read(input)?);
            }
            all
        }
        ("pause", [secs, salt]) => {
            std::thread::sleep(Duration::from_secs(number(secs)?));
            format!("{salt}\n").into_bytes()
        }
        ("hog", [mib]) => {
            let mib = number(mib)?;
            hog(mib);
            format!("{mib}\n").into_bytes()
        }
        _ => {
            return Err(Error::Usage(format!(
                "bad arguments for {verb:?}: {rest:?}"
            )));
        }
    };
    let mut file =
        std::fs::File::create(out).map_err(|e| Error::Failed(format!("create {out}: {e}")))?;
    file.write_all(&bytes)
        .map_err(|e| Error::Failed(format!("write {out}: {e}")))
}

fn read(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Failed(format!("read {path}: {e}")))
}

fn number(text: &str) -> Result<u64, Error> {
    text.parse()
        .map_err(|_| Error::Usage(format!("{text:?} is not a whole number")))
}

/// The lines of `text` in byte order, each ending in a newline.
fn sort(text: &[u8]) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = text
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort_unstable();
    let mut out = Vec::with_capacity(text.len() + 1);
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// Touches `mib` MiB, [`HOG_CHUNK_MIB`] at a time at [`HOG_MIB_PER_SEC`], every word a
/// different value from a xorshift sequence, so no page is a zero page, a duplicate or
/// compressible; all of it stays allocated until the end.
fn hog(mib: u64) {
    let words_per_chunk = usize::try_from((HOG_CHUNK_MIB << 20) / 8).unwrap_or(usize::MAX);
    let pause = Duration::from_millis(1000 * HOG_CHUNK_MIB / HOG_MIB_PER_SEC);
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
        touched += HOG_CHUNK_MIB;
        std::thread::sleep(pause);
    }
    // Read it back, so no write above can be optimised away.
    let sum = held
        .iter()
        .flatten()
        .fold(0u64, |acc, w| acc.wrapping_add(*w));
    std::hint::black_box(sum);
}

const fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Catches: a sort that keeps input order, drops the last line when it has no
    /// newline, or keeps empty lines (the sample's `words` output would differ from a
    /// shell `sort`).
    #[test]
    fn sort_orders_lines_and_ends_each_with_a_newline() {
        assert_eq!(sort(b"pear\napple\n\nfig"), b"apple\nfig\npear\n");
    }

    /// Catches: a verb that writes nothing or the wrong bytes to its output, `join`
    /// that drops an input or reorders them, and `upper` that is not applied.
    #[test]
    fn upper_and_join_write_their_output_file() {
        let dir = std::env::temp_dir().join(format!("kbf-m2-act-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let a = dir.join("a").display().to_string();
        let b = dir.join("b").display().to_string();
        let out = dir.join("out").display().to_string();
        std::fs::write(&a, "one\n").expect("a");
        std::fs::write(&b, "two\n").expect("b");
        run(&args(&["upper", &out, &a])).expect("upper");
        assert_eq!(std::fs::read(&out).expect("out"), b"ONE\n");
        run(&args(&["join", &out, &a, &b])).expect("join");
        assert_eq!(std::fs::read(&out).expect("out"), b"one\ntwo\n");
        std::fs::remove_dir_all(&dir).expect("clean");
    }

    /// Catches: a missing argument or a non-number accepted as a usage the binary runs
    /// with defaults (an action would then succeed doing something else), and a failed
    /// read reported as a usage error.
    #[test]
    fn bad_arguments_are_usage_errors_and_a_missing_input_fails() {
        assert!(matches!(run(&args(&["hog", "out"])), Err(Error::Usage(_))));
        assert!(matches!(
            run(&args(&["pause", "out", "x", "salt"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(run(&args(&["join", "out"])), Err(Error::Usage(_))));
        assert!(matches!(run(&args(&["nope", "out"])), Err(Error::Usage(_))));
        assert!(matches!(
            run(&args(&["sort", "out", "/nonexistent/kbf-m2-act"])),
            Err(Error::Failed(_))
        ));
    }

    /// Catches: `pause` or `hog` that writes nothing, or writes other than its salt or
    /// size (the sample's outputs, and run.sh's check of `//:hog`'s, would differ).
    #[test]
    fn pause_and_hog_write_their_salt_and_size() {
        let dir = std::env::temp_dir().join(format!("kbf-m2-act-ph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let out = dir.join("out").display().to_string();
        run(&args(&["pause", &out, "0", "a-salt"])).expect("pause");
        assert_eq!(std::fs::read(&out).expect("out"), b"a-salt\n");
        run(&args(&["hog", &out, "16"])).expect("hog");
        assert_eq!(std::fs::read(&out).expect("out"), b"16\n");
        std::fs::remove_dir_all(&dir).expect("clean");
    }

    /// Catches: a xorshift step that returns its input or zero (every page the hog
    /// touches would be the same, so a kernel could share or compress them and the
    /// lease would never pass its cap).
    #[test]
    fn the_hog_sequence_does_not_repeat_early() {
        let mut x = 0x9e37_79b9_7f4a_7c15;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..10_000 {
            x = xorshift(x);
            assert_ne!(x, 0);
            assert!(seen.insert(x), "repeated {x:#x}");
        }
    }
}

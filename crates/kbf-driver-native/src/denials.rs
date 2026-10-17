//! What the macOS sandbox refused an action, for the operator.
//!
//! An action under the sandbox ([`crate::network`]) that writes outside its lease
//! directory sees only `EPERM`, which reads like any file-permission error. The
//! refusal is in the unified log, one `Sandbox: <process>(<pid>) deny(1) <operation>
//! <target>` line each. When an action fails, the driver reads the log for the
//! action's window ([`system_log`]), keeps the lines of the processes it watched
//! ([`Found`]), and adds them to the end of the action's stderr ([`Found::note`]) and
//! to the daemon's log, both capped ([`MAX_LINES`], [`MAX_NOTE_BYTES`]).
//!
//! Which processes are the action's is known from the polls of its process tree (or
//! its slot user's uid), 250 ms apart by default: a process that starts and ends
//! between two polls is not seen. Its lines, and those of other leases' processes,
//! are counted as "other" and listed in the daemon's log only, never in this lease's
//! result: a result must not carry another lease's paths.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::time::Duration;

/// Denials listed in a note, and in the daemon's log, per lease.
pub const MAX_LINES: usize = 20;

/// The most a note adds to stderr, in bytes.
pub const MAX_NOTE_BYTES: usize = 4096;

/// The longest a target is shown, in characters.
const MAX_TARGET: usize = 300;

/// How long [`system_log`] waits before it reads the log: the sandbox's lines reach the
/// unified log a moment after the refusal.
pub const SETTLE: Duration = Duration::from_secs(1);

/// How long one read of the log may take before it is given up, and what of its
/// output is looked at.
#[cfg(target_os = "macos")]
const READ_WITHIN: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const READ_BYTES: u64 = 4 << 20;

/// A reader of the sandbox's lines since an action started `ago` (it waits [`SETTLE`]
/// for the last ones to reach the log, and covers a second more): it may write to the scratch file
/// `file` (a file, not a pipe: a child another thread spawns can inherit a pipe and hold
/// it open, issue #481). A function, so a test replaces the system's log.
pub type LogReader = fn(file: &Path, ago: Duration) -> io::Result<String>;

/// One refusal: `<process>(<pid>) deny(1) <operation> <target>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Denial {
    pub pid: i32,
    pub process: String,
    pub operation: String,
    pub target: String,
}

/// The refusal in one line of `log show --style compact`: what follows `Sandbox: `, up
/// to the first `(<digits>) deny` (a process name may hold parentheses and spaces),
/// then the operation, then the rest of the line as the target. `allow` lines and
/// other text are `None`.
#[must_use]
pub fn parse_line(line: &str) -> Option<Denial> {
    let s = &line[line.find("Sandbox: ")? + "Sandbox: ".len()..];
    let mut from = 0;
    loop {
        let open = from + s[from..].find('(')?;
        let close = open + s[open..].find(')')?;
        let digits = &s[open + 1..close];
        let denied = s[close + 1..]
            .strip_prefix(" deny")
            .filter(|r| r.is_empty() || r.starts_with(['(', ' ']));
        if let Some(rest) = denied
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(pid) = digits.parse::<i32>()
        {
            // `deny(1) file-write-create /path`: past the parenthesised count.
            let after = rest.split_once(' ').map_or("", |(_, after)| after);
            let (operation, target) = after.split_once(' ').unwrap_or((after, ""));
            return Some(Denial {
                pid,
                process: s[..open].to_owned(),
                operation: operation.to_owned(),
                target: target.trim().to_owned(),
            });
        }
        from = open + 1;
    }
}

/// The refusals in a log, split by whether the process was the action's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The action's own, without repeats, sorted.
    pub mine: Vec<Denial>,
    /// Those of other processes, without repeats, sorted.
    pub other: Vec<Denial>,
}

impl Found {
    /// The refusals in `log`, those of `pids` apart from the rest.
    #[must_use]
    pub fn of(log: &str, pids: &BTreeSet<i32>) -> Self {
        let (mut mine, mut other) = (BTreeSet::new(), BTreeSet::new());
        for denial in log.lines().filter_map(parse_line) {
            if pids.contains(&denial.pid) {
                mine.insert(denial);
            } else {
                other.insert(denial);
            }
        }
        Self {
            mine: mine.into_iter().collect(),
            other: other.into_iter().collect(),
        }
    }

    /// The text added to a failed action's stderr: each refusal of its own, capped, and
    /// how many were other processes'; `None` when none of its own was seen (the
    /// others', which on a busy node are other leases' mostly, stay in the node's log).
    #[must_use]
    pub fn note(&self) -> Option<String> {
        if self.mine.is_empty() {
            return None;
        }
        let mut note = String::new();
        let _ = writeln!(
            note,
            "\nkbf: the macOS sandbox denied this action {} operation(s); it may write only \
             inside its lease directory and /dev (plus the few names its tools use in \
             the user's temporary folder):",
            self.mine.len()
        );
        for d in self.mine.iter().take(MAX_LINES) {
            let _ = writeln!(note, "kbf:   {}", shown(d));
        }
        if self.mine.len() > MAX_LINES {
            let _ = writeln!(note, "kbf:   ... and {} more", self.mine.len() - MAX_LINES);
        }
        if !self.other.is_empty() {
            let _ = writeln!(
                note,
                "kbf: {} more denial(s) by processes this lease did not watch (another \
                 lease's, or one that ended between polls) are in the node's log",
                self.other.len()
            );
        }
        if note.len() > MAX_NOTE_BYTES {
            note.truncate(note.floor_char_boundary(MAX_NOTE_BYTES));
            note.push_str("...\n");
        }
        Some(note)
    }

    /// The refusals of other processes, for the node's log, capped.
    #[must_use]
    pub fn other_lines(&self) -> Vec<String> {
        self.other.iter().take(MAX_LINES).map(shown).collect()
    }
}

/// `<process>(<pid>) <operation> <target>`, a long target cut.
pub(crate) fn shown(d: &Denial) -> String {
    let target: String = d.target.chars().take(MAX_TARGET).collect();
    let cut = if target.len() < d.target.len() {
        "..."
    } else {
        ""
    };
    format!("{}({}) {} {target}{cut}", d.process, d.pid, d.operation)
}

/// The sandbox's lines since an action started `ago` (after [`SETTLE`], over `ago` and
/// the wait and a second more) from the system's unified log, through
/// `/usr/bin/log show` (the full path: over ssh, zsh's builtin `log` shadows it), its
/// output in `file`, read at most [`READ_BYTES`] and given up after [`READ_WITHIN`].
///
/// # Errors
/// `log` could not run or did not end in time; always off macOS.
#[cfg(target_os = "macos")]
pub fn system_log(file: &Path, ago: Duration) -> io::Result<String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    std::thread::sleep(SETTLE);
    let last = ago + SETTLE + Duration::from_secs(1);
    let out = std::fs::File::create(file)?;
    let mut child = Command::new("/usr/bin/log")
        .args(["show", "--style", "compact", "--last"])
        .arg(format!("{}s", last.as_secs().max(1)))
        .args(["--predicate", "sender == \"Sandbox\""])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(Stdio::null())
        .spawn()?;
    let give_up = Instant::now() + READ_WITHIN;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= give_up {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(file);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("log show took more than {READ_WITHIN:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut text = String::new();
    let read = std::fs::File::open(file)?
        .take(READ_BYTES)
        .read_to_string(&mut text);
    let _ = std::fs::remove_file(file);
    read?;
    if status.success() {
        Ok(text)
    } else {
        Err(io::Error::other(format!("log show: {status}")))
    }
}

/// The sandbox's lines since an action started `ago`: macOS only.
///
/// # Errors
/// Always: there is no sandbox log off macOS.
#[cfg(not(target_os = "macos"))]
pub fn system_log(_: &Path, _: Duration) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sandbox log off macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "2026-10-17 10:00:00.1 Df kernel[0:1] (Sandbox) Sandbox: touch(123) deny(1) file-write-create /work/x/y";

    /// Catches a parser that loses the pid, the operation or the target, takes the
    /// count `(1)` for the pid, or cuts a target with spaces.
    #[test]
    fn a_deny_line_gives_its_pid_operation_and_target() {
        let d = parse_line(LINE).expect("a denial");
        assert_eq!(
            d,
            Denial {
                pid: 123,
                process: "touch".into(),
                operation: "file-write-create".into(),
                target: "/work/x/y".into(),
            }
        );
        let spaced = "x Sandbox: Xcode Helper (Plugin)(77) deny(1) file-write-data /a b/c";
        let d = parse_line(spaced).expect("a denial");
        assert_eq!((d.pid, d.process.as_str()), (77, "Xcode Helper (Plugin)"));
        assert_eq!(
            (d.operation.as_str(), d.target.as_str()),
            ("file-write-data", "/a b/c")
        );
    }

    /// Catches an allow line, a line of another sender and an unnumbered process read
    /// as denials.
    #[test]
    fn only_deny_lines_with_a_pid_are_denials() {
        assert_eq!(
            parse_line("Sandbox: touch(1) allow(1) file-write-create /x"),
            None
        );
        assert_eq!(parse_line("no sandbox here"), None);
        assert_eq!(
            parse_line("Sandbox: touch(x) deny(1) file-write-create /x"),
            None
        );
        assert_eq!(
            parse_line("Sandbox: touch() deny(1) file-write-create /x"),
            None
        );
        assert_eq!(
            parse_line("Sandbox: touch(1) denied file-write-create /x"),
            None
        );
        // Parentheses that never close, or never name a pid.
        assert_eq!(
            parse_line("Sandbox: touch(12 deny(1) file-write-create /x"),
            None
        );
        assert_eq!(parse_line("Sandbox: touch deny file-write-create /x"), None);
        let bare = parse_line("Sandbox: touch(1) deny file-read-data /x").expect("denial");
        assert_eq!(bare.operation, "file-read-data");
        let none = parse_line("Sandbox: touch(1) deny(1)").expect("denial");
        assert_eq!((none.operation.as_str(), none.target.as_str()), ("", ""));
    }

    /// Catches another lease's denial listed as this one's, and a repeat listed twice.
    #[test]
    fn the_actions_denials_are_apart_from_others_and_without_repeats() {
        let other = LINE.replace("(123)", "(999)");
        let log = format!("{LINE}\n{LINE}\n{other}\nnoise\n");
        let found = Found::of(&log, &BTreeSet::from([123]));
        assert_eq!(found.mine.len(), 1);
        assert_eq!(found.other.len(), 1);
        assert_eq!(found.other[0].pid, 999);
        assert_eq!(
            found.other_lines(),
            ["touch(999) file-write-create /work/x/y"]
        );
        let note = found.note().expect("a note");
        assert!(
            note.contains("kbf:   touch(123) file-write-create /work/x/y\n"),
            "{note}"
        );
        assert!(!note.contains("999"), "{note}");
        assert!(note.contains("1 more denial(s)"), "{note}");
        assert!(note.ends_with('\n'), "{note:?}");
    }

    /// Catches a note for a log with no denials of the action's own: stderr of a
    /// failure that was no sandbox's (a test failing, a compile error) stays the action's.
    #[test]
    fn nothing_of_the_actions_own_is_no_note() {
        assert_eq!(Found::default().note(), None);
        let found = Found::of(LINE, &BTreeSet::new());
        assert_eq!(found.note(), None);
        assert_eq!(found.other_lines().len(), 1);
    }

    /// Catches an unbounded note: many denials, one with a huge target.
    #[test]
    fn a_note_is_capped_in_lines_and_bytes() {
        let mut log = String::new();
        for i in 0..50 {
            log.push_str(&format!("Sandbox: t(1) deny(1) file-write-create /p{i}\n"));
        }
        let found = Found::of(&log, &BTreeSet::from([1]));
        let note = found.note().expect("a note");
        assert_eq!(note.matches("kbf:   t(1)").count(), MAX_LINES);
        assert!(
            note.contains(&format!("... and {} more", 50 - MAX_LINES)),
            "{note}"
        );
        // Long targets of two-byte characters: the cut falls inside one unless moved.
        let mut huge = String::new();
        for i in 0..MAX_LINES {
            huge.push_str(&format!(
                "Sandbox: t(1) deny(1) file-write-create /{i}{}\n",
                "\u{e9}".repeat(5000)
            ));
        }
        let note = Found::of(&huge, &BTreeSet::from([1]))
            .note()
            .expect("a note");
        assert!(note.len() <= MAX_NOTE_BYTES + 4, "{}", note.len());
        assert!(note.ends_with("...\n"), "{note}");
        let shown_target = note.lines().nth(2).expect("a line").chars().count();
        assert!(
            shown_target < MAX_TARGET + 40,
            "a target is cut: {shown_target}"
        );
    }

    /// Catches a log reader off macOS that claims to read something.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_there_is_no_sandbox_log() {
        let why =
            system_log(Path::new("/nonexistent"), Duration::from_secs(1)).expect_err("no log");
        assert_eq!(why.kind(), io::ErrorKind::Unsupported);
    }
}

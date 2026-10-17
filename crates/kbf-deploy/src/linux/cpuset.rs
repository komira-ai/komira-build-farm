//! Rule `cpuset` of [`super::modes`]: the unit of a mode that runs builds, and
//! `kbf.slice` above it, keep every CPU and NUMA node of the node. The daemon offers the
//! node's CPUs to its leases, and `--lease-placement=numa-node` pins each lease to one
//! NUMA node's CPUs and memory; an `AllowedCPUs=` or `AllowedMemoryNodes=` narrower
//! than the node keeps the unit's cgroup, and every lease in it, off the rest.
//!
//! The lists are read as systemd reads them (systemd.resource-control(5);
//! `config_parse_cpu_set` in `src/shared/cpu-set-util.c`): each assignment adding to the
//! earlier ones, an empty one resetting the list. A value is split into words at
//! spaces, tabs, line breaks and commas by `extract_first_word(..., EXTRACT_UNQUOTE)`
//! (`src/basic/extract-word.c`): `'...'` and `"..."` group and are taken off anywhere in
//! a word, and a `\` keeps the next character (no C escapes: the flag is not given).
//! An unclosed quote or a trailing `\` makes systemd ignore the whole assignment. Each
//! word is a number or an `a-b` range (`parse_range` and `safe_atou` in
//! `src/basic/parse-util.c`: `0x`, `0o`, `0b` and a leading `0` pick the base, a `+` or
//! leading spaces are allowed), and a malformed word, `b-a`, or a number at or past
//! systemd's bound is ignored. A `%` specifier is not expanded: a value with one is
//! read as written and a `WARN cpuset:` line says so. The node's own sets are the kernel's
//! `/sys/devices/system/cpu/online` and `/sys/devices/system/node/online`, read on the
//! host `check-units` runs on: run it on the node. Where they cannot be read (another
//! system, a container without `/sys`), a set list is not compared and a
//! `WARN cpuset:` line says so. `StartupAllowedCPUs=` and `StartupAllowedMemoryNodes=`
//! (boot and shutdown only) are not read.

use std::fmt;

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;

use crate::unit::{UnitFile, WHITESPACE};

/// systemd's bound on a CPU number (`CPU_SET_MAX_NCPU`): a word at or past it is ignored.
const MAX_ID: u32 = 8192;

/// The keys the rule reads, what each lists, and the kernel file with the node's own.
pub(super) const KEYS: [(&str, &str, &str); 2] = [
    ("AllowedCPUs", "CPUs", "/sys/devices/system/cpu/online"),
    (
        "AllowedMemoryNodes",
        "NUMA nodes",
        "/sys/devices/system/node/online",
    ),
];

/// A set of CPU or NUMA node numbers, as sorted, disjoint, non-adjacent ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Ids(Vec<(u32, u32)>);

impl Ids {
    /// One assignment's list as systemd reads it: malformed words, `b-a` and numbers
    /// past systemd's bound are ignored, and a value that does not split into words
    /// (an unclosed quote, a trailing `\`) adds nothing.
    pub fn lenient(value: &str) -> Ids {
        let mut ids = Ids::default();
        for word in unquoted_words(value).unwrap_or_default() {
            if let Some((a, b)) = range(&word).filter(|&(_, b)| b < MAX_ID) {
                ids.add(a, b);
            }
        }
        ids
    }

    /// A list as the kernel writes it (`0-3,8`); `None` when a word is malformed.
    pub fn strict(text: &str) -> Option<Ids> {
        let mut ids = Ids::default();
        for word in text.trim().split(',') {
            let (a, b) = range(word)?;
            ids.add(a, b);
        }
        Some(ids)
    }

    fn add(&mut self, a: u32, b: u32) {
        self.0.push((a, b));
        self.0.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.0.len());
        for &(a, b) in &self.0 {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.0 = merged;
    }

    fn extend(&mut self, other: &Ids) {
        for &(a, b) in &other.0 {
            self.add(a, b);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether every number of `other` is in this set.
    pub fn covers(&self, other: &Ids) -> bool {
        other
            .0
            .iter()
            .all(|&(a, b)| self.0.iter().any(|&(x, y)| x <= a && b <= y))
    }
}

impl fmt::Display for Ids {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self
            .0
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect();
        f.write_str(&words.join(","))
    }
}

/// The words of a list value, as `extract_first_word(&p, &word, WHITESPACE ",",
/// EXTRACT_UNQUOTE)` gives them; `None` where it fails (an unclosed quote, a trailing
/// `\`), which makes systemd ignore the assignment.
fn unquoted_words(value: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            // Inside quotes too: `\` keeps the next character as it is.
            (_, '\\') => word.get_or_insert_with(String::new).push(chars.next()?),
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, c) if c == ',' || WHITESPACE.contains(&c) => out.extend(word.take()),
            (_, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    out.extend(word);
    Some(out)
}

/// `n` or `a-b` with `a <= b`, as systemd's `parse_range` reads a word: the lower
/// bound runs to the first `-` that no `\` keeps (each `\` taken off it), the upper
/// bound is the rest as written.
fn range(word: &str) -> Option<(u32, u32)> {
    let mut lower = String::new();
    let mut upper = None;
    let mut chars = word.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => lower.push(chars.next()?.1),
            '-' => {
                upper = Some(&word[i + 1..]);
                break;
            }
            c => lower.push(c),
        }
    }
    let a = safe_atou(&lower)?;
    let b = match upper {
        None => a,
        Some(text) => safe_atou(text)?,
    };
    (a <= b).then_some((a, b))
}

/// systemd's `safe_atou` (`safe_atou_full` with base 0): leading `WHITESPACE` skipped,
/// `0b` or `0o` (either case) picking base 2 or 8, then glibc's `strtoul`; a number
/// written with a `-` that is not 0, or past `u32`, refused.
fn safe_atou(text: &str) -> Option<u32> {
    let text = text.trim_start_matches(WHITESPACE);
    let (text, base) = match text.get(..2) {
        Some("0b" | "0B") => (&text[2..], 2),
        Some("0o" | "0O") => (&text[2..], 8),
        _ => (text, 0),
    };
    let value = strtoul(text, base)?;
    if value != 0 && text.starts_with('-') {
        return None;
    }
    u32::try_from(value).ok()
}

/// glibc's `strtoul(text, &end, base)` in the C locale, with `*end` required to be the
/// end: `isspace` skipped, a `+` or `-` (which negates modulo 2^64), and for base 0 a
/// `0x` before a hex digit for 16, a leading `0` for 8, else 10; `None` with no digit,
/// a character left over, or a number past `u64`.
fn strtoul(text: &str, base: u32) -> Option<u64> {
    let text = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let hex = |t: &str| {
        t.get(2..3)
            .is_some_and(|c| c.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let (digits, base) = match base {
        0 if matches!(text.get(..2), Some("0x" | "0X")) && hex(text) => (&text[2..], 16),
        0 if text.starts_with('0') => (text, 8),
        0 => (text, 10),
        base => (text, base),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return None;
    }
    let value = u64::from_str_radix(digits, base).ok()?;
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

/// Whether `value` holds a `%` specifier, which systemd expands before it reads the
/// list (`config_parse_unit_cpu_set`); `%%` is a plain `%`.
fn has_specifier(value: &str) -> bool {
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '%' && chars.next() != Some('%') {
            return true;
        }
    }
    false
}

/// The set `key` in `[section]` leaves, every assignment applied in order; `None` when
/// none is left (never set, reset, or only malformed words): no restriction.
pub(super) fn allowed(unit: &UnitFile, section: &str, key: &str) -> Option<Ids> {
    let mut ids = Ids::default();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == section && d.key == key)
    {
        if d.value.is_empty() {
            ids = Ids::default();
        } else {
            ids.extend(&Ids::lenient(&d.value));
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// The node's set from the kernel file `file`, or why it cannot be read.
pub(super) fn node<E: Exec>(exec: &E, file: &str) -> Result<Ids, String> {
    let path = HostPath::new(file).map_err(|e| e.to_string())?;
    match exec.inspect(&path) {
        Ok(Some(Entry::File { bytes, .. })) => Ids::strict(&String::from_utf8_lossy(&bytes))
            .ok_or_else(|| format!("{file} is not a list of numbers")),
        Ok(Some(_)) => Err(format!("{file} is not a file")),
        Ok(None) => Err(format!("no {file}")),
        Err(e) => Err(format!("{file}: {e}")),
    }
}

/// Rule `cpuset` on `unit` (the file `name`, its keys in `[section]`): failures, and
/// `WARN cpuset:` notes for a list that could not be compared.
pub(super) fn check<E: Exec>(
    exec: &E,
    name: &str,
    unit: &UnitFile,
    section: &str,
    failures: &mut Vec<String>,
    notes: &mut Vec<String>,
) {
    for (key, what, file) in KEYS {
        notes.extend(
            unit.directives()
                .iter()
                .filter(|d| d.section == section && d.key == key && has_specifier(&d.value))
                .map(|d| {
                    format!(
                        "WARN cpuset: {name} sets {key}={}, whose % specifier is not \
                         expanded here: the value is read as written",
                        d.value
                    )
                }),
        );
        let Some(set) = allowed(unit, section, key) else {
            continue;
        };
        match node(exec, file) {
            Ok(all) if set.covers(&all) => {}
            Ok(all) => failures.push(format!(
                "{name} sets {key}={set}, narrower than the node's {what} {all}: the daemon \
                 places leases on every one of them (--lease-placement=numa-node pins a \
                 lease to one NUMA node), and the unit's cgroup keeps them off the rest"
            )),
            Err(why) => notes.push(format!(
                "WARN cpuset: {name} sets {key}={set}, and the node's {what} cannot be read \
                 here ({why}): run check-units on the node to compare them"
            )),
        }
    }
}

#[cfg(test)]
#[path = "cpuset_tests.rs"]
mod tests;

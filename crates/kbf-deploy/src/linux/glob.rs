//! Whether an `EnvironmentFile=` wildcard names a path, as systemd expands it.
//!
//! systemd hands the path to `safe_glob(fn, 0, &paths)` (`exec_context_load_environment`
//! in `src/core/execute.c`), which runs glibc's `glob(3)` with no flag but
//! `GLOB_ALTDIRFUNC`, its readdir skipping `.` and `..` (`src/basic/glob-util.c`). PID 1
//! sets no locale, so glob and fnmatch run in the C locale, one byte a character. What
//! that gives, from glibc's `posix/glob.c` and `posix/fnmatch_loop.c`:
//!
//! - The pattern is taken apart at every `/`, and a part ending in an odd run of `\`
//!   loses the last one (`foo\/bar`). The parts before the first that holds a
//!   wildcard (`*`, `?`, or `[` with or without its `]`) name directories as written,
//!   each `\` taken off and keeping the next byte. Every later part is matched against
//!   the names in its directory by `fnmatch(3)` with `FNM_PERIOD`, so `.` and `..` (not
//!   listed) never match one, except a part with no wildcard and no `\`, which is
//!   opened as written. A `.` or `..` part opened as written is resolved by its words
//!   alone, as [`super::credential::normalise`] does (`..` over a wildcard assumes it
//!   matches some directory).
//! - A trailing `/` is taken off (`pattern/` is expanded as `pattern`, with the last `\`
//!   of an odd run before it gone) with GLOB_ONLYDIR set, which only the directory read
//!   honours: a last name with no wildcard and no `\` is opened as written and listed
//!   even when it is a file; any other lists directories only. A last `.` or `..`
//!   names a directory (after a file, the kernel answers ENOTDIR).
//! - In a part: `?` is one byte and `*` any run of them; neither, nor a `[...]`,
//!   matches a leading `.`. `\` keeps the next byte, a trailing one matches nothing.
//! - `[...]` is one byte: `!` or `^` first negates, a `]` first is a member, `a-z` a
//!   range by byte value, `\` keeps the next byte, `[:alpha:]` and the other C-locale
//!   classes (an unknown name matches nothing), `[=c=]` and `[.c.]` the byte `c` itself.
//!   A `[` that no `]` closes is a plain `[`.
//!
//! No braces or `~`: systemd passes neither `GLOB_BRACE` nor `GLOB_TILDE`. A leading
//! `^` negates as it does when PID 1's environment has no `POSIXLY_CORRECT` (with it,
//! glibc reads the `^` as a member).

/// `__glob_pattern_type`'s answers (`posix/glob_internal.h`).
const SPECIAL: u8 = 1;
const BACKSLASH: u8 = 2;
const BRACKET: u8 = 4;

/// glibc's bound on a class name in `[:name:]` (`CHARCLASS_NAME_MAX`).
const CLASS_NAME_MAX: usize = 2048;

/// What `text` holds, as `__glob_pattern_type(text, 1)` reads it: [`SPECIAL`] for a
/// `*`, `?` or a `[` closed by a `]`, [`BRACKET`] for a `[` alone, [`BACKSLASH`].
fn kind(text: &[u8]) -> u8 {
    let mut out = 0;
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'?' | b'*' => return SPECIAL,
            b'\\' => {
                if i + 1 < text.len() {
                    i += 1;
                }
                out |= BACKSLASH;
            }
            b'[' => out |= BRACKET,
            b']' if out & BRACKET != 0 => return SPECIAL,
            _ => {}
        }
        i += 1;
    }
    out
}

/// Whether `text` is a wildcard: glob(3) reads a directory for it.
pub(super) fn is_wildcard(text: &str) -> bool {
    kind(text.as_bytes()) & (SPECIAL | BRACKET) != 0
}

/// `part` without the last `\` of an odd run that ends it.
fn before_slash(part: &[u8]) -> &[u8] {
    let run = part.iter().rev().take_while(|b| **b == b'\\').count();
    &part[..part.len() - run % 2]
}

/// `part` with each `\` taken off and the byte after it kept.
fn unescape(part: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(part.len());
    let mut bytes = part.iter();
    while let Some(&b) = bytes.next() {
        out.extend(if b == b'\\' { bytes.next() } else { Some(&b) });
    }
    out
}

/// One part of a pattern: a directory named as written, or a name fnmatch matches.
enum Part<'a> {
    Name(Vec<u8>),
    Match(&'a [u8]),
}

/// The parts of the absolute `pattern`, one per name of the paths it matches.
fn parts(pattern: &[u8]) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let mut split = pattern.split(|b| *b == b'/').skip(1).collect::<Vec<_>>();
    let last = split.pop().unwrap_or_default();
    let mut literal = true;
    for part in split {
        let part = before_slash(part);
        let kind = kind(part);
        literal &= kind & (SPECIAL | BRACKET) == 0;
        // A part glob(3) opens as written; past the first wildcard, only one with no
        // `\` (a `\` sends glibc to read the directory, which lists no `.` or `..`).
        let name = if literal {
            unescape(part)
        } else if kind == 0 {
            part.to_vec()
        } else {
            out.push(Part::Match(part));
            continue;
        };
        match name.as_slice() {
            b"" | b"." => {}
            b".." => {
                out.pop();
            }
            _ => out.push(Part::Name(name)),
        }
    }
    out.push(Part::Match(last));
    out
}

/// `pattern` without the `/` that end it, and whether there were any: glob(3) expands
/// `pattern/` as `pattern` with GLOB_ONLYDIR, first taking the last `\` of an odd run
/// off what is left (glibc's `"pattern/"` case, once for each `/`).
fn trailing_slashes(mut pattern: &[u8]) -> (&[u8], bool) {
    let mut only_dirs = false;
    while pattern.len() > 1 && pattern.ends_with(b"/") {
        pattern = before_slash(&pattern[..pattern.len() - 1]);
        only_dirs = true;
    }
    (pattern, only_dirs)
}

/// The last name of `pattern`, after its last `/`.
fn last_name(pattern: &[u8]) -> &[u8] {
    pattern.rsplit(|b| *b == b'/').next().unwrap_or_default()
}

/// Whether glob(3), as systemd calls it, lists the file `path` (absolute, with no `.`
/// or `..`) for the absolute `pattern`. Before a trailing `/`, a last name with no
/// wildcard and no `\` is opened as written (`glob_in_dir` checks no GLOB_ONLYDIR
/// there, so a file is listed); any other is read from its directory, directories only.
pub(super) fn matches(pattern: &str, path: &str) -> bool {
    if !pattern.starts_with('/') {
        return false;
    }
    let (pattern, only_dirs) = trailing_slashes(pattern.as_bytes());
    if only_dirs && kind(last_name(pattern)) != 0 {
        return false;
    }
    let names: Vec<&[u8]> = path
        .as_bytes()
        .split(|b| *b == b'/')
        .filter(|n| !n.is_empty())
        .collect();
    let parts = parts(pattern);
    parts.len() == names.len()
        && parts.iter().zip(&names).all(|(part, name)| match part {
            Part::Name(n) => n == name,
            Part::Match(p) => fnmatch(p, name),
        })
}

/// The path of the file a pattern with no wildcard lists, its `\` taken off and a
/// trailing `/` dropped as glob(3) drops it; `None` for a wildcard, a name ending in a
/// lone `\` (which matches nothing), and a pattern that lists no file: a last `.` or
/// `..`, or a name with a `\` before a trailing `/` (read for directories only).
pub(super) fn literal(text: &str) -> Option<String> {
    if is_wildcard(text) {
        return None;
    }
    let (text, only_dirs) = trailing_slashes(text.as_bytes());
    let name = last_name(text);
    let dir = text
        .len()
        .checked_sub(name.len() + 1)
        .map(|end| &text[..end]);
    if before_slash(name).len() != name.len()
        || (only_dirs && kind(name) != 0)
        || matches!(unescape(name).as_slice(), b"." | b"..")
    {
        return None;
    }
    let mut out = Vec::new();
    for part in dir.into_iter().flat_map(|d| d.split(|b| *b == b'/')) {
        out.extend(unescape(before_slash(part)));
        out.push(b'/');
    }
    out.extend(unescape(name));
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// glibc's `fnmatch(pattern, name, FNM_PERIOD)` in the C locale, for one name.
pub(super) fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    matched(pattern, name, true)
}

/// The byte at `i`, or 0 past the end (the C strings' terminator).
fn at(p: &[u8], i: usize) -> u8 {
    p.get(i).copied().unwrap_or(0)
}

/// `internal_fnmatch`: `leading` is whether a `.` here is one wildcards do not match.
fn matched(pat: &[u8], s: &[u8], mut leading: bool) -> bool {
    let (mut p, mut n) = (0, 0);
    while p < pat.len() {
        let mut c = pat[p];
        p += 1;
        match c {
            b'*' => return star(pat, p, s, n, leading),
            b'?' if n == s.len() || (s[n] == b'.' && leading) => return false,
            b'?' => {}
            b'[' if n == s.len() || (s[n] == b'.' && leading) => return false,
            b'[' => match bracket(pat, p, s[n]) {
                Bracket::Matched(end) => p = end,
                Bracket::NoMatch => return false,
                // No `]` closes it: a plain `[`.
                Bracket::Plain if s[n] == b'[' => {}
                Bracket::Plain => return false,
            },
            _ => {
                if c == b'\\' {
                    // A trailing `\` matches nothing.
                    if p == pat.len() {
                        return false;
                    }
                    c = pat[p];
                    p += 1;
                }
                if n == s.len() || s[n] != c {
                    return false;
                }
            }
        }
        leading = false;
        n += 1;
    }
    n == s.len()
}

/// A `*` whose next pattern byte is at `p`, at `n` in the name.
fn star(pat: &[u8], mut p: usize, s: &[u8], mut n: usize, mut leading: bool) -> bool {
    if n < s.len() && s[n] == b'.' && leading {
        return false;
    }
    let mut c = at(pat, p);
    p += 1;
    while c == b'?' || c == b'*' {
        if c == b'?' {
            if n == s.len() {
                return false;
            }
            n += 1;
        }
        c = at(pat, p);
        p += 1;
    }
    if c == 0 {
        return true;
    }
    // The rest of the pattern, from the byte `c`, tried at each place it can start.
    let rest = &pat[p - 1..];
    let first = if c == b'\\' { at(pat, p) } else { c };
    for k in n..s.len() {
        if (c == b'[' || s[k] == first) && matched(rest, &s[k..], leading) {
            return true;
        }
        leading = false;
    }
    false
}

/// What a `[...]` does with one byte.
enum Bracket {
    /// It matches; the pattern goes on at this index, past the `]`.
    Matched(usize),
    /// It does not, and so neither does the pattern.
    NoMatch,
    /// No `]` closes it: the `[` is a plain character.
    Plain,
}

/// A C-locale class (`wctype`), or `None` for an unknown name.
fn class(name: &[u8]) -> Option<fn(&u8) -> bool> {
    Some(match name {
        b"alnum" => u8::is_ascii_alphanumeric,
        b"alpha" => u8::is_ascii_alphabetic,
        b"blank" => |b: &u8| matches!(*b, b' ' | b'\t'),
        b"cntrl" => u8::is_ascii_control,
        b"digit" => u8::is_ascii_digit,
        b"graph" => u8::is_ascii_graphic,
        b"lower" => u8::is_ascii_lowercase,
        b"print" => |b: &u8| b.is_ascii_graphic() || *b == b' ',
        b"punct" => u8::is_ascii_punctuation,
        b"space" => |b: &u8| matches!(*b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
        b"upper" => u8::is_ascii_uppercase,
        b"xdigit" => u8::is_ascii_hexdigit,
        _ => return None,
    })
}

/// How the `[:` at `p - 1` reads.
enum ClassRead {
    /// Not a class: a byte outside `a`..`y` before the `:]`, so the `[` is a member.
    NotOne,
    /// A class name glibc does not know, or one too long: the pattern matches nothing.
    Unknown,
    /// A class, and the index past its `:]`.
    Known(fn(&u8) -> bool, usize),
}

fn class_at(pat: &[u8], mut p: usize) -> ClassRead {
    let start = p;
    for _ in 0..CLASS_NAME_MAX {
        p += 1;
        let c = at(pat, p);
        if c == b':' && at(pat, p + 1) == b']' {
            return match class(&pat[start + 1..p]) {
                Some(test) => ClassRead::Known(test, p + 2),
                None => ClassRead::Unknown,
            };
        }
        if not_in_class_name(c) {
            return ClassRead::NotOne;
        }
    }
    ClassRead::Unknown
}

/// glibc's test for a byte that cannot be in a class name (`c < 'a' || c >= 'z'`: no
/// name holds a `z`).
fn not_in_class_name(c: u8) -> bool {
    !(b'a'..=b'y').contains(&c)
}

/// The `[.x.]` whose `.` is at `p`: its byte and the index past it; `None` (the
/// pattern matches nothing) unless it is one byte and closed.
fn symbol(pat: &[u8], p: usize) -> Option<(u8, usize)> {
    let mut i = p + 1;
    loop {
        match at(pat, i) {
            0 => return None,
            b'.' if at(pat, i + 1) == b']' => break,
            _ => i += 1,
        }
    }
    (i == p + 2).then(|| (pat[p + 1], i + 2))
}

/// Whether a `-` at `p` makes a range of the member before it: `normal_bracket`'s
/// test, and the looser one after a `[.x.]` (a `]` may end it).
fn range_at(pat: &[u8], p: usize, symbol: bool) -> bool {
    at(pat, p) == b'-' && at(pat, p + 1) != 0 && (symbol || at(pat, p + 1) != b']')
}

/// The `[...]` whose body starts at `p` against the byte `f`, as `fnmatch_loop.c`
/// reads it in the C locale (no collation rules: byte order, one-byte symbols).
fn bracket(pat: &[u8], mut p: usize, f: u8) -> Bracket {
    let not = matches!(at(pat, p), b'!' | b'^');
    p += usize::from(not);
    let mut c = at(pat, p);
    p += 1;
    loop {
        // Whether `c` is read by the last arm (`[.x.]` is seen there), not reached by a
        // `goto normal_bracket`.
        let mut direct = true;
        if c == b'\\' {
            if at(pat, p) == 0 {
                return Bracket::NoMatch;
            }
            c = pat[p];
            p += 1;
            direct = false;
        } else if c == b'[' && at(pat, p) == b':' {
            match class_at(pat, p) {
                ClassRead::NotOne => direct = false,
                ClassRead::Unknown => return Bracket::NoMatch,
                ClassRead::Known(test, end) if test(&f) => return skip(pat, end, not),
                ClassRead::Known(_, end) => {
                    c = at(pat, end);
                    p = end + 1;
                    if c == b']' {
                        break;
                    }
                    continue;
                }
            }
        } else if c == b'[' && at(pat, p) == b'=' {
            if at(pat, p + 1) == 0 || at(pat, p + 2) != b'=' || at(pat, p + 3) != b']' {
                direct = false;
            } else if f == pat[p + 1] {
                return skip(pat, p + 4, not);
            } else {
                c = at(pat, p + 4);
                p += 5;
                if c == b']' {
                    break;
                }
                continue;
            }
        } else if c == 0 {
            return Bracket::Plain;
        }
        let low = match (direct && c == b'[' && at(pat, p) == b'.').then(|| symbol(pat, p)) {
            Some(None) => return Bracket::NoMatch,
            Some(Some((sym, end))) => {
                p = end;
                (sym, true)
            }
            None => (c, false),
        };
        if !range_at(pat, p, low.1) && low.0 == f {
            return skip(pat, p, not);
        }
        c = at(pat, p);
        p += 1;
        if c == b'-' && at(pat, p) != b']' {
            let mut high = at(pat, p);
            p += 1;
            if high == b'[' && at(pat, p) == b'.' {
                let Some((sym, end)) = symbol(pat, p) else {
                    return Bracket::NoMatch;
                };
                high = sym;
                p = end;
            } else {
                if high == b'\\' {
                    high = at(pat, p);
                    p += 1;
                }
                if high == 0 {
                    return Bracket::NoMatch;
                }
            }
            if low.0 <= f && f <= high {
                return skip(pat, p, not);
            }
            c = at(pat, p);
            p += 1;
        }
        if c == b']' {
            break;
        }
    }
    if not {
        Bracket::Matched(p)
    } else {
        Bracket::NoMatch
    }
}

/// The rest of a `[...]` that matched, from `p`: `Matched` past its `]` (or `NoMatch`
/// when it is negated), as glibc's `matched:` loop skips it.
fn skip(pat: &[u8], mut p: usize, not: bool) -> Bracket {
    loop {
        let c = at(pat, p);
        p += 1;
        match c {
            b']' => break,
            0 => return Bracket::Plain,
            b'\\' if at(pat, p) == 0 => return Bracket::NoMatch,
            b'\\' => p += 1,
            b'[' if at(pat, p) == b':' => {
                let start = p;
                for len in 1.. {
                    p += 1;
                    if len == CLASS_NAME_MAX {
                        return Bracket::NoMatch;
                    }
                    if at(pat, p) == b':' && at(pat, p + 1) == b']' {
                        p += 2;
                        break;
                    }
                    if not_in_class_name(at(pat, p)) {
                        p = start;
                        break;
                    }
                }
            }
            b'[' if at(pat, p) == b'=' => {
                if at(pat, p + 1) == 0 || at(pat, p + 2) != b'=' || at(pat, p + 3) != b']' {
                    return Bracket::NoMatch;
                }
                p += 4;
            }
            b'[' if at(pat, p) == b'.' => {
                loop {
                    p += 1;
                    match at(pat, p) {
                        0 => return Bracket::NoMatch,
                        b'.' if at(pat, p + 1) == b']' => break,
                        _ => {}
                    }
                }
                p += 2;
            }
            _ => {}
        }
    }
    if not {
        Bracket::NoMatch
    } else {
        Bracket::Matched(p)
    }
}

#[cfg(test)]
#[path = "glob_tests.rs"]
mod tests;

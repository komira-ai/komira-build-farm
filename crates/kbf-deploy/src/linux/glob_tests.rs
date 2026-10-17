//! The glob(3) reading behind rule `credential`'s `EnvironmentFile=` wildcards. The
//! expected answers were taken from glibc 2.42 itself (`fnmatch(3)` with `FNM_PERIOD`
//! and `glob(3)` over a tree holding `etc/kbf/s3.env` and `etc/other/`, both in the C
//! locale); the rows marked otherwise follow from systemd's or glibc's sources.

// How the glibc rows were made: by hand, on a Linux host with glibc 2.42, never in CI
// (the answers are pasted into the rows below; nothing here calls the host's libc).
// The generator is not a checked-in file: the repository lints refuse any new Python
// file (tools/interpreter_budget.tsv only shrinks). It is this script, reconstructed
// from the run behind these rows (not rerun since), saved as `glob_expected.py` in an
// empty scratch directory with the pairs pasted in and run there as
//
//     LC_ALL=C python3 -I glob_expected.py
//
//     import ctypes, os
//     libc = ctypes.CDLL("libc.so.6", use_errno=True)
//     libc.setlocale(6, b"C")  # LC_ALL, as PID 1 runs
//     libc.gnu_get_libc_version.restype = ctypes.c_char_p
//     print("glibc", libc.gnu_get_libc_version().decode())
//     FNM_PERIOD = 4
//     def fnmatch(pattern, name):
//         return libc.fnmatch(pattern, name, FNM_PERIOD) == 0
//     class Glob(ctypes.Structure):
//         _fields_ = [("pathc", ctypes.c_size_t),
//                     ("pathv", ctypes.POINTER(ctypes.c_char_p)),
//                     ("offs", ctypes.c_size_t), ("flags", ctypes.c_int),
//                     ("funcs", ctypes.c_void_p * 5)]
//     def glob(pattern):
//         g = Glob()
//         libc.glob(pattern, 0, None, ctypes.byref(g))
//         found = [g.pathv[i] for i in range(g.pathc)]
//         libc.globfree(ctypes.byref(g))
//         return found
//     # names_match_as_glibc_fnmatch_matches_them: (pattern, name) as bytes
//     FNMATCH = [(rb"s3.env", rb"s3.env")]
//     # paths_match_as_glibc_glob_lists_them: the glibc rows' patterns
//     PATHS = [rb"/etc/kbf/s3.env"]
//     CLASSES = [b"alnum", b"alpha", b"blank", b"cntrl", b"digit", b"graph",
//                b"lower", b"print", b"punct", b"space", b"upper", b"xdigit"]
//     for pattern, name in FNMATCH:
//         print(pattern, name, fnmatch(pattern, name))
//     for c in CLASSES:
//         print(c, bytes(b for b in range(1, 256)
//                        if fnmatch(b"[[:" + c + b":]]", bytes([b]))))
//     os.makedirs("etc/kbf"); os.makedirs("etc/other")
//     open("etc/kbf/s3.env", "w").close()
//     root = os.getcwd().encode()  # a path with no wildcard or `\` in it
//     for pattern in PATHS:
//         print(pattern, root + b"/etc/kbf/s3.env" in glob(root + pattern))
//
// glob(3) is called with no GLOB_ALTDIRFUNC, so its readdir lists `.` and `..`, which
// systemd's does not: the rows marked as systemd's are not the script's. A class row
// is the bytes printed for that class.

use super::*;

const KEY: &str = "/etc/kbf/s3.env";

/// Catches a part of fnmatch read other than glibc reads it: `?` and `*` (a leading
/// `.`, runs of `*?`, the `*` before an escape or a bracket, glibc's `*?[` quirk on a
/// `.`), `\` escapes (a trailing one matches nothing), and `[...]`: negation by `!` and
/// `^`, a `]` first, ranges (a `-` last, escaped bounds, an unfinished range), classes
/// (unknown names, uppercase, unterminated, inside a matched bracket), `[=c=]`,
/// `[.c.]` (in a range at either end, longer than one byte, unclosed), and an
/// unterminated `[` as a plain character.
#[test]
fn names_match_as_glibc_fnmatch_matches_them() {
    for (pattern, name, want) in [
        (r"s3.env", "s3.env", true),
        (r"s3.env", "s3.envx", false),
        (r"s3.envx", "s3.env", false),
        (r"?3.env", "s3.env", true),
        (r"?", "", false),
        (r"?env", ".env", false),
        (r".?nv", ".env", true),
        (r"*", ".env", false),
        (r"*", "x", true),
        (r"*", "", true),
        (r".*", ".env", true),
        (r"x*", "x.env", true),
        (r"*?", "", false),
        (r"*??", "ab", true),
        (r"*?*", "a", true),
        (r"*.env", "s3.env", true),
        (r"*\.env", "s3.env", true),
        (r"*\", "s3", false),
        (r"*[3]*", "s3.env", true),
        (r"*??[.]env", "s3.env", false),
        (r"*?[!x]", "a.", false),
        (r"*[!x]", "a.", true),
        (r"\s3.env", "s3.env", true),
        (r"s3.env\", "s3.env", false),
        (r"\", "", false),
        (r"a\", "a", false),
        (r"[s]3.env", "s3.env", true),
        (r"[!s]3.env", "s3.env", false),
        (r"[^s]3.env", "s3.env", false),
        (r"[^a]3.env", "s3.env", true),
        (r"[]s]3.env", "s3.env", true),
        (r"[!]]3.env", "s3.env", true),
        (r"[]]", "]", true),
        (r"[a-z]3.env", "s3.env", true),
        (r"[t-z]3.env", "s3.env", false),
        (r"[a-]3.env", "-3.env", true),
        (r"[a-]3.env", "s3.env", false),
        (r"[\s]3.env", "s3.env", true),
        (r"[\", "s", false),
        (r"[a\-z]", "-", true),
        (r"[a\-z]", "b", false),
        (r"[a-\z]", "z", true),
        (r"[a-", "a", false),
        (r"[a-\", "a", false),
        (r"[b-", "b", false),
        (r"[[:alpha:]]3.env", "s3.env", true),
        (r"[[:digit:]]3.env", "s3.env", false),
        (r"[![:digit:]]3.env", "s3.env", true),
        (r"[[:bogus:]]", "a", false),
        (r"[[:alpha:]x]", "x", true),
        (r"[[:digit:]x]", "x", true),
        (r"[x[:digit:]]", "x", true),
        (r"[[:Alpha:]]", "A", false),
        (r"[[:Alpha:]]", "[", false),
        (r"[[:alpha:]", "a", false),
        (r"[[:alpha:]", "[:alpha:]", false),
        (r"[x[:A]", "x", true),
        (r"[x[:A]", ":", true),
        (r"[[=s=]]3.env", "s3.env", true),
        (r"[[=a=]s]3.env", "s3.env", true),
        (r"[[=a=]]", "b", false),
        (r"[x[=a=]]", "x", true),
        (r"[x[=a]", "x", false),
        (r"[[=a]", "[", true),
        (r"[[=a]", "a", true),
        (r"[[=", "[", false),
        (r"[x[=", "x", false),
        (r"[[=a=", "a", false),
        (r"[[.s.]]3.env", "s3.env", true),
        (r"[[.ab.]]", "a", false),
        (r"[[.a.]-z]", "s", true),
        (r"[a-[.z.]]", "s", true),
        (r"[a-[.zz.]]", "s", false),
        (r"[a-[.z", "s", false),
        (r"[[.a", "a", false),
        (r"[[.a.]-]", "a", false),
        (r"[[.a.]-]", "-", true),
        (r"[x[.a.]]", "x", true),
        (r"[x[.a", "x", false),
        (r"[\[.a.]]", "[", false),
        (r"[[.a.]]", "b", false),
        (r"[x\]]", "x", true),
        (r"[x\", "x", false),
        (r"[ab", "[ab", true),
        (r"[ab", "a", false),
        (r"[a-cs]3.env", "s3.env", true),
        (r"[!a-c]3.env", "s3.env", true),
        (r"[.]env", ".env", false),
        (r"x[.]", "x.", true),
        (r"s[3]", "s", false),
        (r"*[", "a[", true),
        (r"*[", "a", false),
        (r"[!x", "[!x", true),
        (r"[!x", "a", false),
        (r"[[:alpha:]-z]", "b", true),
        (r"[x-[:alpha:]]", "-", false),
        (r"[[:alpha:]]", "", false),
        (r"[s]", "", false),
        (r"\s", "s", true),
        (r"\s", "", false),
        (r"a\*", "a*", true),
        (r"a\*", "ab", false),
        (r"[[:zz:]]", "[]", true),
        (r"[[:zz:]]", "z]", true),
        (r"[x[:z:]]", "x]", true),
        (r"[[:az:]]", "[]", true),
        (r"s3?env", "s3.env", true),
        (r"[[:a:b:]]", "[]", true),
        (r"[[:a:b:]]", "a", false),
        (r"[[.a.b.]]", "a", false),
        (r"[[.a.b.]]", "b", false),
        (r"[x[:a:b]", "x", true),
        (r"[x[:a:b]", ":", true),
        (r"[x[=a=b]", "x", false),
        (r"[x[]", "x", true),
        (r"[x[]", "x]", false),
        (r"[x[.a.b.]]", "x", true),
        (r"[x[.a.b.]]", "x]", false),
    ] {
        assert_eq!(
            fnmatch(pattern.as_bytes(), name.as_bytes()),
            want,
            "{pattern} {name}"
        );
    }
    // glibc's bound on a class name (2048 bytes, the `:` counted when skipping).
    let long = |n: usize| "a".repeat(n);
    assert!(fnmatch(format!("[[:{}A]", long(2047)).as_bytes(), b"["));
    assert!(!fnmatch(format!("[[:{}A]", long(2048)).as_bytes(), b"["));
    assert!(fnmatch(format!("[x[:{}:]]", long(2046)).as_bytes(), b"x"));
    assert!(!fnmatch(format!("[x[:{}:]]", long(2047)).as_bytes(), b"x"));
}

/// Catches a C-locale class with another membership than glibc's (the bytes each
/// class matches, as glibc 2.42 answered for every byte from 1 to 255), or a byte past
/// ASCII taken for a letter.
#[test]
fn classes_hold_the_c_locale_bytes() {
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const DIGIT: &[u8] = b"0123456789";
    const PUNCT: &[u8] = b"!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
    let any = |sets: &[&[u8]], b: u8| sets.iter().any(|s| s.contains(&b));
    let rows: [(&str, &dyn Fn(u8) -> bool); 12] = [
        ("alnum", &|b| any(&[UPPER, LOWER, DIGIT], b)),
        ("alpha", &|b| any(&[UPPER, LOWER], b)),
        ("blank", &|b| any(&[b"\t "], b)),
        ("cntrl", &|b| b < 32 || b == 127),
        ("digit", &|b| any(&[DIGIT], b)),
        ("graph", &|b| any(&[UPPER, LOWER, DIGIT, PUNCT], b)),
        ("lower", &|b| any(&[LOWER], b)),
        ("print", &|b| any(&[UPPER, LOWER, DIGIT, PUNCT, b" "], b)),
        ("punct", &|b| any(&[PUNCT], b)),
        ("space", &|b| any(&[b"\t\n\x0b\x0c\r "], b)),
        ("upper", &|b| any(&[UPPER], b)),
        ("xdigit", &|b| any(&[DIGIT, b"ABCDEFabcdef"], b)),
    ];
    for (name, want) in rows {
        let pattern = format!("[[:{name}:]]");
        for b in 1..=255u8 {
            assert_eq!(
                matched(pattern.as_bytes(), &[b], false),
                want(b),
                "{name} {b}"
            );
        }
    }
}

/// Catches a path pattern taken apart other than glob(3) does: an odd `\` before a `/`
/// kept (or an even one dropped), `\` not taken off a directory written out, `.`,
/// `..` and `//` not resolved, a part after the first wildcard not matched by fnmatch
/// (or one with no `\` not opened as written), `[`/`]` across a `/`, and a pattern for
/// a directory or with too few or too many parts matching the file; and a trailing `/`
/// read other than glob(3) reads it (a plain name before it listed even as a file, a
/// wildcard or `\` one only as a directory, an odd `\` before it dropped, a last `.`).
#[test]
fn paths_match_as_glibc_glob_lists_them() {
    for (pattern, want) in [
        (r"/etc/kbf/s3.env", true),
        (r"/etc/kbf\/s3.env", true),
        (r"/etc/kbf\\/s3.env", false),
        (r"/etc/k\bf/s3.env", true),
        (r"/etc/other/../kbf/s3.env", true),
        (r"/etc/./kbf/s3.env", true),
        (r"/etc/*/s3.env", true),
        (r"/etc/*/../kbf/s3.env", true),
        (r"/e*/kbf/s3.env", true),
        (r"/e*/k\bf/s3.env", true),
        (r"/e*//kbf/s3.env", true),
        (r"/etc[/]kbf/s3.env", false),
        (r"/etc/kbf/[[:alpha:]]3.env", true),
        (r"/etc/kbf/\s3.env", true),
        (r"/etc/kbf/s3.env\", false),
        (r"/etc/kbf/", false),
        (r"/", false),
        (r"/*/*/*", true),
        (r"/*/*", false),
        (r"/etc/[k]bf/s3.env", true),
        (r"/etc/kbf[/s3.env", false),
        (r"/e[t]c/kbf\/s3.env", true),
        (r"/etc/*/\/s3.env", true),
        (r"/etc/*/./s3.env", true),
        (r"/e[t]c/./kbf/s3.env", true),
        (r"/e*/other/../kbf/s3.env", true),
        (r"/etc/kbf/.", false),
        (r"/[!.]tc/kbf/s3.env", true),
        (r"/\etc/kbf/s3.env", true),
        (r"/e\[t]c/kbf/s3.env", false),
        (r"/e[t]c/k[/s3.env", false),
        // From systemd's sources, not glibc alone: its readdir skips `.` and `..`
        // (`readdir_no_dot` in `src/basic/glob-util.c`), so a `\.` past a wildcard,
        // which glob(3) reads the directory for, matches nothing.
        (r"/e*/\./kbf/s3.env", false),
        // glob(3) needs a directory under `kbf/` for the `*`; the rule reads `..` by its
        // words, as `normalise` does before it.
        (r"/etc/kbf/*/../s3.env", true),
        // systemd globs only an absolute path.
        (r"etc/kbf/s3.env", false),
        // A trailing `/`, from glibc's `posix/glob.c` (the `"pattern/"` case and
        // `glob_in_dir`): `pattern` is expanded with GLOB_ONLYDIR, which only the
        // directory read honours. A last name with no wildcard and no `\` is lstat'ed
        // and kept even as a file; one with either is matched against the directory's
        // entries and a file skipped. An odd run of `\` before the `/` loses one.
        // glibc 2.39 (glob(3) with no flags, CI's x86_64 and arm64 runners, a
        // throwaway probe) listed exactly these.
        (r"/etc/kbf/s3.env/", true),
        (r"/etc/kbf/s3.env//", true),
        (r"/etc/*/s3.env/", true),
        (r"/etc/kbf/s3.env\/", true),
        (r"/etc/kbf/s3.env\\/", false),
        (r"/etc/kbf/\s3.env/", false),
        (r"/etc/kbf/s3*/", false),
        (r"/etc/kbf/s3.env/.", false),
        (r"/etc/kbf/s3.env/./", false),
        (r"/etc/kbf/s3.env/..", false),
    ] {
        assert_eq!(matches(pattern, KEY), want, "{pattern}");
    }
}

/// Catches a wildcard told from a path by another test than glob(3)'s (`[` alone counts,
/// an escaped `*` does not), and the path a literal names read with its `\` kept, or a
/// name ending in a lone `\` read as a file; a trailing `/` kept, or a pattern that
/// lists only a directory (a last `.` or `..`, a `\` name before a `/`) read as a file.
#[test]
fn a_path_with_no_wildcard_names_its_file_unescaped() {
    for (text, wildcard) in [
        ("/etc/kbf/s3.env", false),
        ("/etc/kbf/s?.env", true),
        ("/etc/kbf/[s", true),
        (r"/etc/kbf/\*.env", false),
        (r"/etc/kbf/\[s]", false),
    ] {
        assert_eq!(is_wildcard(text), wildcard, "{text}");
    }
    assert_eq!(literal(r"/etc/k\bf\/s3.\env").as_deref(), Some(KEY));
    assert_eq!(
        literal(r"/etc/kbf\\/s3.env").as_deref(),
        Some(r"/etc/kbf\/s3.env")
    );
    assert_eq!(literal(r"s3\.env").as_deref(), Some("s3.env"));
    assert_eq!(
        literal(r"/etc/kbf/s3.env\\").as_deref(),
        Some(r"/etc/kbf/s3.env\")
    );
    assert_eq!(literal(r"/etc/kbf/s3.env\"), None);
    assert_eq!(literal("/etc/*/s3.env"), None);
    // A trailing `/` is taken off (with the last `\` of an odd run before it); a name
    // with a `\` before it, or a last `.` or `..`, lists a directory only.
    assert_eq!(literal("/etc/kbf/s3.env//").as_deref(), Some(KEY));
    assert_eq!(literal(r"/etc/kbf/s3.env\/").as_deref(), Some(KEY));
    assert_eq!(literal(r"/etc/kbf/\s3.env/"), None);
    assert_eq!(literal("/etc/kbf/s3.env/."), None);
    assert_eq!(literal("/etc/kbf/s3.env/.."), None);
    assert_eq!(literal(r"/etc/kbf/s3.env/\."), None);
    assert_eq!(literal("."), None);
    assert_eq!(literal("/").as_deref(), Some("/"));
}

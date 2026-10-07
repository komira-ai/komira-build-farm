//! Public-hygiene scan for repository text: no machine addresses and no home paths.
//!
//! Everything in this repository is public, so a text file must not carry:
//! - an IPv4 address, except the documentation ranges (192.0.2.0/24, 198.51.100.0/24,
//!   203.0.113.0/24, RFC 5737), `127.0.0.1` and `0.0.0.0`; addresses in the private
//!   ranges (10/8, 172.16/12, 192.168/16, RFC 1918) are reported as such;
//! - an absolute home path such as `/home/<user>` or `/Users/<user>`.
//!
//! A dotted run is read as an address only when it has exactly four parts of one to
//! three digits, each at most 255, so `1.2.3` (a version) and `1.2.3.4.5` are not
//! addresses.
//!
//! [`decode`] turns a file's bytes into the text to scan, so that no file escapes the
//! scan by its encoding: UTF-16 with a byte order mark is decoded, and a file holding
//! a NUL byte is refused unless its extension is on [`BINARY_EXTENSIONS`].

use std::fmt;

/// What a finding is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// An address in an RFC 1918 private range.
    PrivateIpv4([u8; 4]),
    /// Any other address outside the allowed set.
    Ipv4([u8; 4]),
    /// An absolute home path; holds the matched prefix and user name.
    HomePath(String),
}

/// One finding: a 1-based line number and what was found there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub kind: Kind,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dotted = |a: &[u8; 4]| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
        match &self.kind {
            Kind::PrivateIpv4(a) => {
                write!(f, "line {}: private IPv4 address {}", self.line, dotted(a))
            }
            Kind::Ipv4(a) => write!(f, "line {}: IPv4 address {}", self.line, dotted(a)),
            Kind::HomePath(p) => write!(f, "line {}: home path {p}", self.line),
        }
    }
}

/// True for an RFC 1918 private address.
pub fn is_private(a: [u8; 4]) -> bool {
    a[0] == 10 || (a[0] == 172 && (16..=31).contains(&a[1])) || (a[0] == 192 && a[1] == 168)
}

/// True for an address a public file may carry: the RFC 5737 documentation ranges,
/// `127.0.0.1` and `0.0.0.0`.
pub fn is_allowed(a: [u8; 4]) -> bool {
    matches!(
        (a[0], a[1], a[2]),
        (192, 0, 2) | (198, 51, 100) | (203, 0, 113)
    ) || a == [127, 0, 0, 1]
        || a == [0, 0, 0, 0]
}

/// Extensions (compared ignoring ASCII case) of files that may hold a NUL byte; the
/// scan skips such a file as binary. Any other file holding a NUL byte is refused.
pub const BINARY_EXTENSIONS: &[&str] = &[
    "gif", "gz", "ico", "jpeg", "jpg", "pdf", "png", "wasm", "webp", "zip", "zst",
];

/// The text of the file at `path` (relative, used for its extension) to scan:
/// `Ok(None)` for a binary file on [`BINARY_EXTENSIONS`], an error for a file the scan
/// cannot read. A file that starts with a UTF-16 byte order mark is decoded as UTF-16
/// (and refused if that fails or yields a NUL); any other file holding a NUL byte is
/// refused unless its extension is on the allow list; the rest is read as UTF-8, with
/// invalid bytes replaced.
pub fn decode(path: &str, bytes: &[u8]) -> Result<Option<String>, String> {
    let utf16 = match bytes {
        [0xFF, 0xFE, rest @ ..] => Some((rest, u16::from_le_bytes as fn([u8; 2]) -> u16)),
        [0xFE, 0xFF, rest @ ..] => Some((rest, u16::from_be_bytes as fn([u8; 2]) -> u16)),
        _ => None,
    };
    if let Some((rest, unit)) = utf16 {
        if rest.len() % 2 != 0 {
            return Err("a UTF-16 file with an odd number of bytes is not read".to_owned());
        }
        let units = rest.chunks_exact(2).map(|c| unit([c[0], c[1]]));
        let text: String = char::decode_utf16(units)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("not valid UTF-16 ({e}), so it is not read"))?;
        if text.contains('\0') {
            return Err("a UTF-16 file holding a NUL character is not read".to_owned());
        }
        return Ok(Some(text));
    }
    if bytes.contains(&0) {
        let name = path.rsplit('/').next().unwrap_or(path);
        let binary = name.rsplit_once('.').is_some_and(|(_, ext)| {
            BINARY_EXTENSIONS
                .iter()
                .any(|b| ext.eq_ignore_ascii_case(b))
        });
        return if binary {
            Ok(None)
        } else {
            Err("holds a NUL byte, and its extension is not on the binary allow list".to_owned())
        };
    }
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

/// Scans one text and returns every finding, in line order.
pub fn scan(text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for a in ipv4_in(line) {
            if is_allowed(a) {
                continue;
            }
            let kind = if is_private(a) {
                Kind::PrivateIpv4(a)
            } else {
                Kind::Ipv4(a)
            };
            out.push(Finding { line: i + 1, kind });
        }
        for p in home_paths_in(line) {
            out.push(Finding {
                line: i + 1,
                kind: Kind::HomePath(p),
            });
        }
    }
    out
}

/// Every four-part dotted decimal in `line`.
fn ipv4_in(line: &str) -> Vec<[u8; 4]> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        // A trailing dot ends a sentence, it is not part of the run.
        let run = line[start..i].trim_end_matches('.');
        if let Some(a) = parse_ipv4(run) {
            out.push(a);
        }
    }
    out
}

fn parse_ipv4(run: &str) -> Option<[u8; 4]> {
    let mut a = [0u8; 4];
    let mut parts = run.split('.');
    for slot in &mut a {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(a)
}

/// Every `/home/<name>` or `/Users/<name>` in `line` that starts a path (the prefix is
/// at the start of the line or follows a character that cannot be part of a name).
fn home_paths_in(line: &str) -> Vec<String> {
    let name_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.';
    let b = line.as_bytes();
    let mut out = Vec::new();
    for prefix in ["/home/", "/Users/"] {
        for (at, _) in line.match_indices(prefix) {
            if at > 0 && name_char(b[at - 1]) {
                continue;
            }
            let rest = &b[at + prefix.len()..];
            let n = rest.iter().take_while(|&&c| name_char(c)).count();
            if n > 0 && rest[0] != b'.' {
                out.push(line[at..at + prefix.len() + n].to_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The planted values are assembled at run time so this file does not trip the scan.
    fn ip(a: u8, rest: &str) -> String {
        format!("{a}.{rest}")
    }

    #[test]
    fn private_ranges_are_reported_as_private() {
        // Catches: a private-range table that misses a range or a boundary.
        let planted = [
            ip(10, "1.2.3"),
            ip(172, "16.0.1"),
            ip(172, "31.255.255"),
            ip(192, "168.1.1"),
        ];
        for s in planted {
            let f = scan(&format!("host {s} here"));
            assert_eq!(f.len(), 1, "{s}");
            assert!(matches!(f[0].kind, Kind::PrivateIpv4(_)), "{s}");
        }
        let f = scan(&ip(172, "32.0.1"));
        assert!(
            matches!(f[0].kind, Kind::Ipv4(_)),
            "172.32/16 is not RFC 1918"
        );
    }

    #[test]
    fn public_addresses_are_reported() {
        // Catches: a scan that only looks at private ranges.
        assert_eq!(scan(&ip(8, "8.8.8")).len(), 1);
        assert_eq!(
            scan(&ip(127, "0.0.2")).len(),
            1,
            "only 127.0.0.1 is allowed"
        );
    }

    #[test]
    fn documentation_and_loopback_addresses_are_allowed() {
        // Catches: an allow list that drops a documentation range, loopback or 0.0.0.0.
        let text = "192.0.2.1 198.51.100.7 203.0.113.255 127.0.0.1 0.0.0.0";
        assert_eq!(scan(text), vec![]);
    }

    #[test]
    fn versions_and_long_runs_are_not_addresses() {
        // Catches: a matcher that reads versions, OIDs or out-of-range parts as addresses.
        let text = "version 1.95.0, oid 1.3.6.1.4.1, 300.1.2.3, 1..2.3";
        assert_eq!(scan(text), vec![]);
    }

    #[test]
    fn address_at_sentence_end_and_line_numbers() {
        // Catches: a trailing full stop hiding an address, or off-by-one line numbers.
        let text = format!("ok\nreach it at {}.", ip(10, "0.0.9"));
        let f = scan(&text);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].line, 2);
    }

    #[test]
    fn home_paths_are_reported() {
        // Catches: a scan that misses either home prefix or a path inside a URL.
        let home = format!("/{}/alice/x", "home");
        let users = format!("file:///{}/bob", "Users");
        let f = scan(&format!("{home} and {users}"));
        let got: Vec<_> = f.iter().map(|x| x.kind.clone()).collect();
        assert_eq!(
            got,
            vec![
                Kind::HomePath(format!("/{}/alice", "home")),
                Kind::HomePath(format!("/{}/bob", "Users")),
            ]
        );
    }

    fn utf16(text: &str, le: bool) -> Vec<u8> {
        let mut out = if le {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for u in text.encode_utf16() {
            out.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    #[test]
    fn utf16_files_are_decoded_and_scanned() {
        // Catches: a UTF-16 file skipped as binary (its ASCII holds NUL bytes), which
        // hid an address from the scan; and a decoder that ignores the byte order.
        let planted = format!("host {}", ip(10, "1.2.3"));
        for le in [true, false] {
            let text = decode("notes.txt", &utf16(&planted, le))
                .expect("UTF-16 is read")
                .expect("UTF-16 is text");
            assert_eq!(text, planted, "le={le}");
            assert_eq!(scan(&text).len(), 1, "le={le}");
        }
        assert!(decode("a.txt", &[0xFF, 0xFE, b'a']).is_err(), "odd length");
        assert!(
            decode("a.txt", &[0xFF, 0xFE, 0x00, 0xD8]).is_err(),
            "lone surrogate"
        );
        // UTF-32LE starts like UTF-16LE and decodes to NUL characters.
        assert!(
            decode("a.txt", &[0xFF, 0xFE, 0, 0, b'a', 0, 0, 0]).is_err(),
            "UTF-32"
        );
    }

    #[test]
    fn nul_bytes_are_refused_unless_the_extension_is_binary() {
        // Catches: any file holding a NUL skipped as binary, so a text file with one
        // planted NUL escaped the scan.
        let bytes = b"host\0 text";
        for path in ["a.txt", "Makefile", "dir.png/Makefile", "a.png.txt"] {
            assert!(decode(path, bytes).is_err(), "{path}");
        }
        for path in ["logo.png", "docs/x.PDF", "a.tar.gz"] {
            assert_eq!(decode(path, bytes), Ok(None), "{path}");
        }
        assert_eq!(decode("a.txt", b"plain"), Ok(Some("plain".to_owned())));
    }

    #[test]
    fn placeholders_and_relative_paths_are_not_home_paths() {
        // Catches: false positives on a placeholder, a bare prefix or a relative path.
        let text = "/home/<user> /Users/ src/home/x ./home/x";
        assert_eq!(scan(text), vec![]);
    }
}

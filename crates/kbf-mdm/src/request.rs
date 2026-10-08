//! The operator's erase request (M4.2): the text an operator signs with
//! `ssh-keygen -Y sign -n kbf-mdm-erase`, and its parser.
//!
//! ```text
//! kbf-erase-v1
//! serial <serial>
//! purpose erase-now | privileged-lease <lease id>
//! reason <free text>
//! nonce <128 random bits, hex>
//! not-after <RFC 3339 time>
//! ```
//!
//! The parser is strict: those six lines in that order, each once, and at most one
//! final newline. The gate parses only bytes whose signature it has verified.

use crate::clock::parse_rfc3339;

/// The first line of every request.
pub const MAGIC: &str = "kbf-erase-v1";

/// What the signed request asks for. The gate acts on this, never on the server's word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Erase the Mac now (within the caps), or refuse; never held.
    EraseNow,
    /// Hold the request for `grant-admin` of this lease.
    PrivilegedLease(String),
}

/// A parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EraseRequest {
    pub serial: String,
    pub purpose: Purpose,
    pub reason: String,
    /// 32 lowercase hex digits.
    pub nonce: String,
    /// Seconds since the epoch.
    pub not_after: i64,
}

/// Why a request's text is not a request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("the request is not {MAGIC} text")]
    Magic,
    #[error("line {line}: expected `{field} <value>`")]
    Field { line: usize, field: &'static str },
    #[error("bad serial (1 to 32 letters and digits)")]
    Serial,
    #[error("bad purpose (erase-now or privileged-lease <lease id>)")]
    Purpose,
    #[error("bad reason (1 to 512 printable characters)")]
    Reason,
    #[error("bad nonce (32 lowercase hex digits)")]
    Nonce,
    #[error("bad not-after (an RFC 3339 time)")]
    NotAfter,
    #[error("the request has more than six lines")]
    Trailing,
}

/// Whether `serial` looks like a Mac serial: 1 to 32 ASCII letters and digits.
pub fn valid_serial(serial: &str) -> bool {
    (1..=32).contains(&serial.len()) && serial.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether `lease` is a lease id the gate accepts: 1 to 128 of `[A-Za-z0-9._-]`.
pub fn valid_lease(lease: &str) -> bool {
    (1..=128).contains(&lease.len())
        && lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn field<'a>(
    lines: &mut impl Iterator<Item = (usize, &'a str)>,
    name: &'static str,
) -> Result<&'a str, RequestError> {
    let (index, line) = lines.next().ok_or(RequestError::Field {
        line: 0,
        field: name,
    })?;
    line.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or(RequestError::Field {
            line: index + 1,
            field: name,
        })
}

fn purpose(text: &str) -> Result<Purpose, RequestError> {
    if text == "erase-now" {
        return Ok(Purpose::EraseNow);
    }
    match text.strip_prefix("privileged-lease ") {
        Some(lease) if valid_lease(lease) => Ok(Purpose::PrivilegedLease(lease.to_owned())),
        _ => Err(RequestError::Purpose),
    }
}

/// Parses a request's text.
///
/// # Errors
/// The text is not exactly one well-formed request.
pub fn parse(text: &str) -> Result<EraseRequest, RequestError> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut lines = body.split('\n').enumerate();
    if lines.next().map(|(_, l)| l) != Some(MAGIC) {
        return Err(RequestError::Magic);
    }
    let serial = field(&mut lines, "serial")?;
    if !valid_serial(serial) {
        return Err(RequestError::Serial);
    }
    let purpose = purpose(field(&mut lines, "purpose")?)?;
    let reason = field(&mut lines, "reason")?;
    if reason.is_empty() || reason.chars().count() > 512 || reason.chars().any(char::is_control) {
        return Err(RequestError::Reason);
    }
    let nonce = field(&mut lines, "nonce")?;
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(RequestError::Nonce);
    }
    let not_after = parse_rfc3339(field(&mut lines, "not-after")?).ok_or(RequestError::NotAfter)?;
    if lines.next().is_some() {
        return Err(RequestError::Trailing);
    }
    Ok(EraseRequest {
        serial: serial.to_owned(),
        purpose,
        reason: reason.to_owned(),
        nonce: nonce.to_owned(),
        not_after,
    })
}

/// Writes a request's text (what `kbf-admin erase` signs); the inverse of [`parse`].
pub fn render(request: &EraseRequest) -> String {
    let purpose = match &request.purpose {
        Purpose::EraseNow => "erase-now".to_owned(),
        Purpose::PrivilegedLease(lease) => format!("privileged-lease {lease}"),
    };
    format!(
        "{MAGIC}\nserial {}\npurpose {purpose}\nreason {}\nnonce {}\nnot-after {}\n",
        request.serial,
        request.reason,
        request.nonce,
        crate::clock::format_rfc3339(request.not_after),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(purpose: Purpose) -> EraseRequest {
        EraseRequest {
            serial: "C02ABC123XYZ".into(),
            purpose,
            reason: "leak after reboot".into(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            not_after: 1_800_000_000,
        }
    }

    #[test]
    fn render_and_parse_round_trip() {
        for purpose in [
            Purpose::EraseNow,
            Purpose::PrivilegedLease("lease-7.a_b".into()),
        ] {
            let request = sample(purpose);
            let text = render(&request);
            assert_eq!(parse(&text), Ok(request.clone()));
            // Without the final newline too.
            assert_eq!(parse(text.trim_end_matches('\n')), Ok(request));
        }
    }

    fn with_line(index: usize, line: &str) -> String {
        let text = render(&sample(Purpose::EraseNow));
        let mut lines: Vec<&str> = text.lines().collect();
        lines[index] = line;
        lines.join("\n")
    }

    #[test]
    fn each_malformed_field_is_refused() {
        // Catches: a parser that skips a field's check, so a request the operator did
        // not mean (another purpose, no nonce) would pass as signed.
        assert_eq!(parse("kbf-erase-v2\n"), Err(RequestError::Magic));
        assert_eq!(parse(""), Err(RequestError::Magic));
        assert_eq!(
            parse(&with_line(1, "serial")),
            Err(RequestError::Field {
                line: 2,
                field: "serial"
            })
        );
        assert_eq!(
            parse("kbf-erase-v1\nserial ABC"),
            Err(RequestError::Field {
                line: 0,
                field: "purpose"
            })
        );
        assert_eq!(
            parse(&with_line(1, "serial ABC-1")),
            Err(RequestError::Serial)
        );
        assert_eq!(parse(&with_line(1, "serial ")), Err(RequestError::Serial));
        assert_eq!(
            parse(&with_line(2, "purpose erase-later")),
            Err(RequestError::Purpose)
        );
        assert_eq!(
            parse(&with_line(2, "purpose privileged-lease bad/lease")),
            Err(RequestError::Purpose)
        );
        assert_eq!(parse(&with_line(3, "reason ")), Err(RequestError::Reason));
        assert_eq!(
            parse(&with_line(3, "reason a\tb")),
            Err(RequestError::Reason)
        );
        let long = format!("reason {}", "x".repeat(513));
        assert_eq!(parse(&with_line(3, &long)), Err(RequestError::Reason));
        assert_eq!(parse(&with_line(4, "nonce 0123")), Err(RequestError::Nonce));
        assert_eq!(
            parse(&with_line(4, "nonce 0123456789ABCDEF0123456789abcdef")),
            Err(RequestError::Nonce)
        );
        assert_eq!(
            parse(&with_line(5, "not-after tomorrow")),
            Err(RequestError::NotAfter)
        );
        let mut extra = render(&sample(Purpose::EraseNow));
        extra.push_str("serial OTHER\n");
        assert_eq!(parse(&extra), Err(RequestError::Trailing));
    }

    #[test]
    fn lease_ids_are_limited() {
        assert!(valid_lease("a"));
        assert!(!valid_lease(""));
        assert!(!valid_lease(&"a".repeat(129)));
        assert!(!valid_lease("a b"));
    }
}

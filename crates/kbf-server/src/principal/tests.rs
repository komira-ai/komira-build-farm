use super::*;

const TOKEN_A: &str = "kbf-test-token-a-0123456789abcdef0123456789";
const TOKEN_B: &str = "kbf-test-token-b-0123456789abcdef0123456789";

fn line(principal: &str, qos: &str, token: &str) -> String {
    let qos: Qos = qos.parse().expect("a built-in level");
    token_line(principal, ClientRole::Client, &qos, token.as_bytes()).expect("a line")
}

fn hex_of(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Catches: a field dropped or reordered in the line, the role or QoS not read, or
/// comments and blank lines refused.
#[test]
fn a_file_parses_to_its_entries() {
    let text = format!(
        "# who may call\n\n{}\n  {}   # rotated in\n\t\n",
        line("ci-main", "ci", TOKEN_A),
        line("dev@box-1", "interactive", TOKEN_B),
    );
    let principals = Principals::parse(&text).expect("parses");
    assert_eq!(principals.len(), 2);
    let a = principals
        .admit(format!("Bearer {TOKEN_A}").as_bytes())
        .expect("A");
    assert_eq!(
        (a.name(), a.role(), a.qos()),
        ("ci-main", ClientRole::Client, &Qos::Ci)
    );
    let b = principals
        .admit(format!("Bearer {TOKEN_B}").as_bytes())
        .expect("B");
    assert_eq!((b.name(), b.qos()), ("dev@box-1", &Qos::Interactive));
}

/// Catches: upper-case hex refused (operators paste digests from other tools).
#[test]
fn a_digest_in_upper_case_parses() {
    let text = format!(
        "ci client batch sha256:{}",
        hex_of(TOKEN_A).to_ascii_uppercase()
    );
    let principals = Principals::parse(&text).expect("parses");
    let got = principals.admit(format!("Bearer {TOKEN_A}").as_bytes());
    assert_eq!(got.map(|p| p.qos().clone()), Some(Qos::Batch));
}

/// Catches: a malformed line skipped instead of refused (a typo would silently drop
/// a principal, or worse admit a half-read one), and the reported line number off.
#[test]
fn a_bad_line_is_refused_with_its_number() {
    let digest = format!("sha256:{}", hex_of(TOKEN_A));
    let short = format!("sha256:{}", &hex_of(TOKEN_A)[..63]);
    let long = format!("sha256:{}0", hex_of(TOKEN_A));
    let not_hex = format!("sha256:{}g", &hex_of(TOKEN_A)[..63]);
    let not_ascii = format!("sha256:{}é", &hex_of(TOKEN_A)[..62]);
    let sha1 = format!("sha1:{}", hex_of(TOKEN_A));
    let too_long_name = "p".repeat(MAX_PRINCIPAL_BYTES + 1);
    let cases = [
        format!("ci client {digest}"),
        format!("ci client ci {digest} extra"),
        format!("ci admin ci {digest}"),
        format!("ci client urgent {digest}"),
        format!("c/i client ci {digest}"),
        format!("{too_long_name} client ci {digest}"),
        format!("ci client ci {short}"),
        format!("ci client ci {long}"),
        format!("ci client ci {not_hex}"),
        format!("ci client ci {not_ascii}"),
        format!("ci client ci {sha1}"),
        format!("ci client ci {}", hex_of(TOKEN_A)),
    ];
    for bad in cases {
        let text = format!("# header\n{}\n{bad}\n", line("ok", "ci", TOKEN_B));
        let refused = Principals::parse(&text);
        let Err((at, why)) = refused else {
            panic!("{bad:?} parsed");
        };
        assert_eq!(at, 3, "{bad:?}: {why}");
    }
    let exact = "p".repeat(MAX_PRINCIPAL_BYTES);
    assert!(Principals::parse(&format!("{exact} client ci {digest}")).is_ok());
}

/// Catches: two principals sharing a token (which one a call is would depend on the
/// order of lines), while one principal with two tokens (a rotation) stays allowed;
/// and the duplicate error echoing the shared digest, in hex or as bytes, or the
/// token (it would reach the server's log when the file is refused).
#[test]
fn a_digest_names_one_principal() {
    let twice = format!(
        "{}\n{}\n",
        line("a", "ci", TOKEN_A),
        line("b", "ci", TOKEN_A)
    );
    let Err((at, why)) = Principals::parse(&twice) else {
        panic!("a shared digest parsed");
    };
    assert_eq!(at, 2);
    assert!(why.contains("entry 1"), "{why}");
    let hex = hex_of(TOKEN_A);
    let bytes = format!("{:?}", &Sha256::digest(TOKEN_A.as_bytes())[..4]);
    assert!(
        !why.contains(&hex[..16])
            && !why.contains(&hex[48..])
            && !why.contains(&hex[..16].to_ascii_uppercase())
            && !why.contains(&bytes[1..bytes.len() - 1])
            && !why.contains(&TOKEN_A[..16]),
        "{why}"
    );
    let rotating = format!(
        "{}\n{}\n",
        line("a", "ci", TOKEN_A),
        line("a", "ci", TOKEN_B)
    );
    let principals = Principals::parse(&rotating).expect("one principal, two tokens");
    for token in [TOKEN_A, TOKEN_B] {
        let got = principals.admit(format!("Bearer {token}").as_bytes());
        assert_eq!(got.map(Principal::name), Some("a"));
    }
}

/// Catches: a comparison that stops at a prefix, ignores a trailing byte, accepts
/// another scheme or no scheme, or matches an empty token.
#[test]
fn only_the_exact_token_as_bearer_is_admitted() {
    let principals = Principals::parse(&line("ci", "ci", TOKEN_A)).expect("parses");
    for ok in [
        format!("Bearer {TOKEN_A}"),
        format!("bearer {TOKEN_A}"),
        format!("BEARER   {TOKEN_A}"),
    ] {
        assert!(principals.admit(ok.as_bytes()).is_some(), "{ok:?}");
    }
    let prefix = &TOKEN_A[..TOKEN_A.len() - 1];
    for refused in [
        String::new(),
        "Bearer".to_owned(),
        "Bearer ".to_owned(),
        TOKEN_A.to_owned(),
        format!("Basic {TOKEN_A}"),
        format!("Token {TOKEN_A}"),
        format!("Bearer {prefix}"),
        format!("Bearer {TOKEN_A}x"),
        format!("Bearer {TOKEN_A} "),
        format!("Bearer {TOKEN_A}\n"),
        format!("Bearer {TOKEN_B}"),
        format!("Bearer sha256:{}", hex_of(TOKEN_A)),
    ] {
        assert!(
            principals.admit(refused.as_bytes()).is_none(),
            "{refused:?}"
        );
    }
}

/// Catches: an entry other than the first never admitted (a scan that stops early or
/// keeps only the first entry), and the last entry dropped.
#[test]
fn every_entry_is_scanned() {
    let tokens: Vec<String> = (0..5).map(|i| format!("{TOKEN_A}-{i}")).collect();
    let text: String = tokens
        .iter()
        .enumerate()
        .map(|(i, t)| line(&format!("p{i}"), "ci", t) + "\n")
        .collect();
    let principals = Principals::parse(&text).expect("parses");
    for (i, t) in tokens.iter().enumerate() {
        let got = principals.admit(format!("Bearer {t}").as_bytes());
        assert_eq!(got.map(Principal::name), Some(format!("p{i}").as_str()));
    }
}

/// Catches: a digest (or any byte of one) shown by `Debug`, which would reach a log
/// through any `{:?}` of a value holding it.
#[test]
fn debug_shows_no_digest() {
    let text = format!(
        "{}\n{}\n",
        line("ci", "ci", TOKEN_A),
        line("dev", "batch", TOKEN_B)
    );
    let principals = Principals::parse(&text).expect("parses");
    let shown = format!("{principals:?} {:#?}", principals.entries[0]);
    assert!(
        shown.contains("\"ci\"") && shown.contains("\"batch\""),
        "{shown}"
    );
    for token in [TOKEN_A, TOKEN_B] {
        let hex = hex_of(token);
        assert!(!shown.contains(&hex[..16]), "{shown}");
        assert!(!shown.contains(&hex[48..]), "{shown}");
        assert!(!shown.contains(token), "{shown}");
        let bytes = format!("{:?}", &Sha256::digest(token.as_bytes())[..4]);
        assert!(!shown.contains(&bytes[1..bytes.len() - 1]), "{shown}");
    }
}

/// Catches: a parse error that echoes the digest field (it would reach the server's
/// log when the file is refused).
#[test]
fn a_parse_error_does_not_echo_the_digest() {
    let hex = hex_of(TOKEN_A);
    for bad in [
        format!("ci client ci sha256:{}", &hex[..63]),
        format!("ci client ci sha512:{hex}"),
    ] {
        let Err((_, why)) = Principals::parse(&bad) else {
            panic!("{bad:?} parsed");
        };
        assert!(!why.contains(&hex[..16]), "{why}");
    }
}

/// Catches: a name, role or qos error that echoes its field. A line with its fields
/// out of order puts a digest there, and a raw token pasted into a column puts the
/// token there; either would reach the server's log when the file is refused.
#[test]
fn a_line_with_its_fields_out_of_order_echoes_none_of_them() {
    let hex = hex_of(TOKEN_A);
    for bad in [
        format!("sha256:{hex} client ci ci"),
        format!("ci sha256:{hex} client ci"),
        format!("ci {TOKEN_A} ci sha256:{hex}"),
        format!("ci client sha256:{hex} ci"),
        format!("ci client {TOKEN_A} sha256:{hex}"),
    ] {
        let Err((at, why)) = Principals::parse(&bad) else {
            panic!("{bad:?} parsed");
        };
        assert_eq!(at, 1, "{why}");
        assert!(
            !why.contains(&hex[..16]) && !why.contains(&TOKEN_A[..16]),
            "{bad:?}: {why}"
        );
    }
}

/// Catches: the hash-token line not in the file's format, the token hashed with its
/// trailing newline, or a short or spaced token or a bad name accepted.
#[test]
fn a_token_line_round_trips() {
    let qos = Qos::Interactive;
    let made = token_line(
        "dev",
        ClientRole::Client,
        &qos,
        format!("{TOKEN_A}\n").as_bytes(),
    )
    .expect("a line");
    assert_eq!(
        made,
        format!("dev client interactive sha256:{}", hex_of(TOKEN_A))
    );
    let principals = Principals::parse(&made).expect("parses");
    assert!(
        principals
            .admit(format!("Bearer {TOKEN_A}").as_bytes())
            .is_some()
    );
    let short = "x".repeat(crate::token::MIN_TOKEN_BYTES - 1);
    let spaced = format!("{} {}", &TOKEN_A[..20], &TOKEN_A[20..]);
    for bad in [short.as_str(), &spaced, "", "\n"] {
        assert!(
            token_line("dev", ClientRole::Client, &qos, bad.as_bytes()).is_err(),
            "{bad:?}"
        );
    }
    assert!(token_line("a b", ClientRole::Client, &qos, TOKEN_A.as_bytes()).is_err());
    assert!(token_line("", ClientRole::Client, &qos, TOKEN_A.as_bytes()).is_err());
}

struct Broken;

impl std::io::Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
}

/// Catches: a token input of any size read whole (hash-token reads stdin), the limit
/// off by one, and a read error taken as an empty token.
#[test]
fn a_token_input_is_bounded_and_its_errors_reported() {
    let max = crate::token::MAX_TOKEN_FILE_BYTES;
    let padded = format!("{TOKEN_A}{}", "\n".repeat(max - TOKEN_A.len()));
    let made = token_line_from("dev", ClientRole::Client, &Qos::Ci, &mut padded.as_bytes());
    assert_eq!(made, Ok(line("dev", "ci", TOKEN_A)));
    let over = format!("{padded}\n");
    let refused = token_line_from("dev", ClientRole::Client, &Qos::Ci, &mut over.as_bytes());
    assert!(refused.is_err_and(|e| e.contains("more than 4096 bytes")));
    let refused = token_line_from("dev", ClientRole::Client, &Qos::Ci, &mut Broken);
    assert!(refused.is_err_and(|e| e.contains("read the token")));
}

/// Catches: `is_empty` and `len` disagreeing with the entries (an empty file admits no
/// one).
#[test]
fn an_empty_file_has_no_entries() {
    let empty = Principals::parse("# nobody yet\n\n").expect("parses");
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(
        empty
            .admit(format!("Bearer {TOKEN_A}").as_bytes())
            .is_none()
    );
    let one = Principals::parse(&line("ci", "ci", TOKEN_A)).expect("parses");
    assert!(!one.is_empty());
    assert_eq!(one.len(), 1);
}

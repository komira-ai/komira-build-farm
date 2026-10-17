//! Reading the names out of a certificate (`names_of`) and the host out of an address
//! (`host_of`): the DNS and IP subjectAltNames, and whether they cover a host.

use std::net::IpAddr;

use kbf_peer::{ROLE_URI_PREFIX, RoleError, host_of, names_of};
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, SanType};

fn uri(text: &str) -> SanType {
    SanType::URI(text.try_into().expect("an IA5 name"))
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn ip(text: &str) -> SanType {
    SanType::IpAddress(text.parse::<IpAddr>().expect("an IP address"))
}

/// A self-signed client certificate with these subjectAltNames.
fn der(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params
        .self_signed(&KeyPair::generate().expect("key"))
        .expect("sign")
        .der()
        .to_vec()
}

/// Whether the certificate with `sans` has a name covering `host`.
fn covers(sans: Vec<SanType>, host: &str) -> bool {
    names_of(&der(sans)).expect("a certificate").covers(host)
}

/// Catches a name read from where it is not one: the role URI, another URI naming the
/// host, an email, and the subject's common name are not names, and a DNS name that
/// looks like a role is only a DNS name. Also the order and spelling the refusal prints:
/// the DNS names as written, then the IPs.
#[test]
fn the_names_are_the_dns_and_ip_subject_alt_names_and_nothing_else() {
    let role = uri(&format!("{ROLE_URI_PREFIX}storage"));
    let names = names_of(&der(vec![
        ip("192.0.2.1"),
        dns("node-a"),
        role.clone(),
        uri("https://node-b"),
        SanType::Rfc822Name("ops@node-c".try_into().expect("an IA5 name")),
        ip("2001:db8::1"),
        dns("Node-D.Example"),
    ]))
    .expect("a certificate");
    assert_eq!(
        names.to_string(),
        "node-a, Node-D.Example, 192.0.2.1, 2001:db8::1"
    );
    for not_a_name in [
        "urn:kbf:role:storage",
        "storage",
        "https://node-b",
        "node-b",
        "ops@node-c",
        "node-c",
    ] {
        assert!(!names.covers(not_a_name), "{not_a_name}");
    }
    for name in ["node-a", "node-d.example", "192.0.2.1", "2001:db8::1"] {
        assert!(names.covers(name), "{name}");
    }

    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.distinguished_name.push(DnType::CommonName, "node-a");
    let cn_only = params
        .self_signed(&KeyPair::generate().expect("key"))
        .expect("sign");
    let names = names_of(cn_only.der()).expect("a certificate");
    assert!(!names.covers("node-a"), "the common name is not read");
    assert_eq!(names.to_string(), "no DNS or IP name");
    assert!(!covers(vec![role], "node-a"));
}

/// Catches a DNS name compared other than exactly up to ASCII case: a name that is a
/// prefix, a suffix, a parent or a child of the host, a trailing dot, an empty host, and
/// a host with a port or userinfo are not covered. A certificate with an empty DNS name
/// (an issuer's mistake) does not cover the empty host an address like `:8982` has.
///
/// Mutant seen red: the empty host left out of the refusals, so the empty name equals it.
#[test]
fn a_dns_name_covers_its_host_in_any_ascii_case_and_no_other() {
    let sans = || vec![dns("node-a.farm.example")];
    for host in [
        "node-a.farm.example",
        "NODE-A.Farm.EXAMPLE",
        "Node-A.farm.example",
    ] {
        assert!(covers(sans(), host), "{host}");
    }
    for host in [
        "",
        "node-a",
        "farm.example",
        "x.node-a.farm.example",
        "node-a.farm.example.",
        "node-a.farm.example.evil",
        "xnode-a.farm.example",
        "node-a.farm.example:8982",
        "user@node-a.farm.example",
    ] {
        assert!(!covers(sans(), host), "{host:?}");
    }
    assert_eq!(
        host_of(":8982"),
        "",
        "an address with only a port has no host"
    );
    for host in [host_of(":8982"), "node-a"] {
        assert!(!covers(vec![dns("")], host), "{host:?}");
    }
}

/// Catches a wildcard honoured. A wildcard name covers nothing: not a host under it, not
/// its parent, and not its own spelling (a node cannot register `*.farm.example`). The
/// farm's certificates carry the node's own names, so a wildcard is refused rather than
/// widened.
///
/// Mutant seen red: a name starting `*.` matched against the host's parent domain.
#[test]
fn a_wildcard_name_covers_nothing() {
    for host in [
        "node-a.farm.example",
        "a.b.farm.example",
        "farm.example",
        "*.farm.example",
        "*",
    ] {
        assert!(!covers(vec![dns("*.farm.example")], host), "{host}");
        assert!(!covers(vec![dns("*")], host), "{host}");
    }
    assert!(covers(
        vec![dns("*.farm.example"), dns("node-a.farm.example")],
        "node-a.farm.example"
    ));
}

/// Catches an IP compared as text, and a DNS name and an IP taken for each other. An IP
/// name covers the parsed address in any spelling of it (case, leading zero groups, the
/// compressed form), not another address, not an IPv4-mapped IPv6 address, and not a
/// string that is no address; a DNS name spelled like an IP covers no IP, and an IP
/// covers no DNS name.
///
/// Mutant seen red: a DNS name compared to an IP host.
#[test]
fn an_ip_name_covers_the_parsed_address_and_a_dns_name_never_an_ip() {
    let sans = || vec![ip("192.0.2.1"), ip("2001:db8::1")];
    for host in [
        "192.0.2.1",
        "2001:db8::1",
        "2001:DB8::1",
        "2001:0db8:0:0:0:0:0:1",
        "2001:db8:0000:0000:0000:0000:0000:0001",
    ] {
        assert!(covers(sans(), host), "{host}");
    }
    for host in [
        "192.0.2.2",
        "192.0.2.10",
        "192.0.2.01",
        "192.0.2",
        "2001:db8::2",
        "::ffff:192.0.2.1",
        "[2001:db8::1]",
        "192.0.2.1:80",
        "node-a",
    ] {
        assert!(!covers(sans(), host), "{host}");
    }
    assert!(!covers(vec![dns("192.0.2.1")], "192.0.2.1"));
    assert!(!covers(vec![dns("2001:db8::1")], "2001:db8::1"));
    assert!(!covers(vec![ip("192.0.2.1")], "node-a"));
}

/// Catches the port cut from the wrong place, and the cut made where it is not a port.
/// `host:port` loses its port; an IPv6 literal loses its brackets, and its port only
/// after them; anything else (a colon not followed by a port, an unbracketed IPv6
/// address, a path or userinfo) stays whole, so it is never mistaken for a covered host.
///
/// Mutant seen red: the port cut from the first `:`.
#[test]
fn the_host_of_an_address_loses_its_port_and_its_brackets_only() {
    for (addr, host) in [
        ("node-a:8982", "node-a"),
        ("node-a", "node-a"),
        ("node-a.farm.example:1", "node-a.farm.example"),
        ("127.0.0.1:80", "127.0.0.1"),
        ("127.0.0.1", "127.0.0.1"),
        ("[::1]:8982", "::1"),
        ("[::1]", "::1"),
        ("[2001:db8::1]:65535", "2001:db8::1"),
        ("::1", "::1"),
        ("::1:8982", "::1:8982"),
        ("[::1", "[::1"),
        ("[::1]x", "[::1]x"),
        ("[::1]:", "[::1]:"),
        ("[::1]:port", "[::1]:port"),
        ("[node-a]:80", "[node-a]:80"),
        ("[192.0.2.1]:80", "[192.0.2.1]:80"),
        ("node-a:", "node-a:"),
        ("node-a:http", "node-a:http"),
        ("node-a:+80", "node-a:+80"),
        ("node-a:65536", "node-a:65536"),
        ("node-a:1/x", "node-a:1/x"),
        ("node-a@node-b:1", "node-a@node-b"),
        (":80", ""),
        ("", ""),
    ] {
        assert_eq!(host_of(addr), host, "{addr:?}");
    }
}

/// Catches bytes that are no certificate taken for one with no names.
#[test]
fn bytes_that_are_no_certificate_are_refused() {
    assert!(matches!(
        names_of(b"not DER"),
        Err(RoleError::NotACertificate(_))
    ));
}

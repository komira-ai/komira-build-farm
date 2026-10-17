//! The deny entry grammar: one parser for the deny list file, the server's checks and the
//! map. Each test names the defect it catches.

use kbf_map::{DenyEntry, DenyEntryError, normalize_serial};

const KEY: &str = "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

// Catches: a serial kept in a spelling (case, colons, leading zeros), which would make one
// certificate two entries and a lookup by the certificate's own spelling miss.
#[test]
fn spellings_of_one_entry_are_equal_and_canonical() {
    let want = DenyEntry::Serial("8f0a".to_owned());
    for spelled in [
        "serial 00:8F:0A",
        "serial 8f0a",
        "serial   8F0A",
        "serial 008f0a",
    ] {
        assert_eq!(spelled.parse::<DenyEntry>().unwrap(), want, "{spelled}");
    }
    assert_eq!(want.to_string(), "serial 8f0a");
    assert_eq!(
        "serial 0:00".parse::<DenyEntry>().unwrap().to_string(),
        "serial 0"
    );
    let key = format!("spki-sha256 {KEY}").parse::<DenyEntry>().unwrap();
    assert_eq!(key, DenyEntry::SpkiSha256(KEY.to_ascii_lowercase()));
    let node = "node Mac-07".parse::<DenyEntry>().unwrap();
    assert_eq!(node.to_string(), "node Mac-07", "a node id keeps its case");
    for e in [want, key, node] {
        assert_eq!(
            e.to_string().parse::<DenyEntry>().unwrap(),
            e,
            "display round trips"
        );
    }
    assert_eq!(normalize_serial("000AbC"), "abc");
    assert_eq!(normalize_serial("000"), "0");
}

// Catches: a serial that is accepted when it is not hex (the mutant this test was
// written against), is empty, or is only colons.
#[test]
fn a_serial_that_is_not_hex_is_refused() {
    for bad in ["0x1", "xyz", "12g4", ":", "::", "ab-cd"] {
        let e = format!("serial {bad}").parse::<DenyEntry>().unwrap_err();
        assert_eq!(e, DenyEntryError::Serial(bad.to_owned()), "{bad}");
        assert!(e.to_string().contains("a serial is hex digits"));
    }
    assert_eq!(
        "serial".parse::<DenyEntry>(),
        Err(DenyEntryError::NoValue("serial".to_owned()))
    );
}

// Catches: a key hash of the wrong length or alphabet, and kinds and shapes that are not
// entries.
#[test]
fn other_malformed_entries_are_refused() {
    let short = "spki-sha256 abcd".parse::<DenyEntry>().unwrap_err();
    assert_eq!(short, DenyEntryError::Spki("abcd".to_owned()));
    assert!(short.to_string().contains("64 hex digits"));
    let g = format!("spki-sha256 {}", "g".repeat(64));
    assert!(matches!(
        g.parse::<DenyEntry>(),
        Err(DenyEntryError::Spki(_))
    ));
    let kind = "fingerprint ab".parse::<DenyEntry>().unwrap_err();
    assert_eq!(kind.to_string(), "unknown entry kind \"fingerprint\"");
    for shape in ["", "   ", "serial 0a extra"] {
        assert_eq!(
            shape.parse::<DenyEntry>(),
            Err(DenyEntryError::Shape),
            "{shape:?}"
        );
    }
    assert_eq!(
        DenyEntryError::NoValue("node".to_owned()).to_string(),
        "node needs a value"
    );
}

// Catches: a comment or blank line read as an entry, or a comment hiding a bad entry.
#[test]
fn lines_of_a_file_may_be_blank_or_comments() {
    for skipped in ["", "   ", "# a comment", "  # serial zz"] {
        assert_eq!(DenyEntry::parse_line(skipped), Ok(None), "{skipped:?}");
    }
    assert_eq!(
        DenyEntry::parse_line("serial 0A:1B   # node-a's leaked certificate"),
        Ok(Some(DenyEntry::Serial("a1b".to_owned())))
    );
    assert_eq!(
        DenyEntry::parse_line("serial 0a extra"),
        Err(DenyEntryError::Shape)
    );
}

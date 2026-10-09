//! Golden bytes for every message, written out by hand from the layout in the module
//! docs (never produced by the encoder), and the decoder's refusals.

use super::*;

/// Hex with spaces and newlines allowed, for readable goldens.
fn bytes(hex: &str) -> Vec<u8> {
    let compact: String = hex.split_whitespace().collect();
    hex::decode(compact).expect("valid hex in a golden")
}

fn run_request() -> RunRequest {
    RunRequest {
        argv: vec!["sh".into(), "-c".into()],
        env: vec![("A".into(), "b".into())],
        cwd: "w".into(),
        timeout_ms: 5000,
        outputs: vec!["o".into()],
    }
}

fn host_goldens() -> Vec<(HostMsg, Vec<u8>)> {
    vec![
        (
            HostMsg::Hello {
                version: 1,
                token: [0x11; TOKEN_LEN],
            },
            // kind, version, 32 token bytes
            bytes(&format!("01 0001 {}", "11".repeat(32))),
        ),
        (
            HostMsg::Run(run_request()),
            bytes(
                "02
                 00000002 00000002 7368 00000002 2d63
                 00000001 00000001 41 00000001 62
                 00000001 77
                 0000000000001388
                 00000001 00000001 6f",
            ),
        ),
        (HostMsg::Kill, bytes("03")),
    ]
}

fn exit() -> Exit {
    Exit {
        end: End::Signaled(9),
        usage: Usage {
            user_micros: 1,
            system_micros: 2,
            peak_rss_bytes: 3,
            wall_micros: 4,
        },
        outputs: vec![OutputEntry {
            path: "o".into(),
            kind: OutputKind::File,
            size: 7,
        }],
        stragglers: true,
    }
}

fn guest_goldens() -> Vec<(GuestMsg, Vec<u8>)> {
    vec![
        (
            GuestMsg::Ready {
                version: 1,
                session: "Aqua".into(),
            },
            bytes("81 0001 00000004 41717561"),
        ),
        (
            GuestMsg::Refused {
                reason: Refusal::Token,
                detail: "no".into(),
            },
            bytes("82 02 00000002 6e6f"),
        ),
        (GuestMsg::Started { pid: 0x0102_0304 }, bytes("83 01020304")),
        (
            GuestMsg::Exited(exit()),
            bytes(
                "84 01 00000009
                 0000000000000001 0000000000000002 0000000000000003 0000000000000004
                 00000001 00000001 6f 01 0000000000000007
                 01",
            ),
        ),
    ]
}

/// Catches any change to a message layout (field order, width, byte order, a tag
/// value) that was not made as a new protocol version.
#[test]
fn every_message_encodes_to_its_golden_bytes() {
    for (msg, golden) in host_goldens() {
        assert_eq!(hex::encode(msg.encode()), hex::encode(&golden), "{msg:?}");
    }
    for (msg, golden) in guest_goldens() {
        assert_eq!(hex::encode(msg.encode()), hex::encode(&golden), "{msg:?}");
    }
}

/// Catches a decoder that drifts from the encoder, field by field.
#[test]
fn every_golden_decodes_to_its_message() {
    for (msg, golden) in host_goldens() {
        assert_eq!(HostMsg::decode(&golden).expect("decodes"), msg);
    }
    for (msg, golden) in guest_goldens() {
        assert_eq!(GuestMsg::decode(&golden).expect("decodes"), msg);
    }
}

/// The other end values and tags, so each arm of `End` and every `Refusal` and
/// `OutputKind` round-trips (a swapped tag would decode as another variant).
#[test]
fn every_tag_round_trips() {
    for end in [
        End::Exited(-3),
        End::Signaled(15),
        End::TimedOut,
        End::Killed,
    ] {
        let msg = GuestMsg::Exited(Exit { end, ..exit() });
        assert_eq!(GuestMsg::decode(&msg.encode()).expect("decodes"), msg);
    }
    for reason in [
        Refusal::Version,
        Refusal::Token,
        Refusal::AlreadyRan,
        Refusal::BadRequest,
        Refusal::NoHello,
        Refusal::StartFailed,
    ] {
        let msg = GuestMsg::Refused {
            reason,
            detail: String::new(),
        };
        assert_eq!(GuestMsg::decode(&msg.encode()).expect("decodes"), msg);
    }
    for kind in [
        OutputKind::Missing,
        OutputKind::File,
        OutputKind::Directory,
        OutputKind::Symlink,
        OutputKind::Other,
    ] {
        let mut x = exit();
        x.outputs[0].kind = kind;
        x.stragglers = false;
        let msg = GuestMsg::Exited(x);
        assert_eq!(GuestMsg::decode(&msg.encode()).expect("decodes"), msg);
    }
}

/// Catches a decoder that reads past a short body or ignores bytes after it: every
/// strict prefix of every golden is refused, and so is one byte more.
#[test]
fn a_short_or_long_body_is_refused() {
    for (_, golden) in host_goldens() {
        for n in 0..golden.len() {
            assert!(
                HostMsg::decode(&golden[..n]).is_err(),
                "prefix {n} of {golden:?}"
            );
        }
        let mut long = golden.clone();
        long.push(0);
        assert!(matches!(
            HostMsg::decode(&long),
            Err(WireError::Trailing(1))
        ));
    }
    for (_, golden) in guest_goldens() {
        for n in 0..golden.len() {
            assert!(
                GuestMsg::decode(&golden[..n]).is_err(),
                "prefix {n} of {golden:?}"
            );
        }
        let mut long = golden.clone();
        long.push(0);
        assert!(matches!(
            GuestMsg::decode(&long),
            Err(WireError::Trailing(1))
        ));
    }
}

/// Catches a side that accepts the other side's messages, an unknown kind or tag, or
/// a string that is not UTF-8.
#[test]
fn unknown_kinds_tags_and_bad_strings_are_refused() {
    assert!(matches!(
        HostMsg::decode(&[READY]),
        Err(WireError::Kind(READY))
    ));
    assert!(matches!(
        GuestMsg::decode(&[KILL]),
        Err(WireError::Kind(KILL))
    ));
    assert!(matches!(
        HostMsg::decode(&[0x7f]),
        Err(WireError::Kind(0x7f))
    ));
    let bad_refusal = bytes("82 07 00000000");
    assert!(matches!(
        GuestMsg::decode(&bad_refusal),
        Err(WireError::Tag {
            field: "refusal",
            tag: 7
        })
    ));
    let mut bad_end = GuestMsg::Exited(exit()).encode();
    bad_end[1] = 4;
    assert!(matches!(
        GuestMsg::decode(&bad_end),
        Err(WireError::Tag {
            field: "end",
            tag: 4
        })
    ));
    let mut bad_kind = GuestMsg::Exited(exit()).encode();
    let kind_at = bad_kind.len() - 10;
    bad_kind[kind_at] = 5;
    assert!(matches!(
        GuestMsg::decode(&bad_kind),
        Err(WireError::Tag {
            field: "output kind",
            tag: 5
        })
    ));
    let mut bad_flag = GuestMsg::Exited(exit()).encode();
    *bad_flag.last_mut().expect("non-empty") = 2;
    assert!(matches!(
        GuestMsg::decode(&bad_flag),
        Err(WireError::Tag {
            field: "stragglers",
            tag: 2
        })
    ));
    let not_utf8 = bytes("81 0001 00000001 ff");
    assert!(matches!(GuestMsg::decode(&not_utf8), Err(WireError::Utf8)));
}

/// Catches a list count trusted before its items are there (a 4-billion-item count
/// would otherwise allocate before failing).
#[test]
fn a_list_count_larger_than_the_body_is_refused_first() {
    let huge = bytes("02 ffffffff");
    assert!(matches!(HostMsg::decode(&huge), Err(WireError::Truncated)));
}

/// Catches a frame length not checked before the buffer is allocated, a missing
/// length prefix, and a mid-frame end taken as a clean close.
#[test]
fn frames_carry_their_length_and_bad_lengths_are_refused() {
    let mut out = Vec::new();
    write_frame(&mut out, &HostMsg::Kill.encode()).expect("written");
    assert_eq!(out, bytes("00000001 03"));
    assert_eq!(read_frame(&mut out.as_slice()).expect("read"), vec![KILL]);

    assert!(matches!(read_frame(&mut &[][..]), Err(WireError::Closed)));
    assert!(matches!(
        read_frame(&mut &[0, 0][..]),
        Err(WireError::Truncated)
    ));
    assert!(matches!(
        read_frame(&mut &bytes("00000000")[..]),
        Err(WireError::Length(0))
    ));
    let over = (MAX_FRAME + 1).to_be_bytes();
    assert!(matches!(read_frame(&mut &over[..]), Err(WireError::Length(n)) if n == MAX_FRAME + 1));
    assert!(matches!(
        read_frame(&mut &bytes("00000002 03")[..]),
        Err(WireError::Truncated)
    ));

    assert!(matches!(
        write_frame(&mut Vec::new(), &[]),
        Err(WireError::Length(0))
    ));
    let big = vec![0u8; MAX_FRAME as usize + 1];
    assert!(matches!(
        write_frame(&mut Vec::new(), &big),
        Err(WireError::Length(_))
    ));
    let max = vec![KILL; MAX_FRAME as usize];
    write_frame(&mut Vec::new(), &max).expect("a frame of exactly MAX_FRAME is allowed");
}

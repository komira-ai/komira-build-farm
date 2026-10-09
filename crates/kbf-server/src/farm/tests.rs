//! Unit tests of the farm core's private helpers.

use super::*;

/// Catches a server process that reuses an earlier process's term (issue #137:
/// every process granted leases from `(1, 0)`), and one whose term does not order
/// after that of a process started a few milliseconds before. A term is also never
/// 1, the term every server before the fix used.
#[test]
fn each_process_term_is_new_and_later() {
    let first = process_term();
    std::thread::sleep(Duration::from_millis(3));
    let second = process_term();
    assert!(second > first, "{second} does not order after {first}");
    assert!(first >> 16 > 0, "{first} could be a pre-fix term");
}

/// Catches a name parser that is not the exact inverse of [`operation_name`]: one
/// that ignores the term or accepts another (issue #154), and one that accepts a
/// spelling the server never writes, which would give one operation several names.
#[test]
fn operation_names_parse_only_as_this_term_writes_them() {
    let term = 0x0199_8a6b_2c3d_4e5f;
    for n in [0, 1, 42, u64::MAX] {
        let name = operation_name(term, WaiterId(n));
        assert_eq!(
            parse_operation_name(&name, term),
            Some(WaiterId(n)),
            "{name}"
        );
        for other in [0, term - 1, term + 1, u64::MAX] {
            assert_eq!(
                parse_operation_name(&name, other),
                None,
                "{name} as {other}"
            );
        }
    }
    assert_eq!(operation_name(7, WaiterId(3)), "operations/7-3");
    for refused in [
        "",
        "operations/",
        "operations/7",
        "operations/7-",
        "operations/-3",
        "operations/7--3",
        "operations/7-3-1",
        "operations/07-3",
        "operations/7-03",
        "operations/+7-3",
        "operations/7-+3",
        "operations/7-3 ",
        " operations/7-3",
        "operations/7-18446744073709551616",
        "Operations/7-3",
        "operation/7-3",
        "operations/7_3",
        "7-3",
        "operations/3",
        "operations/cached/7-3",
    ] {
        assert_eq!(parse_operation_name(refused, 7), None, "{refused:?}");
    }
    assert_eq!(parse_operation_name("operations/7-0", 7), Some(WaiterId(0)));
}

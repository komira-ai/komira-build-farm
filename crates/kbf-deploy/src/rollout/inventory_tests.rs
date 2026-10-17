use super::*;

const FLEET: &str = "\
# the fleet
m1 daemon-mac

s1   storage-and-daemon
front\tfrontend
d1 daemon
s2 storage-and-daemon
m2 daemon-mac
";

fn names(hosts: &[Host]) -> Vec<&str> {
    hosts.iter().map(|h| h.name.as_str()).collect()
}

/// Catches an order read from the file instead of the roles, an unstable sort among
/// hosts of one role, and the Macs ahead of the Linux daemons.
#[test]
fn hosts_are_ordered_by_role_then_by_the_file() {
    let inventory = Inventory::parse(FLEET).expect("parses");
    assert_eq!(
        names(&inventory.ordered()),
        ["s1", "s2", "front", "d1", "m1", "m2"]
    );
    assert_eq!(inventory.frontend().as_str(), "front");
}

fn refused(text: &str) -> String {
    Inventory::parse(text).expect_err("refused").to_string()
}

/// Catches a typo read as a host: each way a line or the whole file can be wrong, with
/// its line number.
#[test]
fn a_bad_inventory_is_refused_with_its_line() {
    assert_eq!(
        refused("front frontend\ns1\n"),
        "line 2: expected `NAME ROLE`"
    );
    assert_eq!(
        refused("front frontend\ns1 daemon extra\n"),
        "line 2: expected `NAME ROLE`"
    );
    assert!(refused("Front frontend\n").starts_with("line 1: host name \"Front\" is not valid"));
    assert_eq!(
        refused("front frontend\ns1 storage\n"),
        "line 2: unknown role \"storage\" (storage-and-daemon, frontend, daemon or daemon-mac)"
    );
    assert_eq!(
        refused("front frontend\n\ns1 daemon\ns1 daemon-mac\n"),
        "line 4: host s1 is already listed on line 3"
    );
}

/// Catches a rollout with no frontend to back up, or two.
#[test]
fn exactly_one_frontend_is_required() {
    assert_eq!(
        refused("s1 daemon\n"),
        "the inventory needs exactly one frontend, and has 0"
    );
    assert_eq!(
        refused("a frontend\nb frontend\n"),
        "the inventory needs exactly one frontend, and has 2"
    );
    assert_eq!(
        refused(""),
        "the inventory needs exactly one frontend, and has 0"
    );
}

/// Catches a role printed under another name.
#[test]
fn roles_print_as_they_are_written() {
    for word in ["storage-and-daemon", "frontend", "daemon", "daemon-mac"] {
        let role = Role::parse(word).expect("a role");
        assert_eq!(role.to_string(), word);
    }
}

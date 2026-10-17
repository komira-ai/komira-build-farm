use std::collections::BTreeMap;

use super::*;
use kbf_host::step::{Owner, Perms};

fn user(uid: u32, gid: u32, groups: &[u32]) -> RunUser {
    RunUser {
        name: "kbf".to_owned(),
        uid,
        gid,
        groups: groups.to_vec(),
        home: "/srv/kbf".to_owned(),
    }
}

fn dir(bits: u32, uid: u32, gid: u32) -> Meta {
    Meta {
        kind: Kind::Dir,
        perms: Perms::new(bits),
        owner: Owner { uid, gid },
        len: 0,
    }
}

fn blocked(user: &RunUser, hot: &str, tree: &[(&str, Meta)]) -> Option<String> {
    let tree: BTreeMap<&str, Meta> = tree.iter().copied().collect();
    let hot = HostPath::new(hot).expect("path");
    blocked_by(user, &hot, |p| Ok(tree.get(p.as_str()).cloned()))
        .expect("stat")
        .map(|(p, _)| p.to_string())
}

/// The failure seen live: `/var/lib/kbf-server` is the server account's, 0700, and the
/// unit runs as another user. Catches a check that looks only at the directory itself
/// or at the owner class, and one that skips the ancestors.
#[test]
fn a_parent_the_user_cannot_enter_is_named() {
    let tree = [
        ("/", dir(0o755, 0, 0)),
        ("/var", dir(0o755, 0, 0)),
        ("/var/lib", dir(0o755, 0, 0)),
        ("/var/lib/kbf-server", dir(0o700, 996, 996)),
    ];
    assert_eq!(
        blocked(&user(1500, 1500, &[1500]), "/var/lib/kbf-server/hot", &tree).as_deref(),
        Some("/var/lib/kbf-server")
    );
    // The owner itself gets in.
    assert_eq!(
        blocked(&user(996, 996, &[996]), "/var/lib/kbf-server/hot", &tree),
        None
    );
}

/// The class that matches decides, as in the kernel: an owner with no owner bit is
/// refused even when other has it; a group member needs the group bit; everyone else
/// needs other's. Catches a check that ORs the three classes.
#[test]
fn the_first_matching_class_decides() {
    let hot = "/d/hot";
    let root = ("/", dir(0o755, 0, 0));
    let with = |bits| [root, ("/d", dir(bits, 10, 20))];
    // owner, no owner x, other x
    assert_eq!(
        blocked(&user(10, 99, &[99]), hot, &with(0o601)).as_deref(),
        Some("/d")
    );
    // group member
    assert_eq!(blocked(&user(11, 99, &[99, 20]), hot, &with(0o710)), None);
    assert_eq!(
        blocked(&user(11, 99, &[99, 20]), hot, &with(0o701)).as_deref(),
        Some("/d")
    );
    // primary gid counts even if groups omits it
    assert_eq!(blocked(&user(11, 20, &[]), hot, &with(0o710)), None);
    // other
    assert_eq!(blocked(&user(11, 99, &[99]), hot, &with(0o701)), None);
    assert_eq!(
        blocked(&user(11, 99, &[99]), hot, &with(0o750)).as_deref(),
        Some("/d")
    );
}

/// Directories that do not exist yet are made 0755 by the install, so they and
/// everything below are not judged; the hot directory itself is not judged either (it
/// is made the user's); a symbolic link (not a directory here) is not followed.
#[test]
fn missing_directories_the_dir_itself_and_links_are_not_judged() {
    let u = user(1500, 1500, &[1500]);
    let tree = [("/", dir(0o755, 0, 0)), ("/srv", dir(0o700, 0, 0))];
    assert_eq!(blocked(&u, "/srv/x/hot", &tree).as_deref(), Some("/srv"));
    assert_eq!(blocked(&u, "/var/lib/kbf/hot", &tree), None);
    let own = [
        ("/", dir(0o755, 0, 0)),
        ("/srv", dir(0o755, 0, 0)),
        ("/srv/hot", dir(0o000, 0, 0)),
    ];
    assert_eq!(blocked(&u, "/srv/hot", &own), None);
    let mut link = dir(0o000, 0, 0);
    link.kind = Kind::Other;
    assert_eq!(
        blocked(&u, "/l/hot", &[("/", dir(0o755, 0, 0)), ("/l", link)]),
        None
    );
}

#[test]
fn the_message_names_the_path_and_the_user() {
    let u = user(1500, 1500, &[1500]);
    let m = message(
        &u,
        &HostPath::new("/var/lib/kbf-server/hot").expect("path"),
        &HostPath::new("/var/lib/kbf-server").expect("path"),
        &dir(0o700, 996, 996),
    );
    assert!(
        m.contains("/var/lib/kbf-server/hot") && m.contains("kbf"),
        "{m}"
    );
    assert!(m.contains("/var/lib/kbf-server (mode 0700"), "{m}");
}

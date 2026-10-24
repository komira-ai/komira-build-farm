//! A socket shared with a named client group: who is admitted, the modes and owners of
//! what is shared, and what is refused.

mod support;

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;
use std::sync::Arc;

use kbf_ca::{Authority, CaError, KeyFileFault, Policy, Signer};
use kbf_signer::{
    Accounts, ClientGroup, Flags, Route, SignerError, SignerServer, SocketSigner, Started, bind,
    bind_for_group, ensure, ensure_shared,
};
use support::{
    NOW, Stream, a_group_to_share_with, a_named_group_of_mine, accounts, egid, euid, make_ca,
    scratch,
};
use x509_parser::prelude::{FromDer, X509Certificate};

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("metadata").mode() & 0o7777
}

fn gid_of(path: &Path) -> u32 {
    std::fs::metadata(path).expect("metadata").gid()
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The group file line of a group called `clients`.
fn clients(gid: u32, members: &str) -> String {
    format!("clients:x:{gid}:{members}")
}

/// A group id this account is not a member of (or root, which may use any).
fn a_group_i_am_not_in() -> u32 {
    let mine: Vec<u32> = rustix::process::getgroups()
        .expect("getgroups")
        .into_iter()
        .map(|g| g.as_raw())
        .chain([egid()])
        .collect();
    (1..).find(|g| !mine.contains(g)).expect("a free id")
}

// ---- who is admitted ------------------------------------------------------------------

// Resolving a group by name: the first group of that name gives its id, comments and blank
// lines are skipped, and a group that is not there (or an empty name, or an id that is not
// a number) is refused by name, as is an unreadable group file.
#[test]
fn a_group_is_found_by_name_and_a_missing_one_is_refused() {
    let dir = scratch("group_resolve");
    let files = accounts(
        &dir,
        &[],
        &[
            "# a comment",
            "",
            "root:x:0:",
            "clients:x:2001:alice",
            "clients:x:2002:",
            "broken:x:notanumber:",
            "short",
            ":x:2003:",
        ],
    );
    let found = ClientGroup::resolve("clients", files.clone()).expect("clients");
    assert_eq!(found.gid(), 2001, "the first of that name");
    assert_eq!(
        ClientGroup::resolve("root", files.clone())
            .expect("root")
            .gid(),
        0
    );
    for name in ["nobody-here", "", "broken", "short", "client", "clients "] {
        match ClientGroup::resolve(name, files.clone()) {
            Err(SignerError::NoSuchGroup(n)) => assert_eq!(n, name),
            other => panic!("{name:?}: {other:?}"),
        }
    }
    let message = SignerError::NoSuchGroup("kbf-signer-clients".into()).to_string();
    assert_eq!(message, "--client-group kbf-signer-clients: no such group");

    let nowhere = Accounts::in_dir(&dir.join("absent"));
    assert!(matches!(
        ClientGroup::resolve("clients", nowhere),
        Err(SignerError::Io(_))
    ));
}

// Who belongs: an account listed in the group's members, an account whose primary group it
// is, a peer whose effective group it is; and nobody else. A name that merely starts with a
// member's, a member of another group, an unknown user id and a user id whose files cannot
// be read are all outside.
#[test]
fn membership_is_listed_members_primary_group_or_effective_group() {
    let dir = scratch("group_members");
    let files = accounts(
        &dir,
        &[
            "alice:x:1001:1001::/srv/x:/bin/sh",
            "bob:x:1002:1002::/srv/x:/bin/sh",
            "carol:x:1003:2001::/srv/x:/bin/sh",
            "dave:x:1004:1004::/srv/x:/bin/sh",
            "alice-two:x:1005:1005::/srv/x:/bin/sh",
            "# comment",
            "",
            "short",
            "nobody:x:notanumber:",
        ],
        &[
            "alice:x:1001:",
            "clients:x:2001:alice,dave2",
            "others:x:2002:bob,dave",
        ],
    );
    let group = ClientGroup::resolve("clients", files).expect("clients");
    // Listed member.
    assert!(group.admits(1001, 1001));
    // Primary group.
    assert!(group.admits(1003, 5));
    // Effective group, whoever the user is.
    assert!(group.admits(9999, 2001));
    // Not members: listed only in another group, a name that begins with a member's, a
    // member name that is not an account, a user id nobody has.
    assert!(!group.admits(1002, 1002));
    assert!(!group.admits(1004, 1004));
    assert!(!group.admits(1005, 1005));
    assert!(!group.admits(4242, 4242));
}

// Membership is read at each connection: an account added to the group is admitted at once
// and one removed is refused at once; an unreadable file admits nobody.
#[test]
fn membership_is_read_again_at_every_check() {
    let dir = scratch("group_reread");
    let files = accounts(
        &dir,
        &["alice:x:1001:1001::/srv/x:/bin/sh"],
        &[&clients(2001, "")],
    );
    let group = ClientGroup::resolve("clients", files).expect("clients");
    assert!(!group.admits(1001, 1001));
    std::fs::write(dir.join("group"), clients(2001, "alice")).expect("add");
    assert!(group.admits(1001, 1001));
    std::fs::write(dir.join("group"), clients(2001, "")).expect("remove");
    assert!(!group.admits(1001, 1001));
    std::fs::write(dir.join("group"), clients(2001, "alice")).expect("add again");
    std::fs::remove_file(dir.join("passwd")).expect("no passwd");
    assert!(!group.admits(1001, 1001), "passwd unreadable");
    std::fs::write(dir.join("passwd"), "alice:x:1001:1001::/srv/x:/bin/sh").expect("passwd");
    std::fs::remove_file(dir.join("group")).expect("no group");
    assert!(!group.admits(1001, 1001), "group unreadable");
}

/// Serves the CA's key on a socket shared with the account's own group, to the peers of
/// `allowed_uid` and to the members of `group`.
fn serve_to_group(ca: &support::Ca, allowed_uid: u32, group: ClientGroup) -> PathBuf {
    let socket = ca.scratch.join(format!("run-{allowed_uid}")).join("sock");
    let server = SignerServer::from_dir(&ca.dir, allowed_uid, Policy::default())
        .expect("the server")
        .with_clock(|| NOW)
        .with_client_group(group);
    let listener = bind_for_group(&socket, a_group_to_share_with()).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });
    socket
}

fn raw(socket: &Path, request: &[u8]) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket).expect("connect");
    if stream.write_all(request).is_err() {
        return Vec::new();
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    answer
}

// A peer that is not the configured user but belongs to the group connects and gets a
// signature that verifies under the CA; a peer that is neither is closed on without a
// byte; the configured user is still served when it is not in the group.
#[test]
fn a_member_connects_and_signs_and_a_non_member_is_refused() {
    let ca = make_ca("group_serve");
    let dir = scratch("group_serve_accounts");
    // The group's id, which this account is not in; its primary group is another.
    let x = a_group_i_am_not_in();
    let me = format!("tester:x:{}:{}::/srv/x:/bin/sh", euid(), x.wrapping_add(1));

    // A member (listed), the configured user being another.
    let member = accounts(&dir.join("member"), &[&me], &[&clients(x, "tester")]);
    let socket = serve_to_group(
        &ca,
        euid().wrapping_add(1),
        ClientGroup::resolve("clients", member).expect("group"),
    );
    let client = SocketSigner::connect(&socket, Route::Node).expect("a member is served");
    assert_eq!(client.public_key(), ca.spki.as_slice());
    let client = Arc::new(client);
    let authority = Authority::import(client, Policy::default(), &ca.root).expect("authority");
    let id = "node-a".parse().expect("id");
    let key = support::fresh_key();
    let issued = authority
        .issue_node(
            &kbf_ca::NodeCert {
                node_id: &id,
                role: kbf_types::Role::Daemon,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Stream(3),
        )
        .expect("a member signs");
    let (_, leaf) = X509Certificate::from_der(&issued.der).expect("leaf");
    let (_, root) = X509Certificate::from_der(&ca.root).expect("root");
    leaf.verify_signature(Some(root.public_key()))
        .expect("verifies under the root");

    // Not a member: neither listed nor in the group by primary or effective group.
    let stranger = accounts(&dir.join("stranger"), &[&me], &[&clients(x, "someone")]);
    let socket = serve_to_group(
        &ca,
        euid().wrapping_add(2),
        ClientGroup::resolve("clients", stranger).expect("group"),
    );
    let refused = SocketSigner::connect(&socket, Route::Node).expect_err("a stranger is refused");
    assert!(refused.0.contains("signer socket"), "{refused}");
    assert!(raw(&socket, &[0, 0, 0, 1, 0]).is_empty(), "not a byte");

    // The configured user, in no group.
    let socket = serve_to_group(
        &ca,
        euid(),
        ClientGroup::resolve(
            "clients",
            accounts(&dir.join("none"), &[], &[&clients(x, "")]),
        )
        .expect("group"),
    );
    SocketSigner::connect(&socket, Route::Node).expect("the configured user is still served");
}

// A peer is a member by its primary group in passwd or by its effective group; both are
// served on a real socket.
#[test]
fn a_primary_or_effective_group_member_is_served() {
    let ca = make_ca("group_primary");
    let dir = scratch("group_primary_accounts");
    let x = a_group_i_am_not_in();
    let by_primary = accounts(
        &dir.join("primary"),
        &[&format!("tester:x:{}:{x}::/srv/x:/bin/sh", euid())],
        &[&clients(x, "")],
    );
    let socket = serve_to_group(
        &ca,
        euid().wrapping_add(1),
        ClientGroup::resolve("clients", by_primary).expect("group"),
    );
    SocketSigner::connect(&socket, Route::Node).expect("the primary group");

    let by_effective = accounts(&dir.join("effective"), &[], &[&clients(egid(), "")]);
    let socket = serve_to_group(
        &ca,
        euid().wrapping_add(2),
        ClientGroup::resolve("clients", by_effective).expect("group"),
    );
    SocketSigner::connect(&socket, Route::Node).expect("the effective group");
}

// ---- the socket and its directory -----------------------------------------------------

// Shared with a group: the directory is 0750 and the socket 0660, both of the group, the
// account still owns both; a restart over the stale socket gives the same result, and a
// directory made 0700 by an earlier unshared run is opened to the group.
#[test]
fn the_shared_socket_is_0660_in_a_0750_directory_of_the_group() {
    let gid = a_group_to_share_with();
    let base = scratch("shared_modes");
    let socket = base.join("run").join("sock");
    let dir = base.join("run");
    {
        let _bound = bind_for_group(&socket, gid).expect("bind");
        for path in [&dir, &socket] {
            assert_eq!(gid_of(path), gid, "{}", path.display());
            assert_eq!(std::fs::metadata(path).expect("m").uid(), euid());
        }
        assert_eq!(mode(&dir), 0o750);
        assert_eq!(mode(&socket), 0o660);
        assert_eq!(mode(&base.join("run").join("sock.lock")), 0o600);
    }
    // The socket is stale now; the directory exists already.
    let _bound = bind_for_group(&socket, gid).expect("rebind");
    assert_eq!((mode(&dir), mode(&socket)), (0o750, 0o660));

    // A directory that an unshared run left at 0700.
    drop(_bound);
    let unshared = base.join("older").join("sock");
    drop(bind(&unshared).expect("unshared"));
    assert_eq!(mode(unshared.parent().expect("dir")), 0o700);
    let _again = bind_for_group(&unshared, gid).expect("shared");
    assert_eq!(mode(unshared.parent().expect("dir")), 0o750);
    assert_eq!(mode(&unshared), 0o660);
    assert_eq!(gid_of(&unshared), gid);
}

// Without a group nothing changes: the directory is 0700 and the socket 0600, and a
// directory opened to a group is refused as before.
#[test]
fn without_a_group_the_socket_stays_private() {
    let base = scratch("unshared_modes");
    let socket = base.join("run").join("sock");
    let default_gid = {
        let bound = bind(&socket).expect("bind");
        drop(bound);
        gid_of(&socket)
    };
    assert_eq!(mode(&base.join("run")), 0o700);
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(gid_of(&base.join("run")), default_gid);

    chmod(&base.join("run"), 0o750);
    assert!(matches!(bind(&socket), Err(SignerError::SocketDir)));
    assert_eq!(mode(&base.join("run")), 0o750, "not repaired");
}

// A shared directory that others can enter is refused with the reason, and nothing in it
// is touched; a group-writable one, a link, and a file are refused as a bad directory.
#[test]
fn a_shared_directory_open_to_others_or_writable_by_the_group_is_refused() {
    let gid = a_group_to_share_with();
    let base = scratch("shared_refusals");
    let dir = base.join("run");
    let socket = dir.join("sock");
    std::fs::create_dir(&dir).expect("dir");
    for open in [0o755, 0o705, 0o701, 0o777, 0o750 | 0o004, 0o751] {
        chmod(&dir, open);
        match bind_for_group(&socket, gid) {
            Err(SignerError::WorldAccessible(d)) => assert_eq!(d, dir, "{open:o}"),
            other => panic!("{open:o}: {other:?}"),
        }
        assert_eq!(mode(&dir), open, "left as it was");
        assert!(
            !socket.exists() && !dir.join("sock.lock").exists(),
            "{open:o}"
        );
    }
    for bad in [0o770, 0o760, 0o720, 0o500, 0o3750] {
        chmod(&dir, bad);
        assert!(
            matches!(bind_for_group(&socket, gid), Err(SignerError::SocketDir)),
            "{bad:o}"
        );
        assert!(!socket.exists(), "{bad:o}");
    }
    chmod(&dir, 0o700);
    let link = base.join("link");
    std::os::unix::fs::symlink(&dir, &link).expect("link");
    assert!(matches!(
        bind_for_group(&link.join("sock"), gid),
        Err(SignerError::SocketDir)
    ));
    let file = base.join("file");
    std::fs::write(&file, b"x").expect("file");
    assert!(matches!(
        bind_for_group(&file.join("sock"), gid),
        Err(SignerError::SocketDir)
    ));
    assert!(
        SignerError::WorldAccessible(dir)
            .to_string()
            .contains("accessible to others")
    );
}

// A group the account cannot give files to is an error that names the path (a member of the
// group, or root, can; root is not refused).
#[test]
fn a_group_the_account_is_not_in_is_refused_with_the_path() {
    let base = scratch("shared_not_member");
    let socket = base.join("run").join("sock");
    let result = bind_for_group(&socket, a_group_i_am_not_in());
    if euid() == 0 {
        result.expect("root may give a file to any group");
        return;
    }
    match result {
        Err(e @ SignerError::Share { .. }) => {
            assert!(e.to_string().contains("sock"), "{e}");
            assert!(e.to_string().contains("client group"), "{e}");
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

// ---- the CA directory -----------------------------------------------------------------

fn ensure_shared_at(dir: &Path, gid: u32) -> Result<Started, SignerError> {
    ensure_shared(dir, Policy::default(), NOW, &mut Stream(9), gid).map(|(_, s)| s)
}

// A new CA made for a group: the directory is 0750 and ca.pem 0640 of the group, ca.key is
// 0600 and not given to the group, whichever start made it; a restart loads the same root.
#[test]
fn the_ca_is_shared_for_reading_and_the_key_stays_private() {
    let gid = a_group_to_share_with();
    let base = scratch("ca_shared");
    let dir = base.join("ca");
    let key = dir.join("ca.key");
    let pem = dir.join("ca.pem");

    assert_eq!(ensure_shared_at(&dir, gid).expect("made"), Started::Created);
    let key_group = gid_of(&key);
    let check = || {
        assert_eq!((mode(&dir), gid_of(&dir)), (0o750, gid));
        assert_eq!((mode(&pem), gid_of(&pem)), (0o640, gid));
        assert_eq!(mode(&key), 0o600);
        assert_eq!(std::fs::metadata(&key).expect("key").uid(), euid());
        assert_eq!(
            gid_of(&key),
            key_group,
            "the key is not handed to the group"
        );
    };
    check();
    let root = std::fs::read(&pem).expect("pem");

    assert_eq!(
        ensure_shared_at(&dir, gid).expect("loaded"),
        Started::Loaded
    );
    check();
    assert_eq!(std::fs::read(&pem).expect("pem"), root, "the same root");
    let server = SignerServer::from_shared_dir(&dir, euid(), Policy::default()).expect("server");
    drop(server);

    // The plain load refuses the shared directory, as it always did.
    assert!(matches!(
        SignerServer::from_dir(&dir, euid(), Policy::default()),
        Err(SignerError::Ca(CaError::KeyFile(KeyFileFault::DirMode)))
    ));
    assert!(matches!(
        ensure(&dir, Policy::default(), NOW, &mut Stream(9)),
        Err(SignerError::Ca(CaError::KeyFile(KeyFileFault::DirMode)))
    ));
}

// A CA made by an unshared start (0700, ca.pem 0644) is opened to the group at the next
// shared start, with the same root.
#[test]
fn an_unshared_ca_is_opened_to_the_group() {
    let gid = a_group_to_share_with();
    let ca = make_ca("ca_opened");
    let before = std::fs::read(ca.dir.join("ca.pem")).expect("pem");
    assert_eq!(mode(&ca.dir), 0o700);
    assert_eq!(
        ensure_shared_at(&ca.dir, gid).expect("loaded"),
        Started::Loaded
    );
    assert_eq!((mode(&ca.dir), gid_of(&ca.dir)), (0o750, gid));
    assert_eq!(
        (mode(&ca.dir.join("ca.pem")), gid_of(&ca.dir.join("ca.pem"))),
        (0o640, gid)
    );
    assert_eq!(mode(&ca.dir.join("ca.key")), 0o600);
    assert_eq!(std::fs::read(ca.dir.join("ca.pem")).expect("pem"), before);
}

// What is refused is refused before anything is changed: a key that the group could read, a
// directory others can enter or the group can write, and a directory that is not exactly a
// CA.
#[test]
fn a_shared_ca_refuses_an_open_key_or_directory_and_changes_nothing() {
    let gid = a_group_to_share_with();
    let ca = make_ca("ca_refused");
    let (dir, key, pem) = (ca.dir.clone(), ca.dir.join("ca.key"), ca.dir.join("ca.pem"));
    let fault = |r: Result<Started, SignerError>| match r {
        Err(SignerError::Ca(CaError::KeyFile(f))) => f,
        other => panic!("{other:?}"),
    };

    for open in [0o640, 0o660, 0o604] {
        chmod(&key, open);
        assert_eq!(
            fault(ensure_shared_at(&dir, gid)),
            KeyFileFault::Mode,
            "{open:o}"
        );
        assert_eq!(mode(&key), open, "the key is not repaired");
        assert_eq!(
            (mode(&dir), mode(&pem)),
            (0o700, 0o644),
            "nothing is opened"
        );
    }
    chmod(&key, 0o600);

    for open in [0o755, 0o705, 0o770, 0o760] {
        chmod(&dir, open);
        assert_eq!(
            fault(ensure_shared_at(&dir, gid)),
            KeyFileFault::DirMode,
            "{open:o}"
        );
        assert_eq!((mode(&dir), mode(&pem)), (open, 0o644), "{open:o}");
    }
    chmod(&dir, 0o700);

    std::fs::write(dir.join("extra"), b"x").expect("extra");
    assert!(matches!(
        ensure_shared_at(&dir, gid),
        Err(SignerError::CaDirState { .. })
    ));
    assert_eq!((mode(&dir), mode(&pem)), (0o700, 0o644));
    std::fs::remove_file(dir.join("extra")).expect("rm");

    // And a CA this account does not own: the id is not the account's.
    assert!(matches!(
        Authority::load_dir_owned_by(&dir, Policy::default(), euid().wrapping_add(1)),
        Err(CaError::KeyFile(KeyFileFault::Owner))
    ));
}

// A group this account is not in cannot be given the CA: the failure names the path, and
// the key is still closed.
#[test]
fn a_ca_cannot_be_given_to_a_group_the_account_is_not_in() {
    if euid() == 0 {
        return;
    }
    let base = scratch("ca_not_member");
    let dir = base.join("ca");
    match ensure_shared_at(&dir, a_group_i_am_not_in()) {
        Err(SignerError::Share { path, .. }) => assert_eq!(path, dir.join("ca.pem")),
        other => panic!("{other:?}"),
    }
    assert_eq!(mode(&dir), 0o700, "the directory stays closed");
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
}

// ---- the flag -------------------------------------------------------------------------

// The flag is optional and named --client-group; the other flags are as before.
#[test]
fn the_flag_is_optional_and_takes_a_group_name() {
    let base = [
        "kbf-signer",
        "--ca-dir",
        "/ca",
        "--socket",
        "/run/s",
        "--frontend-uid",
        "7",
    ];
    let without = Flags::try_parse_from(base).expect("parsed");
    assert_eq!(without.client_group, None);
    let with = Flags::try_parse_from(
        base.into_iter()
            .chain(["--client-group", "kbf-signer-clients"]),
    )
    .expect("parsed");
    assert_eq!(with.client_group.as_deref(), Some("kbf-signer-clients"));
    assert!(Flags::try_parse_from(base.into_iter().chain(["--client-group"])).is_err());
}

fn binary(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kbf-signer"));
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd
}

// A group that does not exist stops the binary with a message and writes nothing: no CA
// directory and no socket directory.
#[test]
fn the_binary_refuses_a_missing_group_and_writes_nothing() {
    let base = scratch("binary_no_group");
    let out = binary(&[
        "--ca-dir",
        support::path_str(&base.join("ca")),
        "--socket",
        support::path_str(&base.join("run").join("sock")),
        "--frontend-uid",
        &euid().to_string(),
        "--client-group",
        "kbf-no-such-group-here",
    ])
    .output()
    .expect("run");
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        said.trim(),
        "kbf-signer: --client-group kbf-no-such-group-here: no such group"
    );
    assert!(!base.join("ca").exists() && !base.join("run").exists());
}

// The binary with a real group of this account: it serves, the directory and the socket are
// the group's at 0750 and 0660, and the CA directory is 0750 with ca.pem 0640 and ca.key
// 0600. A world-accessible socket directory stops it.
#[test]
fn the_binary_shares_the_socket_and_the_root_with_the_group() {
    let (name, gid) = a_named_group_of_mine();
    let ca = make_ca("binary_group");
    let socket = ca.scratch.join("run").join("sock");
    let args = [
        "--ca-dir",
        support::path_str(&ca.dir),
        "--socket",
        support::path_str(&socket),
        "--frontend-uid",
        &euid().to_string(),
        "--client-group",
        &name,
    ];

    let open = ca.scratch.join("open-run");
    std::fs::create_dir(&open).expect("dir");
    chmod(&open, 0o755);
    let mut refused = args;
    let open_socket = open.join("sock");
    refused[3] = support::path_str(&open_socket);
    let out = binary(&refused).output().expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("accessible to others"));
    assert_eq!(mode(&open), 0o755);

    let child = binary(&args).spawn().expect("spawn");
    let mut running = Running(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(status) = running.0.try_wait().expect("try_wait") {
            panic!("the signer exited: {status}");
        }
        if SocketSigner::connect(&socket, Route::Node).is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never came up");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let run = socket.parent().expect("dir");
    assert_eq!((mode(run), gid_of(run)), (0o750, gid));
    assert_eq!((mode(&socket), gid_of(&socket)), (0o660, gid));
    assert_eq!((mode(&ca.dir), gid_of(&ca.dir)), (0o750, gid));
    assert_eq!(
        (mode(&ca.dir.join("ca.pem")), gid_of(&ca.dir.join("ca.pem"))),
        (0o640, gid)
    );
    assert_eq!(mode(&ca.dir.join("ca.key")), 0o600);
}

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

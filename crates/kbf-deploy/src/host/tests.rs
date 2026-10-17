use std::ffi::{OsStr, OsString};
use std::fs;

use super::state::Locked;
use super::state::tests::{A, B, C, full_state, host_root};
use super::*;
use kbf_host::testkit::FakeExec;

/// Catches a commit id check that takes a short or long id, an upper-case one (Git
/// would name the same commit, and a second release directory would hold it), a
/// non-hex character, or a multi-byte character of the right byte length; and serde,
/// parsing or printing that goes around the check.
#[test]
fn a_commit_id_is_exactly_40_lowercase_hex_characters() {
    let id = CommitId::new(B).expect("a commit");
    assert_eq!((id.as_str(), id.to_string()), (B, B.to_owned()));
    assert_eq!(String::from(id.clone()), B);
    assert_eq!(B.parse::<CommitId>(), Ok(id.clone()));
    assert_eq!(
        serde_json::to_string(&id).expect("serializes"),
        format!("\"{B}\"")
    );
    let refused = [
        "",
        &B[..39],
        &format!("{B}0"),
        &B.to_uppercase(),
        &format!("{}g", &B[..39]),
        &format!("{}é", &B[..38]),
        &format!("{} ", &B[..39]),
        // What would make a release path leave `releases/` or name it twice.
        "/",
        ".",
        "..",
        &format!("{}/", &B[..39]),
        &format!("/{}", &B[..39]),
        &format!("{}.", &B[..39]),
        &format!("..{}", &B[..38]),
        &format!("{}\0", &B[..39]),
        &format!("{}A", &B[..39]),
    ];
    for bad in refused {
        assert_eq!(
            CommitId::new(bad),
            Err(NotACommit(bad.to_owned())),
            "{bad:?}"
        );
        let json = serde_json::to_string(bad).expect("a JSON string");
        let err = serde_json::from_str::<CommitId>(&json).expect_err(bad);
        assert!(err.to_string().contains("is not a commit id"), "{err}");
    }
    assert_eq!(
        NotACommit("x".to_owned()).to_string(),
        "\"x\" is not a commit id (40 lowercase hex characters)"
    );
}

/// Catches a layout path that is not a valid host path (the constant would panic on
/// first use), a release, file or pin path built in the wrong directory, and a link
/// target that is absolute (a copied tree would point out of itself).
#[test]
fn the_layout_names_every_path_under_its_directory() {
    use layout::*;
    for p in [
        KBF_DIR,
        RELEASES,
        CURRENT,
        PREVIOUS,
        STATE_DIR,
        INCOMING,
        STAGED,
        DEPLOYED,
        STATE_FILE,
        CONFIG_DIR,
        POLICY,
        PINS,
        INVENTORY_ALLOW,
        LOCK,
    ] {
        assert_eq!(path(p).as_str(), p);
    }
    let a = CommitId::new(A).expect("a commit");
    assert_eq!(
        release_dir(&a).as_str(),
        format!("/usr/local/kbf/releases/{A}")
    );
    let files = [
        (RELEASE_BINARY, "kbf"),
        (RELEASE_UNITS, "units"),
        (RELEASE_MANIFEST, "manifest.json"),
    ];
    for (name, want) in files {
        assert_eq!(
            release_file(&a, name).as_str(),
            format!("/usr/local/kbf/releases/{A}/{want}")
        );
    }
    assert_eq!(
        pin_file(&a).as_str(),
        format!("/etc/kbf-deploy/pins/{A}.sha256")
    );
    assert_eq!(
        release_link(&a),
        std::path::PathBuf::from(format!("releases/{A}"))
    );
}

/// `kbf-deploy host status <args>` with `--root dir`: exit, stdout, stderr.
fn status_cli(dir: &std::path::Path, args: &[&str]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = ["kbf-deploy", "host", "status", "--root"]
        .into_iter()
        .map(OsString::from)
        .chain([dir.as_os_str().to_owned()])
        .chain(args.iter().map(OsString::from))
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut exec = FakeExec::new(0);
    let code = crate::cli::run(argv, &mut exec, &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a status that leaves out a field, prints a commit or deadline from the wrong
/// field, says the link matches when it does not (or exits 0 then), or reads a host
/// with no state as an error. The lines are what an operator reads before a swap and
/// the JSON what a script parses, so both are pinned whole.
#[test]
fn status_prints_the_state_and_the_link_verdict() {
    let (root, dir) = host_root("deploy-host-status-cli");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert_eq!(
        out,
        "state: /var/lib/kbf-deploy/state.json (none yet)\n\
         current: none\n\
         previous: none\n\
         staged: none\n\
         pending confirm: none\n\
         order floor: 0\n\
         current link: missing (matches the state)\n"
    );
    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        "{\"current_link\":{\"kind\":\"missing\",\"matches_state\":true,\"target\":null},\
         \"exists\":false,\"schema\":1,\"state\":{\"current\":null,\"order_floor\":0,\
         \"pending_confirm\":null,\"previous\":null,\"staged\":null},\
         \"state_file\":\"/var/lib/kbf-deploy/state.json\"}\n"
    );

    Locked::acquire(&root)
        .expect("lock")
        .write(&full_state())
        .expect("write");
    let link = dir.join("usr/local/kbf/current");
    std::os::unix::fs::symlink(format!("releases/{A}"), &link).expect("link");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        format!(
            "state: /var/lib/kbf-deploy/state.json\n\
             current: {A}\n\
             previous: {B}\n\
             staged: {C}\n\
             pending confirm: {A} by 1800000000 (unix seconds), else back to {B}\n\
             order floor: 7\n\
             current link: releases/{A} (matches the state)\n"
        )
    );
    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        format!(
            "{{\"current_link\":{{\"kind\":\"link\",\"matches_state\":true,\
             \"target\":\"releases/{A}\"}},\"exists\":true,\"schema\":1,\"state\":{{\
             \"current\":\"{A}\",\"order_floor\":7,\"pending_confirm\":{{\"commit\":\"{A}\",\
             \"deadline\":1800000000,\"previous\":\"{B}\"}},\"previous\":\"{B}\",\
             \"staged\":\"{C}\"}},\"state_file\":\"/var/lib/kbf-deploy/state.json\"}}\n"
        )
    );

    fs::remove_file(&link).expect("remove");
    std::os::unix::fs::symlink(format!("releases/{B}"), &link).expect("link");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        out.ends_with(&format!(
            "current link: releases/{B} (the state says releases/{A})\n"
        )),
        "{out}"
    );

    let mut first = full_state();
    first.current = None;
    first.pending_confirm.as_mut().expect("pending").previous = None;
    Locked::acquire(&root)
        .expect("lock")
        .write(&first)
        .expect("write");
    fs::remove_file(&link).expect("remove");
    fs::create_dir(&link).expect("a directory");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        out.contains(&format!(
            "pending confirm: {A} by 1800000000 (unix seconds), else back to none\n"
        )),
        "{out}"
    );
    assert!(
        out.ends_with("current link: not a symbolic link (the state has no current release)\n"),
        "{out}"
    );
    let (_, out, _) = status_cli(&dir, &["--json"]);
    assert!(
        out.starts_with(
            "{\"current_link\":{\"kind\":\"not_a_link\",\"matches_state\":false,\
             \"target\":null}"
        ),
        "{out}"
    );
}

/// Catches a state file that cannot be parsed shown as no state (an operator would
/// swap on a host whose state is lost), and a relative `--root`.
#[test]
fn status_refuses_a_broken_state_and_a_relative_root() {
    let (_root, dir) = host_root("deploy-host-status-refused");
    fs::write(dir.join("var/lib/kbf-deploy/state.json"), "{").expect("seed");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(
        err.starts_with("REFUSED: ") && err.contains("is not a deploy state"),
        "{err}"
    );

    let argv: Vec<OsString> = ["kbf-deploy", "host", "status", "--root", "relative"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    assert_eq!(code, ExitCode::from(2));
    assert!(String::from_utf8_lossy(&err).contains("not absolute"));
}

/// `kbf-deploy host <args>`: exit, stdout, stderr.
fn host_cli(args: &[&OsStr]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = [OsStr::new("kbf-deploy"), OsStr::new("host")]
        .into_iter()
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a release the user a service runs as could replace: a binary, unit or
/// directory planned writable by group or others, or with no owner (it would be the
/// running user's). The dry run of `host install` is what the operator reads before
/// `--apply`, so it is pinned whole, with root as the owner of every step.
///
/// Mutants seen red: the binary planned 0775, and planned with no owner.
#[test]
fn install_plans_a_release_only_root_can_change() {
    let dir = kbf_host::testkit::scratch("deploy-host-install-cli");
    for sub in ["run", "var/lib", "usr/local"] {
        fs::create_dir_all(dir.join(sub)).expect("seed");
    }
    let from = kbf_host::testkit::scratch("deploy-host-install-cli-src");
    fs::create_dir(from.join("units")).expect("units");
    fs::write(from.join("kbf"), "kbf A").expect("kbf");
    fs::write(from.join("units/kbf.slice"), "[Slice]\n").expect("unit");
    let args = ["install", "--commit", A, "--from"].map(OsStr::new);
    let args = [
        &args[..],
        &[from.as_os_str(), OsStr::new("--root"), dir.as_os_str()],
    ]
    .concat();
    let (code, out, err) = host_cli(&args);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"kbf A"));
    let r = format!("/usr/local/kbf/releases/{A}");
    assert_eq!(
        out,
        format!(
            "release {A}: {r} (5 bytes, sha256 {sha}), 1 unit(s)\n\
             DRY-RUN: mkdir /var/lib/kbf-deploy (0755, owner 0:0)\n\
             DRY-RUN: mkdir /usr/local/kbf (0755, owner 0:0)\n\
             DRY-RUN: mkdir /usr/local/kbf/releases (0755, owner 0:0)\n\
             DRY-RUN: mkdir {r} (0755, owner 0:0)\n\
             DRY-RUN: mkdir {r}/units (0755, owner 0:0)\n\
             DRY-RUN: write {r}/kbf (0755, owner 0:0, 5 bytes)\n\
             DRY-RUN: write {r}/units/kbf.slice (0644, owner 0:0, 8 bytes)\n\
             dry run: nothing changed. Re-run with --apply.\n\
             links: current none -> {A}, previous none -> none\n\
             DRY-RUN: link /usr/local/kbf/current -> releases/{A}\n\
             dry run: nothing changed. Re-run with --apply.\n"
        )
    );
}

/// Catches `rollback` and `gc` not reaching their verbs from the command line, a refusal
/// that exits 0 or prints nothing, `--apply` read as a dry run (or the reverse), and a
/// commit id the parser takes without the check.
#[test]
fn rollback_and_gc_run_from_the_command_line() {
    let (_root, dir) = host_root("deploy-host-verbs-cli");
    let root = [OsStr::new("--root"), dir.as_os_str()];
    let (code, out, err) = host_cli(&[&[OsStr::new("rollback")], &root[..]].concat());
    assert_eq!((code, out.as_str()), (ExitCode::FAILURE, ""));
    assert_eq!(err, "kbf-deploy: no previous release to roll back to\n");
    let gc = ["gc", "--keep", "2"].map(OsStr::new);
    let (code, out, _) = host_cli(&[&gc[..], &root[..]].concat());
    assert_eq!(
        (code, out.as_str()),
        (
            ExitCode::SUCCESS,
            "keeping: none; removing: none\nnothing to do\n"
        )
    );

    let bad = ["install", "--commit", "HEAD", "--from", "/x"].map(OsStr::new);
    let (code, _, err) = host_cli(&bad);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.contains("\"HEAD\" is not a commit id"), "{err}");

    let args = |apply| ApplyArgs {
        apply,
        root: Root::host(),
    };
    assert_eq!(
        (args(false).mode(), args(true).mode()),
        (Mode::DryRun, Mode::Apply)
    );
}

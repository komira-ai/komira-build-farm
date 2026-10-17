//! How the driver reports an input blob the CAS does not hold, against the stand-in
//! `crun` (`support/fake.rs`). `docs/design/daemon.md` asks for
//! `RuntimeError::MissingBlob` with the blob's `hash/size`, which the lease reports as
//! `FAILED_PRECONDITION` with a `MISSING` violation, the answer that asks a client to
//! upload the blob and retry. The native driver already reports it that way. (The
//! server does not forward that violation to the client yet; see
//! `docs/design/failure-classes.md`.)
//!
//! A module of `fake_crun.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use kbf_daemon::cas::{digest_of, label};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_proto::reapi::{Action, Digest, Directory, DirectoryNode, FileNode};
use kbf_types::Resources;
use prost::Message;

use crate::support::fake::{Fake, image};
use crate::support::{Spec, store_action, work};

/// A runnable Action stored in `fake`'s CAS, with `change` applied to it.
fn action_with(fake: &Fake, change: impl FnOnce(&mut Action)) -> Digest {
    let full = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let mut action =
        Action::decode(fake.cas.blob(&full).expect("action").as_slice()).expect("Action");
    change(&mut action);
    fake.cas.insert(action.encode_to_vec())
}

/// Catches a missing Action, Command, input root, input file or input subdirectory
/// reported as anything but `MissingBlob` naming that blob: as the farm's failure
/// (`Failed`, the defect of issue #130: the client gets INTERNAL and never uploads),
/// as the client's malformed action (`Invalid`), or as a missing blob that names
/// another digest (the client would upload the wrong one). Each case also leaves the
/// lease clean and never reaches `crun run`.
#[tokio::test]
async fn a_missing_input_blob_is_reported_as_missing_with_its_digest() {
    let fake = Fake::new("missing-blobs");
    let absent = |what: &str| digest_of(format!("never stored: {what}").as_bytes());

    let no_action = absent("action");
    let no_command = absent("command");
    let command_gone = action_with(&fake, |a| a.command_digest = Some(no_command.clone()));
    let no_root = absent("root");
    let root_gone = action_with(&fake, |a| a.input_root_digest = Some(no_root.clone()));
    let no_file = absent("file");
    let file_gone = action_with(&fake, |a| {
        let root = Directory {
            files: vec![FileNode {
                name: "data".to_owned(),
                digest: Some(no_file.clone()),
                ..FileNode::default()
            }],
            ..Directory::default()
        };
        a.input_root_digest = Some(fake.cas.insert(root.encode_to_vec()));
    });
    let no_dir = absent("dir");
    let dir_gone = action_with(&fake, |a| {
        let root = Directory {
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(no_dir.clone()),
            }],
            ..Directory::default()
        };
        a.input_root_digest = Some(fake.cas.insert(root.encode_to_vec()));
    });

    let cases = [
        ("action", no_action.clone(), no_action),
        ("command", command_gone, no_command),
        ("input root", root_gone, no_root),
        ("input file", file_gone, no_file),
        ("input directory", dir_gone, no_dir),
    ];
    for (seq, (what, action, missing)) in (1..).zip(cases) {
        let outcome = fake
            .runtime
            .run(work(seq, action, Resources::default()))
            .await;
        let want = label(&missing);
        assert!(
            matches!(&outcome, Err(RuntimeError::MissingBlob(blob)) if *blob == want),
            "{what}: want MissingBlob({want}), got {outcome:?}"
        );
        fake.assert_clean(seq);
    }
    assert_eq!(fake.starts(), 0);
}

/// Catches the opposite defect at the two call sites that fetch messages: an Action or
/// a Command the CAS holds but that does not decode, reported as `MissingBlob`. The
/// client would upload the same bytes and retry, forever. It is the client's malformed
/// action (`Invalid`). The unit test pins this at `tree_error` only; this pins which
/// mapping the Action and Command fetches use.
#[tokio::test]
async fn an_undecodable_action_or_command_is_invalid_not_missing() {
    let fake = Fake::new("undecodable-messages");
    let junk = fake.cas.insert(vec![0xff; 7]);
    let command_junk = action_with(&fake, |a| a.command_digest = Some(junk.clone()));

    let cases = [("action", junk.clone()), ("command", command_junk)];
    for (seq, (what, action)) in (1..).zip(cases) {
        let outcome = fake
            .runtime
            .run(work(seq, action, Resources::default()))
            .await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Invalid(why)) if why.contains("does not decode")),
            "{what}: want Invalid(... does not decode ...), got {outcome:?}"
        );
        fake.assert_clean(seq);
    }
    assert_eq!(fake.starts(), 0);
}

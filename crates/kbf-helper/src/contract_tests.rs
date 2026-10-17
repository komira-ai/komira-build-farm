//! The two ends of the inbox agree: what `kbf-update`'s client writes is what this helper
//! and the installer read. Each test runs the real writer and the real reader.

use std::fs;
use std::path::Path;

use kbf_installer::inbox::{Entry, read_capped};
use kbf_installer::offer::{OFFER_FILE, Offer as Read};
use kbf_installer::testkit::A;
use kbf_update::inbox::Inbox;
use kbf_update::{Component, Offer, Order, Verb};
use sha2::{Digest, Sha256};

use crate::order;

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn scratch(name: &str) -> std::path::PathBuf {
    kbf_host::testkit::scratch(&format!("helper-contract-{name}"))
}

fn component(name: &str, data: &[u8], attestation: &str) -> Component {
    Component {
        name: name.to_owned(),
        sha256: sha(data),
        size: data.len() as u64,
        attestation: attestation.as_bytes().to_vec(),
    }
}

/// Writes `offer` into a new inbox the way the client does (each tarball, then `finish`),
/// and returns the entry directory.
async fn written(dir: &Path, offer: &Offer, tarballs: &[&[u8]]) -> std::path::PathBuf {
    use tokio::io::AsyncWriteExt;
    let inbox = Inbox::open(dir).expect("inbox");
    let staging = inbox.begin(offer).expect("begin");
    for (c, data) in offer.components.iter().zip(tarballs) {
        let mut f = staging
            .create(&kbf_update::inbox::tarball_name(offer, c))
            .await
            .expect("create");
        f.write_all(data).await.expect("write");
        f.flush().await.expect("flush");
    }
    staging.finish(offer).expect("finish");
    dir.join("inbox")
        .join(kbf_update::inbox::dir_name(offer.generation, &offer.commit))
}

/// Catches the two ends disagreeing about an offer: the installer parses `offer.json` the
/// client wrote, every file in the entry is one the offer names (the stage refuses a
/// stranger), and the sha256 of each named file is the file's.
///
/// Mutant seen red: the client writing its own schema (`generation`, `platform`, one
/// attestation file per component): the installer refuses it as not an offer.
#[tokio::test]
async fn the_installer_reads_the_offer_the_client_writes() {
    let dir = scratch("offer");
    let (kbf, daemon) = (b"kbf tarball".as_slice(), b"daemon tarball".as_slice());
    let offer = Offer {
        generation: 4,
        commit: A.to_owned(),
        platform: "linux-x86_64".to_owned(),
        components: vec![
            component("kbf", kbf, "{\"bundle\":1}"),
            component("kbf-daemon", daemon, "{\"bundle\":2}\n"),
        ],
    };
    let entry_dir = written(&dir, &offer, &[kbf, daemon]).await;
    let entry = Entry::open(&entry_dir).expect("opens");
    let mut file = entry.open_file(OFFER_FILE, 64 * 1024).expect("offer.json");
    let bytes = read_capped(&mut file, 64 * 1024, OFFER_FILE).expect("reads");
    let read = Read::parse(&bytes).expect("the installer accepts the client's offer");
    assert_eq!(read.commit.as_str(), A);
    let mut named: Vec<String> = read.names().into_iter().collect();
    let mut held = entry.names().expect("names");
    named.sort();
    held.sort();
    assert_eq!(
        held, named,
        "the entry holds exactly the files the offer names"
    );
    let bundle = fs::read(entry_dir.join(&read.bundle.file)).expect("bundle");
    assert_eq!(sha(&bundle), read.bundle.sha256);
    assert_eq!(
        String::from_utf8(bundle).expect("text"),
        "{\"bundle\":1}\n{\"bundle\":2}\n",
        "one attestation per line, each ended"
    );
    for c in &read.components {
        let data = fs::read(entry_dir.join(&c.tarball.file)).expect("tarball");
        assert_eq!(sha(&data), c.tarball.sha256);
    }
    assert_eq!(read.components.len(), 2);
}

/// Catches the two ends disagreeing about an order file: the client's file is read by the
/// helper's parser, whatever the verb, and it is found by the helper's name rule.
#[test]
fn the_helper_reads_the_orders_the_client_writes() {
    let dir = scratch("order");
    fs::create_dir_all(&dir).expect("dir");
    let inbox = Inbox::open(&dir).expect("inbox");
    for (id, verb) in [(7, Verb::Activate), (8, Verb::Confirm), (9, Verb::Revert)] {
        let sent = Order {
            order_id: id,
            epoch: 2,
            verb: Some(verb),
            commit: A.to_owned(),
            window_s: 600,
        };
        inbox.write_order(&sent, verb.name()).expect("writes");
        let name = format!("{id:020}.json");
        assert_eq!(order::id_of(&name), Some(id));
        let bytes = fs::read(dir.join("inbox/orders").join(&name)).expect("file");
        let read = order::parse(id, &bytes).expect("the helper accepts the client's order");
        assert_eq!(read.commit.as_str(), A);
        assert_eq!(read.window_s, 600);
        let want = match verb {
            Verb::Activate => order::Verb::Activate,
            Verb::Confirm => order::Verb::Confirm,
            Verb::Revert => order::Verb::Revert,
        };
        assert_eq!(read.verb, want);
    }
}

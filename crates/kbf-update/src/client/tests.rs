//! The client's behaviour, through its handle: offers, fetches, orders, status. Each test
//! names the defect it catches; the planted mutant for each is listed in the pull request.

use std::fs;

use sha2::Digest;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::harness::{PROMPT, Rig, at, status_json};
use super::*;
use crate::fixtures::{FakeSource, FakeSpace, PLATFORM, bytes, commit, offer_of, scratch};
use crate::types::{Component, Staged};

/// Catches an offer taken at a generation no higher than one already seen: a stale or
/// repeated offer fetched again, and the newer offer not fetched. (Mutant: act on any
/// offer.)
#[tokio::test]
async fn only_a_higher_generation_is_fetched() {
    let mut rig = Rig::new("generation");
    let five = bytes(450, 1);
    rig.offer(5, &five);
    let done = rig.until(at(5, State::Idle)).await;
    assert_eq!(done.refusal, "");
    assert_eq!(rig.entries(), vec![format!("{:020}-{}", 5, commit(5))]);
    let fetched = rig.source.fetches.load(Ordering::SeqCst);
    assert_eq!(fetched, 1);

    // The same generation again, and a lower one: neither is fetched.
    rig.handle.offer(offer_of(5, &five));
    let four = bytes(300, 2);
    rig.offer(4, &four);
    rig.handle.refresh();
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), fetched);
    assert_eq!(rig.entries().len(), 1);
    assert_eq!(rig.rx.borrow().offered_generation, 5);

    // A higher one is.
    rig.offer(6, &bytes(200, 3));
    rig.until(at(6, State::Idle)).await;
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), fetched + 1);
    assert_eq!(rig.entries().len(), 2);
}

/// Catches an offer that cannot be acted on being fetched anyway, or refused without its
/// reason: the wrong platform, a commit that is not 40 lowercase hex, a component that is
/// not `kbf` or `kbf-daemon`, and one over the size cap. Nothing is fetched and nothing
/// is left in the inbox. (Mutant: skip the check.)
#[tokio::test]
async fn an_offer_that_fails_its_checks_is_refused_without_a_fetch() {
    let data = bytes(100, 1);
    let good = offer_of(1, &data);
    let bad = |edit: &dyn Fn(&mut Offer)| {
        let mut offer = good.clone();
        edit(&mut offer);
        offer
    };
    let cases: Vec<(Offer, &str)> = vec![
        (bad(&|o| o.platform = "darwin-arm64".into()), "platform"),
        (bad(&|o| o.commit = "XYZ".into()), "40 lowercase hex"),
        (
            bad(&|o| o.components[0].name = "kbf-server".into()),
            "not one this node installs",
        ),
        (
            bad(&|o| o.components[0].size = crate::types::MAX_COMPONENT_BYTES + 1),
            "size",
        ),
    ];
    for (generation, (mut offer, expect)) in (1..).zip(cases) {
        let mut rig = Rig::new("refused");
        rig.source.serve(&data);
        offer.generation = generation;
        rig.handle.offer(offer);
        let status = rig.until(|s| s.state == State::Refused).await;
        assert!(status.refusal.contains(expect), "{}", status.refusal);
        assert_eq!(status.offered_generation, generation);
        assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 0, "{expect}");
        assert_eq!(rig.entries(), Vec::<String>::new(), "{expect}");
    }
}

/// Catches a refusal that outlives the next good offer, and a newer offer that does not
/// replace one the worker had not started.
#[tokio::test]
async fn a_good_offer_clears_a_refusal() {
    let mut rig = Rig::new("clears");
    let data = bytes(100, 1);
    rig.source.serve(&data);
    let mut wrong = offer_of(1, &data);
    wrong.platform = "darwin-arm64".into();
    rig.handle.offer(wrong);
    rig.until(|s| s.state == State::Refused).await;
    rig.offer(2, &data);
    let done = rig.until(at(2, State::Idle)).await;
    assert_eq!(done.refusal, "");
}

/// Catches a stream longer than the offer being read to its end or written: the fetch
/// stops at the first chunk past the offered size, the source is not drained, and no
/// directory (partial or visible) is left. (Mutant: no cap.)
#[tokio::test]
async fn a_stream_longer_than_the_offer_stops_at_the_size() {
    let mut rig = Rig::new("long");
    let mut offer = offer_of(1, &bytes(1000, 1));
    // The service holds 100 kB under the digest the offer names.
    rig.source
        .serve_as(&offer.components[0].sha256, &bytes(100_000, 1));
    offer.components[0].size = 1000;
    rig.handle.offer(offer);
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(
        status.refusal.contains("more than the 1000 bytes"),
        "{}",
        status.refusal
    );
    // 1000 bytes in chunks of 100 is ten chunks; the eleventh crosses the size.
    assert_eq!(rig.source.chunks.load(Ordering::SeqCst), 11);
    assert_eq!(rig.entries(), Vec::<String>::new());
}

/// Catches a stream that ends before the offered size being accepted.
#[tokio::test]
async fn a_short_stream_is_refused() {
    let mut rig = Rig::new("short");
    let offer = offer_of(1, &bytes(1000, 1));
    rig.source
        .serve_as(&offer.components[0].sha256, &bytes(400, 1));
    rig.handle.offer(offer);
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(
        status.refusal.contains("served 400 of the 1000"),
        "{}",
        status.refusal
    );
    assert_eq!(rig.entries(), Vec::<String>::new());
}

/// Catches bytes of the right length but the wrong content being made visible: the
/// digest is compared before the rename, so the inbox shows nothing. (Mutant: rename
/// before checking.)
#[tokio::test]
async fn a_digest_mismatch_leaves_no_directory() {
    let mut rig = Rig::new("mismatch");
    let offer = offer_of(1, &bytes(1000, 1));
    rig.source
        .serve_as(&offer.components[0].sha256, &bytes(1000, 9));
    rig.handle.offer(offer);
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(status.refusal.contains("hash to"), "{}", status.refusal);
    assert_eq!(rig.entries(), Vec::<String>::new());
}

/// Catches a blob the service does not hold being retried for ever: it is a refusal.
#[tokio::test]
async fn a_blob_the_frontend_lacks_is_refused_once() {
    let mut rig = Rig::new("missing");
    rig.handle.offer(offer_of(1, &bytes(100, 1)));
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(
        status.refusal.contains("does not hold"),
        "{}",
        status.refusal
    );
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 1);
}

/// Catches a transport error (on connecting, and in the middle of a stream) that ends
/// the offer instead of being tried again from the first byte. (Mutant: no retry.)
#[tokio::test]
async fn a_transport_error_is_retried() {
    let mut rig = Rig::new("retry");
    rig.source.refuse_calls(2);
    rig.source.break_streams(1);
    rig.offer(1, &bytes(450, 1));
    let status = rig.until(at(1, State::Idle)).await;
    assert_eq!(status.refusal, "");
    // Two refused calls, one stream that broke, and the one that worked.
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 4);
    assert_eq!(rig.entries().len(), 1);
}

/// Catches the state not saying FETCHING while a fetch runs, and an offer that is
/// abandoned for a newer one still finishing: the first offer's stream is held half
/// way, the second arrives, and only the second reaches the inbox. (Mutant: no
/// abandon, which would wait on the held stream for ever.)
#[tokio::test]
async fn a_newer_generation_abandons_the_fetch_in_flight() {
    let mut rig = Rig::new("abandon");
    let gate = rig.source.stall();
    let started = Arc::clone(&rig.source.started);
    rig.offer(1, &bytes(450, 1));
    started.notified().await;
    let fetching = rig.until(|s| s.state == State::Fetching).await;
    assert_eq!(fetching.offered_generation, 1);
    assert!(!fetching.gated);

    // The held stream never gets a permit again; the second offer's does.
    let second = bytes(450, 2);
    rig.source.serve(&second);
    rig.handle.offer(offer_of(2, &second));
    gate.add_permits(1000);
    rig.until(at(2, State::Idle)).await;
    let names = rig.entries();
    assert_eq!(names, vec![format!("{:020}-{}", 2, commit(2))]);
}

/// Catches an inbox that grows without bound: the two newest offers stay. (Mutant: no
/// pruning.)
#[tokio::test]
async fn the_two_newest_offers_stay() {
    let mut rig = Rig::new("prune");
    for generation in 1..=3 {
        rig.offer(
            generation,
            &bytes(150, u8::try_from(generation).expect("small")),
        );
        rig.until(at(generation, State::Idle)).await;
    }
    assert_eq!(
        rig.entries(),
        vec![
            format!("{:020}-{}", 2, commit(2)),
            format!("{:020}-{}", 3, commit(3))
        ]
    );
}

/// Catches the newest generation living in memory only: after a restart the same and
/// older offers are not fetched again, and a newer one is. (Mutant: in memory only.)
#[tokio::test]
async fn the_generation_survives_a_restart() {
    let mut rig = Rig::new("restart");
    let five = bytes(300, 1);
    rig.offer(5, &five);
    rig.until(at(5, State::Idle)).await;
    // Pruning and the helper may take the directory away; the floor stays.
    fs::remove_dir_all(rig.inbox().join(format!("{:020}-{}", 5, commit(5)))).expect("remove");
    let mut rig = rig.restart();
    assert_eq!(rig.rx.borrow().offered_generation, 5);
    rig.handle.offer(offer_of(5, &five));
    rig.handle.offer(offer_of(4, &five));
    assert_eq!(rig.rx.borrow().state, State::Idle);
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 1);
    rig.offer(6, &bytes(300, 2));
    rig.until(at(6, State::Idle)).await;
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 2);
}

/// Catches a half-fetched offer counting as seen: a client that dies mid-fetch leaves
/// only a `.partial-*` entry, the inbox shows no half directory, the next start removes
/// it, and the same offer is fetched in full. (Mutant: write the final name directly,
/// which shows a half directory; or persist the generation before the fetch, which
/// leaves the offer unfetched.)
#[tokio::test]
async fn a_crash_mid_fetch_leaves_only_a_partial_entry() {
    let rig = Rig::new("crash");
    let _held = rig.source.stall();
    let started = Arc::clone(&rig.source.started);
    let data = bytes(450, 1);
    rig.offer(1, &data);
    started.notified().await;
    rig.task.abort();
    let names = rig.entries();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with(".partial-"), "{names:?}");

    // The service no longer stalls; a new client starts, sweeps, and fetches.
    rig.source.unstall();
    let mut rig = rig.restart();
    assert_eq!(rig.entries(), Vec::<String>::new());
    rig.handle.offer(offer_of(1, &data));
    rig.until(at(1, State::Idle)).await;
    assert_eq!(rig.entries(), vec![format!("{:020}-{}", 1, commit(1))]);
}

/// Catches a directory made visible before its files are complete, and a file name or
/// mode other than the one the helper reads: the exact contents of the frozen layout.
#[tokio::test]
async fn the_inbox_directory_has_the_frozen_layout() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new("layout");
    let data = bytes(450, 1);
    let mut offer = offer_of(3, &data);
    offer.components.push(Component {
        name: "kbf-daemon".into(),
        ..offer.components[0].clone()
    });
    rig.source.serve(&data);
    rig.handle.offer(offer.clone());
    rig.until(at(3, State::Idle)).await;
    let name = format!("{:020}-{}", 3, commit(3));
    let dir = rig.inbox().join(&name);
    let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o750);
    let mut files: Vec<String> = fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let c = commit(3);
    let expected = vec![
        format!("kbf-{c}-{PLATFORM}.tar.gz"),
        format!("kbf-daemon-{c}-{PLATFORM}.tar.gz"),
        "offer.json".to_owned(),
        "provenance.jsonl".to_owned(),
    ];
    assert_eq!(files, expected);
    assert_eq!(
        fs::read(dir.join(format!("kbf-{c}-{PLATFORM}.tar.gz"))).expect("tarball"),
        data
    );
    // One bundle: each component's attestation on a line of its own.
    let mut bundle = Vec::new();
    for component in &offer.components {
        bundle.extend_from_slice(&component.attestation);
        if bundle.last() != Some(&b'\n') {
            bundle.push(b'\n');
        }
    }
    assert_eq!(
        fs::read(dir.join("provenance.jsonl")).expect("bundle"),
        bundle
    );
    let bundle_sha = hex::encode(sha2::Sha256::digest(&bundle));
    let sha = &offer.components[0].sha256;
    let one =
        |n: &str, t: &str| format!("{{\"name\":\"{n}\",\"file\":\"{t}\",\"sha256\":\"{sha}\"}}");
    let want = format!(
        "{{\"schema\":1,\"commit\":\"{c}\",\"bundle\":{{\"file\":\"provenance.jsonl\",\"sha256\":\"{bundle_sha}\"}},\"components\":[{},{}]}}",
        one("kbf", &format!("kbf-{c}-{PLATFORM}.tar.gz")),
        one("kbf-daemon", &format!("kbf-daemon-{c}-{PLATFORM}.tar.gz")),
    );
    assert_eq!(
        fs::read_to_string(dir.join("offer.json")).expect("offer.json"),
        want
    );
}

/// Catches an offer fetched onto a disk that cannot hold it (less than three times its
/// size free), and one fetched when the free space cannot be read.
#[tokio::test]
async fn a_full_disk_refuses_before_fetching() {
    let mut rig = Rig::new("disk");
    *rig.space.0.lock().expect("lock") = Ok(2999);
    rig.offer(1, &bytes(1000, 1));
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(
        status.refusal.contains("2999 bytes of disk are free"),
        "{}",
        status.refusal
    );
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 0);

    // Exactly three times is enough.
    *rig.space.0.lock().expect("lock") = Ok(3000);
    rig.offer(2, &bytes(1000, 1));
    rig.until(at(2, State::Idle)).await;

    *rig.space.0.lock().expect("lock") = Err(std::io::ErrorKind::PermissionDenied);
    rig.offer(3, &bytes(1000, 3));
    let status = rig.until(at(3, State::Refused)).await;
    assert!(
        status.refusal.contains("cannot tell how much disk"),
        "{}",
        status.refusal
    );
}

/// Catches an inbox that cannot be written being reported as anything but a refusal with
/// the reason, and a partial directory left behind by it: the directory vanishes at the
/// start of the fetch, and at its end.
#[tokio::test]
async fn an_inbox_that_cannot_be_written_refuses_and_cleans_up() {
    let mut rig = Rig::new("inbox-fails");
    // Gone before the file is made.
    let inbox = rig.inbox();
    rig.source.on_fetch(move || {
        for e in fs::read_dir(&inbox).expect("inbox").flatten() {
            if e.file_name().to_string_lossy().starts_with(".partial-") {
                fs::remove_dir_all(e.path()).expect("remove");
            }
        }
    });
    rig.offer(1, &bytes(300, 1));
    let status = rig.until(|s| s.state == State::Refused).await;
    assert!(
        status.refusal.contains("cannot be written"),
        "{}",
        status.refusal
    );

    // Gone before the directory is completed.
    let inbox = rig.inbox();
    rig.source.on_fetch(|| {});
    rig.source.on_end(move || {
        for e in fs::read_dir(&inbox).expect("inbox").flatten() {
            if e.file_name().to_string_lossy().starts_with(".partial-") {
                fs::remove_dir_all(e.path()).expect("remove");
            }
        }
    });
    rig.offer(2, &bytes(300, 2));
    let status = rig.until(at(2, State::Refused)).await;
    assert!(
        status
            .refusal
            .contains("cannot complete the inbox directory"),
        "{}",
        status.refusal
    );
    assert_eq!(rig.entries(), Vec::<String>::new());

    // The inbox itself gone: no directory can be started.
    fs::remove_dir_all(rig.inbox()).expect("remove inbox");
    rig.offer(3, &bytes(300, 3));
    let status = rig.until(at(3, State::Refused)).await;
    assert!(
        status.refusal.contains("cannot start a directory"),
        "{}",
        status.refusal
    );
}

/// Catches a client that cannot record its generation failing the offer: the directory
/// is in the inbox, which is what a restart reads.
#[tokio::test]
async fn an_unrecordable_generation_does_not_fail_the_offer() {
    let mut rig = Rig::new("unrecordable");
    // The temporary file's name is a directory: the state file cannot be replaced.
    fs::create_dir(rig.dir.join("client.json.tmp")).expect("block");
    rig.offer(1, &bytes(300, 1));
    let status = rig.until(at(1, State::Idle)).await;
    assert_eq!(status.refusal, "");
    assert_eq!(rig.entries().len(), 1);
    // A restart finds the directory and does not fetch it again.
    fs::remove_dir(rig.dir.join("client.json.tmp")).expect("unblock");
    let mut rig = rig.restart();
    rig.handle.offer(offer_of(1, &bytes(300, 1)));
    rig.until(at(1, State::Idle)).await;
    assert_eq!(rig.source.fetches.load(Ordering::SeqCst), 1);
}

/// Catches a refusal of a superseded offer being attributed to the offer that replaced it.
#[tokio::test]
async fn a_late_refusal_of_a_superseded_offer_is_not_shown() {
    // No worker runs: the offers wait, and the test plays the worker's part.
    let dir = scratch("late-refusal");
    let config = Rig::config(&dir, &FakeSpace::plenty());
    let (handle, _worker) = start(config, FakeSource::new(10) as Arc<dyn Source>).expect("start");
    let status = handle.status();
    let data = bytes(100, 1);
    handle.offer(offer_of(1, &data));
    handle.offer(offer_of(2, &data));
    let shared = &handle.shared;
    shared.end_fetch(&offer_of(1, &data), Err(Stop::Refused("old".into())));
    assert_eq!(status.borrow().refusal, "");
    shared.end_fetch(&offer_of(2, &data), Err(Stop::Refused("new".into())));
    assert_eq!(status.borrow().refusal, "new");

    // A newer offer that is refused replaces one the worker has not started.
    handle.offer(offer_of(3, &data));
    let mut wrong = offer_of(4, &data);
    wrong.platform = "darwin-arm64".into();
    handle.offer(wrong);
    assert!(shared.begin_fetch().is_none());
}

// --- the helper's status file -------------------------------------------------------

/// Catches the state table: no file is MANUAL with no helper; garbage, a schema this
/// build does not read, an unknown state and a commit that is not hex are REFUSED with the
/// reason; each state a helper reports maps to its own; and STAGED needs the staged commit
/// to be the one offered.
#[tokio::test]
async fn the_helpers_status_maps_to_the_state() {
    let mut rig = Rig::bare("status");
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Manual);
    assert!(!status.helper_present);
    assert_eq!(status.running_commit, "f".repeat(12));

    let refused = |rig: &Rig, text: &str, contains: &str| {
        rig.write_status(text);
        let status = rig.rx.borrow().clone();
        assert_eq!(status.state, State::Refused, "{text}");
        assert!(status.refusal.contains(contains), "{}", status.refusal);
        assert!(status.helper_present, "{text}");
    };
    refused(&rig, "{not json", "status.json");
    refused(
        &rig,
        &status_json("idle", "", "", 0).replace("\"schema\":1", "\"schema\":2"),
        "schema 2",
    );
    refused(&rig, &status_json("rebooting", "", "", 0), "unknown state");
    refused(&rig, &status_json("idle", "XYZ", "", 0), "installed_commit");

    // The offered commit is the first generation's; the helper stages another.
    rig.write_status(&status_json("idle", "", "", 0));
    rig.offer(1, &bytes(200, 1));
    rig.until(at(1, State::Idle)).await;
    let offered = commit(1);
    let (other, installed_commit) = ("9".repeat(40), "1".repeat(40));
    let table = [
        ("idle", "", "", State::Idle),
        ("staged", "", offered.as_str(), State::Staged),
        ("staged", "", other.as_str(), State::Idle),
        ("staged", "", "", State::Idle),
        (
            "activated_unconfirmed",
            installed_commit.as_str(),
            "",
            State::ActivatedUnconfirmed,
        ),
        ("confirmed", "", "", State::Confirmed),
        ("reverted", "", "", State::Reverted),
        ("refused", "", "", State::Refused),
    ];
    for (helper, installed, staged, expected) in table {
        rig.write_status(&status_json(helper, installed, staged, 4));
        let status = rig.rx.borrow().clone();
        assert_eq!(status.state, expected, "{helper} {staged}");
        assert!(status.helper_present);
        assert_eq!(
            status.staged,
            Some(Staged {
                protocol: 2,
                min_protocol: 1,
                state_schema: 3
            })
        );
        assert_eq!(
            status.installed_commit.as_deref(),
            (!installed.is_empty()).then_some(installed)
        );
        assert_eq!(
            status.staged_commit.as_deref(),
            (!staged.is_empty()).then_some(staged)
        );
        let why = if expected == State::Refused {
            "why not"
        } else {
            ""
        };
        assert_eq!(status.refusal, why, "{helper}");
    }

    // A status file that is not a file is unreadable, not missing.
    fs::remove_file(rig.dir.join("status.json")).expect("remove");
    fs::create_dir(rig.dir.join("status.json")).expect("dir");
    rig.handle.refresh();
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Refused);
    assert!(status.refusal.contains("cannot read"), "{}", status.refusal);
}

/// Catches the poll loop not running: a status file written while the worker runs is
/// seen without anyone asking.
#[tokio::test]
async fn the_worker_watches_the_status_file() {
    let dir = scratch("poll");
    let space = FakeSpace::plenty();
    let mut config = Rig::config(&dir, &space);
    config.poll_every = Duration::from_millis(5);
    let source = FakeSource::new(100);
    let (handle, worker) = start(config, source as Arc<dyn Source>).expect("start");
    let task = tokio::spawn(worker.run());
    let mut rx = handle.status();
    fs::write(dir.join("status.json"), status_json("confirmed", "", "", 0)).expect("write");
    tokio::time::timeout(PROMPT, rx.wait_for(|s| s.state == State::Confirmed))
        .await
        .expect("seen in time")
        .expect("runs");
    task.abort();
}

// --- starting ---

/// Catches a client that starts on a state file it cannot read (a guessed zero would
/// let an old offer or order in again) or in a directory it cannot make.
#[tokio::test]
async fn a_client_does_not_start_on_state_it_cannot_read() {
    let why = |dir: &Path| {
        let source = FakeSource::new(10) as Arc<dyn Source>;
        let config = Rig::config(dir, &FakeSpace::plenty());
        start(config, source)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    };

    let dir = scratch("start-garbage");
    fs::write(dir.join("client.json"), "not json").expect("write");
    assert!(why(&dir).contains("not the client's state file"));

    let dir = scratch("start-unreadable");
    fs::create_dir(dir.join("client.json")).expect("dir");
    assert!(why(&dir).contains("cannot read"));

    let dir = scratch("start-blocked");
    fs::write(dir.join("file"), "").expect("write");
    assert!(why(&dir.join("file").join("under")).contains("update directory"));

    // And a good one starts.
    assert_eq!(why(&scratch("start-fine")), "");
}

/// Catches the defaults of a configuration: the real file system's free space, and a
/// backoff that grows.
#[tokio::test]
async fn the_default_configuration_reads_the_real_disk() {
    let dir = scratch("defaults");
    let config = Config::new(dir.clone(), PLATFORM.to_owned(), "a".repeat(12));
    assert!(config.space.space(&dir).expect("statvfs").available > 0);
    assert!(config.backoff.first < config.backoff.max);
    assert_eq!(Backoff::default().first, Duration::from_secs(1));
}

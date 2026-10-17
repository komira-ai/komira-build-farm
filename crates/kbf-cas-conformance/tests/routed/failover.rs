//! `read_to` fails over as `get` does: a copy that is not the blob, that breaks part way
//! or that is not there sends the read to the next owner, the read-repair hook is told
//! what `get` tells it, and the file holds the blob or what it held before, also when the
//! read is dropped part way.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, digest_of, label};
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

use super::owners::{Owners, Told, parsed};
use super::support::scratch;

/// What a file holds before a read writes into it: a read writes from the file's offset,
/// and must leave these bytes, and the offset, as they were when it fails.
const HEAD: &[u8] = b"head";

/// What one owner does when it is read.
#[derive(Clone, Copy, Debug)]
enum Serves {
    /// Sends the blob.
    Good,
    /// Sends bytes of the blob's length that are not the blob.
    Lie,
    /// Sends one byte fewer than the blob.
    Short,
    /// Sends the blob and more.
    Long,
    /// Sends one chunk of the blob, then fails.
    Breaks,
    /// Does not hold it.
    Missing,
    /// Is down.
    Down,
}

/// A blob of `len` bytes that are not all alike.
fn blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// `blob` with its first byte changed.
fn lie(blob: &[u8]) -> Vec<u8> {
    let mut lie = blob.to_vec();
    lie[0] = lie[0].wrapping_add(1);
    lie
}

/// Owners of `digest` (the digest of `blob`) that do as `plan` says, the first in read
/// order first.
fn farm(blob: &[u8], plan: &[Serves]) -> Owners {
    let digest = digest_of(blob);
    let lie = lie(blob);
    let long = [blob, b"tail"].concat();
    let copies: Vec<&[u8]> = plan
        .iter()
        .map(|copy| match copy {
            Serves::Lie => &lie[..],
            Serves::Short => &blob[..blob.len() - 1],
            Serves::Long => &long[..],
            _ => blob,
        })
        .collect();
    let owners = Owners::serving(&digest, &copies);
    for (fake, copy) in owners.in_order(&digest).iter().zip(plan) {
        match copy {
            Serves::Breaks => fake.breaks.store(true, Ordering::SeqCst),
            Serves::Missing => fake.holds.store(false, Ordering::SeqCst),
            Serves::Down => fake.down.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
    owners
}

/// A directory of its own for one read, so that a file left behind in it shows.
fn directory(name: &str) -> PathBuf {
    let dir = scratch().join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("an old directory");
    }
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

/// The names of the files in `dir`.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A new file at `path` that holds [`HEAD`], its offset at the head's end.
async fn with_head(path: &Path) -> tokio::fs::File {
    let mut file = tokio::fs::File::create(path).await.expect("a file");
    file.write_all(HEAD).await.expect("head");
    file
}

/// `read_to` of `digest` into a new file `dir/blob` that holds [`HEAD`] first: what it
/// answered and what the file holds after the head, which must still be there. The
/// file's offset is at its end: after the blob, or after the head when the read failed.
async fn read_to(owners: &Owners, digest: &Digest, dir: &Path) -> (Result<(), CasError>, Vec<u8>) {
    let path = dir.join("blob");
    let mut file = with_head(&path).await;
    let answer = owners.cas.read_to(digest, &mut file).await;
    let offset = file.stream_position().await.expect("the offset");
    drop(file);
    let held = std::fs::read(&path).expect("the file");
    assert_eq!(&held[..HEAD.len()], HEAD, "the bytes before the offset");
    assert_eq!(offset, held.len() as u64, "the offset, {answer:?}");
    (answer, held[HEAD.len()..].to_vec())
}

/// Whether `path` comes to hold exactly `want` within ten seconds.
async fn until_holds(path: &Path, want: &[u8]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if std::fs::read(path).is_ok_and(|held| held == want) {
            return true;
        }
    }
    false
}

/// Catches a same-length lie written as the blob (the read checks nothing, or only the
/// length), a read that stops at the first owner, a bad copy left in the file under the
/// good one (no truncation, or truncation without going back to the offset) and a bad
/// copy the hook is not told about: the first owner lies, the second has the blob. The
/// file is the head and the blob, each owner was read once, and the hook was told once,
/// naming the first owner as the one without a good copy.
#[tokio::test]
async fn a_lying_first_owner_is_passed_over() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Good]);
    let order = owners.order(&digest);

    let (answer, written) = read_to(&owners, &digest, &directory("failover-lie")).await;
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
    assert_eq!(owners.reads(&digest), [1, 1]);
    let told: Vec<Told> = vec![(parsed(&digest), order[1], vec![order[0]])];
    assert_eq!(owners.hook.told(), told);
}

/// Catches a lie answered, or left in the file, when no owner has the blob, and a temp
/// file left beside it: every one of three owners lies, the read is `Corrupt` naming the
/// first lie, each owner was read once, the file holds only the head, nothing else is in
/// its directory, and the hook is told nothing, as `get` tells it nothing: no owner had a
/// copy to repair from.
#[tokio::test]
async fn every_owner_lying_is_corrupt_and_leaves_the_file_as_it_was() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Lie, Serves::Lie]);
    let dir = directory("failover-every-lie");

    let (answer, written) = read_to(&owners, &digest, &dir).await;
    assert_eq!(
        answer,
        Err(CasError::Corrupt(
            label(&digest),
            label(&digest_of(&lie(&blob)))
        ))
    );
    assert!(written.is_empty(), "{} bytes left", written.len());
    assert_eq!(listing(&dir), ["blob"]);
    assert_eq!(owners.reads(&digest), [1, 1, 1]);
    assert!(owners.hook.told().is_empty());
}

/// Catches a read that breaks part way taken as the end of the read, or its first chunk
/// left in the file: the first owner sends a chunk and fails, the second has the blob,
/// and the file is the blob. The hook is told nothing: an owner that broke is down, not
/// missing a copy.
#[tokio::test]
async fn an_owner_that_breaks_part_way_is_passed_over() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Breaks, Serves::Good]);

    let (answer, written) = read_to(&owners, &digest, &directory("failover-breaks")).await;
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
    assert_eq!(owners.reads(&digest), [1, 1]);
    assert!(owners.hook.told().is_empty());
}

/// Catches a read into a file that gathers the blob in memory before it writes it, on the
/// way through a failover: the first owner lies, the second sends half the blob's 1024
/// chunks and waits. While it waits the file already holds the head and exactly that
/// half: the lie is gone and the good bytes are on disk before the blob is complete.
#[tokio::test]
async fn a_large_blob_streams_through_a_failover() {
    let blob = blob(4096);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Good]);
    let dir = directory("failover-streams");
    let path = dir.join("blob");
    let half = owners.in_order(&digest)[1].pause_after(512);

    let want: Vec<u8> = [HEAD, &blob[..2048]].concat();
    let watch = async {
        let seen = until_holds(&path, &want).await;
        half.add_permits(1);
        seen
    };
    let ((answer, written), seen) = tokio::join!(read_to(&owners, &digest, &dir), watch);
    assert!(
        seen,
        "the first half was not in the file while the rest was held back"
    );
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
}

/// Catches `read_to` and `get` disagreeing on the same owners: for each set of owners,
/// two farms set up alike, one read with `get` and one with `read_to`, give the same
/// answer (the bytes, or the same error), read each owner as often, and tell the hook the
/// same things. A failed `read_to` leaves the file as it was.
#[tokio::test]
async fn read_to_and_get_agree_on_every_set_of_owners() {
    use Serves::{Breaks, Down, Good, Lie, Long, Missing, Short};
    let blob = blob(40);
    let digest = digest_of(&blob);
    let plans: [&[Serves]; 10] = [
        &[Good, Good, Good],
        &[Lie, Good, Good],
        &[Lie, Lie, Lie],
        &[Short, Good, Good],
        &[Long, Lie, Good],
        &[Breaks, Good, Good],
        &[Missing, Good, Good],
        &[Down, Missing, Good],
        &[Missing, Missing, Missing],
        &[Down, Breaks, Short],
    ];
    for plan in plans {
        let by_get = farm(&blob, plan);
        let got = by_get.cas.get(&digest).await;
        let by_read_to = farm(&blob, plan);
        let dir = directory("failover-parity");
        let (answer, written) = read_to(&by_read_to, &digest, &dir).await;
        let read = answer.map(|()| written.clone());
        assert_eq!(read, got, "{plan:?}");
        if read.is_err() {
            assert!(written.is_empty(), "{plan:?}: {} bytes left", written.len());
        }
        assert_eq!(listing(&dir), ["blob"], "{plan:?}");
        assert_eq!(by_read_to.reads(&digest), by_get.reads(&digest), "{plan:?}");
        assert_eq!(by_read_to.hook.told(), by_get.hook.told(), "{plan:?}");
    }
}

/// Catches a read dropped part way that leaves the copy it was writing in the file, or
/// the file's offset past what it holds: the only owner sends half the blob and waits;
/// once that half is in the file the read is dropped, and the file at once holds the head
/// alone, its offset at the head's end, and nothing else is in its directory.
#[tokio::test]
async fn a_read_dropped_part_way_leaves_the_file_as_it_was() {
    let blob = blob(4096);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Good]);
    let _rest = owners.in_order(&digest)[0].pause_after(512);
    let dir = directory("failover-dropped");
    let path = dir.join("blob");
    let mut file = with_head(&path).await;

    let want: Vec<u8> = [HEAD, &blob[..2048]].concat();
    let seen = tokio::select! {
        answer = owners.cas.read_to(&digest, &mut file) => panic!("the read ended: {answer:?}"),
        seen = until_holds(&path, &want) => seen,
    };
    assert!(seen, "half the blob was never in the file");
    assert_eq!(std::fs::read(&path).expect("the file"), HEAD);
    let offset = file.stream_position().await.expect("the offset");
    assert_eq!(offset, HEAD.len() as u64);
    assert_eq!(listing(&dir), ["blob"]);
}

/// Catches a write that had not begun when the read was dropped landing after the file
/// was cut back: with the runtime's one blocking thread kept busy, the only owner's first
/// chunk is queued to be written and the read is dropped. When the thread is free the
/// queued write runs (a task queued after it has run), and the file holds the head alone.
#[test]
fn a_write_queued_when_the_read_is_dropped_writes_nothing() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let blob = blob(40);
        let digest = digest_of(&blob);
        let owners = farm(&blob, &[Serves::Good]);
        let owner = &owners.in_order(&digest)[0];
        let first = owner.pause_after(0);
        let path = directory("failover-queued").join("blob");
        let mut file = with_head(&path).await;
        let (free, freed) = std::sync::mpsc::channel::<()>();
        let mut blocker = None;

        let queue = async {
            while owner.reads.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let (busy, busied) = tokio::sync::oneshot::channel();
            let busy_thread = tokio::task::spawn_blocking(move || {
                let _ = busy.send(());
                let _ = freed.recv();
            });
            busied.await.expect("the blocking thread is busy");
            first.add_permits(1);
            blocker = Some(busy_thread);
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        tokio::select! {
            answer = owners.cas.read_to(&digest, &mut file) => panic!("the read ended: {answer:?}"),
            () = queue => {}
        }
        free.send(()).expect("the blocker waits");
        let blocker = blocker.expect("the blocking thread was made busy");
        blocker.await.expect("the blocker");
        tokio::task::spawn_blocking(|| ())
            .await
            .expect("after the queued write");
        assert_eq!(std::fs::read(&path).expect("the file"), HEAD);
    });
}

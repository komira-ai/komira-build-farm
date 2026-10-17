//! The inbox: where the client leaves what it fetched and what it was ordered, for the
//! root helper to read (the helper is wave 2; this layout is its contract).
//!
//! ```text
//! DIR/inbox/<generation:020>-<commit>/   one directory per offer, mode 0750
//!     offer.json                         what the offer names (the installer's schema 1)
//!     <name>-<commit>-<platform>.tar.gz  each component's tarball
//!     provenance.jsonl                   every component's attestation, one per line
//! DIR/inbox/orders/<order_id:020>.json   one file per accepted order
//! ```
//!
//! A directory becomes visible by one rename out of `.partial-<random>`, after every file
//! in it is written and synced, so a reader never sees a half directory; a crash leaves
//! only a `.partial-*` directory, which the next start removes. The client keeps the two
//! newest offer directories.

use std::collections::hash_map::RandomState;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::types::{Component, Offer, Order, is_hex};

/// The prefix of a directory or file that is not complete yet.
pub const PARTIAL: &str = ".partial-";

/// How many offer directories stay.
pub const KEEP: usize = 2;

/// The inbox under an update directory.
#[derive(Clone, Debug)]
pub struct Inbox {
    root: PathBuf,
}

/// The name of an offer's directory: sorts by generation.
#[must_use]
pub fn dir_name(generation: u64, commit: &str) -> String {
    format!("{generation:020}-{commit}")
}

/// The generation and commit a directory name stands for.
fn parse_dir_name(name: &str) -> Option<(u64, String)> {
    let (generation, commit) = name.split_once('-')?;
    if generation.len() != 20 || !is_hex(commit, 40) {
        return None;
    }
    Some((generation.parse().ok()?, commit.to_owned()))
}

/// The file a component's tarball is written to.
#[must_use]
pub fn tarball_name(offer: &Offer, component: &Component) -> String {
    format!(
        "{}-{}-{}.tar.gz",
        component.name, offer.commit, offer.platform
    )
}

/// The file the attestations of an offer's components are written to, one per line: the
/// bundle the installer's `gh attestation verify --bundle` reads.
pub const BUNDLE: &str = "provenance.jsonl";

/// A name no other call (or process) draws: 16 hex digits.
fn unique() -> String {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}",
        RandomState::new().hash_one((std::process::id(), call))
    )
}

/// Creates `path`, which must not exist, with mode 0640, writes `bytes` and syncs.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o640)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Syncs the directory `path`, so a rename into it survives a power cut.
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// The schema of `offer.json`, which the installer (`kbf-installer`'s `offer`) reads.
const OFFER_SCHEMA: u64 = 1;

#[derive(Serialize)]
struct FileRef<'a> {
    file: &'a str,
    sha256: &'a str,
}

#[derive(Serialize)]
struct ComponentFile<'a> {
    name: &'a str,
    file: String,
    sha256: &'a str,
}

#[derive(Serialize)]
struct OfferFile<'a> {
    schema: u64,
    commit: &'a str,
    bundle: FileRef<'a>,
    components: Vec<ComponentFile<'a>>,
}

#[derive(Serialize)]
struct OrderFile<'a> {
    order_id: u64,
    epoch: u64,
    verb: &'a str,
    commit: &'a str,
    window_s: u32,
}

impl Inbox {
    /// The inbox under `dir`, made (mode 0750) if it is not there, with every
    /// `.partial-*` entry a crash left removed.
    ///
    /// # Errors
    /// The directories cannot be made or listed.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let root = dir.join("inbox");
        DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(root.join("orders"))?;
        let removed = sweep(&root, remove_dir)? + sweep(&root.join("orders"), remove_file)?;
        if removed > 0 {
            tracing::info!(removed, "partial inbox entries of an earlier run removed");
        }
        Ok(Self { root })
    }

    /// The offer directories that are visible, oldest first.
    ///
    /// # Errors
    /// The inbox cannot be listed.
    pub fn offers(&self) -> io::Result<Vec<(u64, String)>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            found.extend(parse_dir_name(&name.to_string_lossy()));
        }
        found.sort();
        Ok(found)
    }

    /// Whether the offer's directory is visible already.
    #[must_use]
    pub fn has(&self, offer: &Offer) -> bool {
        self.root
            .join(dir_name(offer.generation, &offer.commit))
            .is_dir()
    }

    /// Starts the directory of `offer`: an empty `.partial-*` one, invisible to readers.
    ///
    /// # Errors
    /// The directory cannot be made.
    pub fn begin(&self, offer: &Offer) -> io::Result<Staging> {
        let partial = self.root.join(format!("{PARTIAL}{}", unique()));
        DirBuilder::new().mode(0o750).create(&partial)?;
        Ok(Staging {
            inbox: self.clone(),
            partial,
            finished: self.root.join(dir_name(offer.generation, &offer.commit)),
        })
    }

    /// Whether the file of order `order_id` is there.
    #[must_use]
    pub fn has_order(&self, order_id: u64) -> bool {
        self.order_path(order_id).is_file()
    }

    fn order_path(&self, order_id: u64) -> PathBuf {
        self.root
            .join("orders")
            .join(format!("{order_id:020}.json"))
    }

    /// Writes the file of `order`, visible by one rename.
    ///
    /// # Errors
    /// The file cannot be written, synced or renamed.
    pub fn write_order(&self, order: &Order, verb: &str) -> io::Result<()> {
        let body = OrderFile {
            order_id: order.order_id,
            epoch: order.epoch,
            verb,
            commit: &order.commit,
            window_s: order.window_s,
        };
        let orders = self.root.join("orders");
        let tmp = orders.join(format!("{PARTIAL}{}.json", unique()));
        write_new(&tmp, &json(&body)?)?;
        fs::rename(&tmp, self.order_path(order.order_id))?;
        sync_dir(&orders)
    }

    /// Removes the oldest offer directories until [`KEEP`] are left.
    fn prune(&self) -> io::Result<()> {
        let offers = self.offers()?;
        for (generation, commit) in &offers[..offers.len().saturating_sub(KEEP)] {
            fs::remove_dir_all(self.root.join(dir_name(*generation, commit)))?;
        }
        Ok(())
    }
}

fn remove_dir(path: &Path) -> io::Result<()> {
    fs::remove_dir_all(path)
}

fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Removes every `.partial-*` entry of `dir` with `remove`; how many there were.
fn sweep(dir: &Path, remove: fn(&Path) -> io::Result<()>) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(PARTIAL) {
            remove(&entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(io::Error::other)
}

/// An offer's directory while it is being written.
#[derive(Debug)]
pub struct Staging {
    inbox: Inbox,
    partial: PathBuf,
    finished: PathBuf,
}

impl Staging {
    /// Opens `name` in the directory for writing, emptied if it is there (a fetch tried
    /// again starts over).
    ///
    /// # Errors
    /// The file cannot be created.
    pub async fn create(&self, name: &str) -> io::Result<tokio::fs::File> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o640);
        tokio::fs::OpenOptions::from(options)
            .open(self.partial.join(name))
            .await
    }

    /// Writes the attestations and `offer.json`, then makes the directory visible, and
    /// prunes the inbox to the newest [`KEEP`] offers. The tarballs are in the directory
    /// already.
    ///
    /// # Errors
    /// A file cannot be written, the rename fails, or the inbox cannot be pruned.
    pub fn finish(&self, offer: &Offer) -> io::Result<()> {
        let mut bundle = Vec::new();
        for c in &offer.components {
            bundle.extend_from_slice(&c.attestation);
            if bundle.last() != Some(&b'\n') {
                bundle.push(b'\n');
            }
        }
        write_new(&self.partial.join(BUNDLE), &bundle)?;
        let bundle_sha = hex::encode(Sha256::digest(&bundle));
        let components = offer
            .components
            .iter()
            .map(|c| ComponentFile {
                name: &c.name,
                file: tarball_name(offer, c),
                sha256: &c.sha256,
            })
            .collect();
        let body = OfferFile {
            schema: OFFER_SCHEMA,
            commit: &offer.commit,
            bundle: FileRef {
                file: BUNDLE,
                sha256: &bundle_sha,
            },
            components,
        };
        write_new(&self.partial.join("offer.json"), &json(&body)?)?;
        sync_dir(&self.partial)?;
        fs::rename(&self.partial, &self.finished)?;
        sync_dir(&self.inbox.root)?;
        self.inbox.prune()
    }

    /// Removes the unfinished directory. Best effort: the next start sweeps what is left.
    pub fn discard(self) {
        let _ = fs::remove_dir_all(&self.partial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{COMMIT, offer, scratch};

    /// Catches an inbox that lists what is not an offer directory (the orders directory, a
    /// partial entry, a stray name, a name whose generation is not a number or whose
    /// commit is not 40 hex digits), or lists offers in any order but the oldest first.
    #[test]
    fn only_offer_directories_are_listed_oldest_first() {
        let dir = scratch("inbox-list");
        let inbox = Inbox::open(&dir).expect("open");
        for name in [
            dir_name(12, COMMIT),
            dir_name(3, COMMIT),
            "garbage".to_owned(),
            format!("{PARTIAL}abc"),
            format!("{}x-{COMMIT}", "0".repeat(19)),
            format!("{:020}-abc", 4),
            format!("{:03}-{COMMIT}", 4),
        ] {
            fs::create_dir(dir.join("inbox").join(name)).expect("make");
        }
        // The partial entry is swept at the next open, not listed in between.
        assert_eq!(
            inbox.offers().expect("list"),
            vec![(3, COMMIT.to_owned()), (12, COMMIT.to_owned())]
        );
    }

    /// Catches a start that leaves what a crash left: a partial offer directory, and a
    /// partial order file; and one that removes anything else.
    #[test]
    fn opening_sweeps_partial_entries_and_nothing_else() {
        let dir = scratch("inbox-sweep");
        let inbox = Inbox::open(&dir).expect("open");
        let keep = dir.join("inbox").join(dir_name(1, COMMIT));
        fs::create_dir(&keep).expect("keep");
        let partial = dir.join("inbox").join(format!("{PARTIAL}one"));
        fs::create_dir(&partial).expect("partial");
        fs::write(partial.join("file"), "x").expect("file");
        let order_tmp = dir.join("inbox/orders").join(format!("{PARTIAL}two.json"));
        fs::write(&order_tmp, "{").expect("tmp order");
        let order = dir.join("inbox/orders/00000000000000000001.json");
        fs::write(&order, "{}").expect("order");
        drop(inbox);
        Inbox::open(&dir).expect("reopen");
        assert!(keep.is_dir());
        assert!(order.is_file());
        assert!(!partial.exists());
        assert!(!order_tmp.exists());
    }

    /// Catches a discard that leaves the directory, and a staging whose files are
    /// visible before the rename.
    #[tokio::test]
    async fn a_discarded_directory_is_gone_and_an_unfinished_one_is_invisible() {
        let dir = scratch("inbox-discard");
        let inbox = Inbox::open(&dir).expect("open");
        let offer = offer();
        let staging = inbox.begin(&offer).expect("begin");
        staging.create("file").await.expect("create");
        assert!(!inbox.has(&offer));
        assert_eq!(inbox.offers().expect("list"), vec![]);
        staging.discard();
        let left: Vec<_> = fs::read_dir(dir.join("inbox"))
            .expect("dir")
            .flatten()
            .collect();
        assert_eq!(left.len(), 1, "only the orders directory: {left:?}");
    }
}

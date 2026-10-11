//! A fault-injecting, in-memory [`Fs`], for tests of every crash point.
//!
//! [`FaultFs`] keeps two views of the directory: what a running process sees, and what
//! is durable (what the last [`FsFile::sync`] of each file and the last
//! [`Fs::sync_dir`] made so). Every call to the filesystem or a file is one numbered
//! operation; [`FaultFs::fail_at`] makes one of them return an error, or crash the
//! process there, after which every operation fails. [`FaultFs::crash`] then gives the
//! directory a power cut would leave: the durable names, each file's durable bytes,
//! and up to a chosen number of the bytes appended after them, which is a torn write
//! cut at that offset. [`FaultFs::crash_reordered`] also lets any chosen subset of
//! the creates, renames and removals since the last directory sync reach the disk, in
//! any combination: POSIX orders none of them before that sync returns.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::fs::{Fs, FsFile};

/// What a planned fault does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The operation fails with an I/O error and changes nothing; later ones run.
    Error,
    /// The process dies at the operation: it and every later one fail and change
    /// nothing.
    Crash,
}

#[derive(Clone, Debug, Default)]
struct Inode {
    data: Vec<u8>,
    durable: Vec<u8>,
}

impl Inode {
    /// How many bytes were appended after the durable ones, if the file only grew.
    fn unsynced(&self) -> usize {
        if self.data.starts_with(&self.durable) {
            self.data.len() - self.durable.len()
        } else {
            0
        }
    }
}

/// One change to the directory's names since its last sync.
#[derive(Clone, Debug)]
enum NameChange {
    Link(String, u64),
    Rename {
        from: String,
        to: String,
        inode: u64,
    },
    Unlink(String),
}

impl NameChange {
    fn apply(&self, names: &mut BTreeMap<String, u64>) {
        match self {
            Self::Link(name, inode) => {
                names.insert(name.clone(), *inode);
            }
            Self::Rename { from, to, inode } => {
                names.remove(from);
                names.insert(to.clone(), *inode);
            }
            Self::Unlink(name) => {
                names.remove(name);
            }
        }
    }

    /// The file the change names, if it adds a name.
    fn inode(&self) -> Option<u64> {
        match self {
            Self::Link(_, inode) | Self::Rename { inode, .. } => Some(*inode),
            Self::Unlink(_) => None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    inodes: BTreeMap<u64, Inode>,
    next_inode: u64,
    names: BTreeMap<String, u64>,
    durable_names: BTreeMap<String, u64>,
    /// The name changes since the last directory sync, in order.
    pending: Vec<NameChange>,
    ops: u64,
    faults: BTreeMap<u64, Fault>,
    crashed: bool,
}

impl State {
    /// Counts one operation and fails it if a fault is planned there.
    fn begin(&mut self) -> io::Result<()> {
        let op = self.ops;
        self.ops += 1;
        if self.crashed {
            return Err(io::Error::other("crashed"));
        }
        match self.faults.get(&op) {
            None => Ok(()),
            Some(Fault::Error) => Err(io::Error::other(format!("injected error at op {op}"))),
            Some(Fault::Crash) => {
                self.crashed = true;
                Err(io::Error::other(format!("crashed at op {op}")))
            }
        }
    }

    fn inode_of(&self, name: &str) -> io::Result<u64> {
        self.names
            .get(name)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))
    }
}

/// An in-memory directory that fails or crashes where a test says. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct FaultFs {
    state: Arc<Mutex<State>>,
}

impl FaultFs {
    /// An empty directory with no faults planned.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Plans `fault` at operation `op` (numbered from zero, counted across the
    /// directory and all its files).
    pub fn fail_at(&self, op: u64, fault: Fault) {
        self.lock().faults.insert(op, fault);
    }

    /// How many operations have run (or been refused) so far.
    #[must_use]
    pub fn ops(&self) -> u64 {
        self.lock().ops
    }

    /// The most bytes any file that can survive a crash holds past its durable ones:
    /// the largest `keep` for which [`FaultFs::crash_reordered`] can differ from
    /// `keep - 1`.
    #[must_use]
    pub fn unsynced(&self) -> usize {
        let s = self.lock();
        s.durable_names
            .values()
            .copied()
            .chain(s.pending.iter().filter_map(NameChange::inode))
            .map(|i| s.inodes[&i].unsynced())
            .max()
            .unwrap_or(0)
    }

    /// How many creates, renames and removals ran since the last directory sync: the
    /// bits of [`FaultFs::crash_reordered`]'s `names` that matter.
    #[must_use]
    pub fn pending_names(&self) -> usize {
        self.lock().pending.len()
    }

    /// The directory after a power cut, as a new [`FaultFs`] with no faults: the
    /// durable names, and in each file its durable bytes followed by at most `keep` of
    /// the bytes appended after them. A file whose unsynced change was not an append
    /// (a truncation) keeps its durable bytes. No name change since the last directory
    /// sync survives: `crash_reordered(keep, 0)`.
    #[must_use]
    pub fn crash(&self, keep: usize) -> Self {
        self.crash_reordered(keep, 0)
    }

    /// [`FaultFs::crash`], where the name changes since the last directory sync whose
    /// bit is set in `names` (bit 0 is the first change) reached the disk too, applied
    /// in the order they ran; the others are lost. Changes past the 64th are lost.
    #[must_use]
    pub fn crash_reordered(&self, keep: usize, names: u64) -> Self {
        let s = self.lock();
        let mut durable = s.durable_names.clone();
        for (bit, change) in s.pending.iter().take(64).enumerate() {
            if (names >> bit) & 1 == 1 {
                change.apply(&mut durable);
            }
        }
        let mut next = State {
            names: durable.clone(),
            durable_names: durable,
            next_inode: s.next_inode,
            ..State::default()
        };
        for &i in next.durable_names.values() {
            let inode = &s.inodes[&i];
            let kept = inode.durable.len() + inode.unsynced().min(keep);
            let data = if inode.unsynced() > 0 {
                inode.data[..kept].to_vec()
            } else {
                inode.durable.clone()
            };
            next.inodes.insert(
                i,
                Inode {
                    durable: data.clone(),
                    data,
                },
            );
        }
        Self {
            state: Arc::new(Mutex::new(next)),
        }
    }

    /// The directory after the process died but the machine stayed up, as a new
    /// [`FaultFs`] with no faults: everything the process wrote is still visible, and
    /// only what it synced is durable, so a later [`FaultFs::crash`] can still lose the
    /// rest.
    #[must_use]
    pub fn restart(&self) -> Self {
        let s = self.lock();
        let next = State {
            inodes: s.inodes.clone(),
            next_inode: s.next_inode,
            names: s.names.clone(),
            durable_names: s.durable_names.clone(),
            pending: s.pending.clone(),
            ..State::default()
        };
        Self {
            state: Arc::new(Mutex::new(next)),
        }
    }

    /// The bytes `name` holds now, or `None` if it does not exist.
    #[must_use]
    pub fn contents(&self, name: &str) -> Option<Vec<u8>> {
        let s = self.lock();
        s.names.get(name).map(|i| s.inodes[i].data.clone())
    }

    /// Flips the bits of byte `offset` of `name`, in what the process sees and in
    /// what is durable: a corruption on the disk.
    ///
    /// # Panics
    ///
    /// If `name` does not exist or is shorter than `offset + 1` bytes.
    pub fn flip(&self, name: &str, offset: usize) {
        let mut s = self.lock();
        let i = s.names[name];
        let inode = s.inodes.get_mut(&i).expect("named inode");
        inode.data[offset] ^= 0xFF;
        if offset < inode.durable.len() {
            inode.durable[offset] ^= 0xFF;
        }
    }

    /// Writes `bytes` as the durable content of a durable file `name`, replacing any:
    /// a file left on the disk by someone else.
    pub fn put(&self, name: &str, bytes: &[u8]) {
        let mut s = self.lock();
        let i = s.next_inode;
        s.next_inode += 1;
        s.inodes.insert(
            i,
            Inode {
                data: bytes.to_vec(),
                durable: bytes.to_vec(),
            },
        );
        s.names.insert(name.to_owned(), i);
        s.durable_names.insert(name.to_owned(), i);
    }
}

impl Fs for FaultFs {
    type File = FaultFile;

    fn list(&self) -> io::Result<Vec<String>> {
        let mut s = self.lock();
        s.begin()?;
        Ok(s.names.keys().cloned().collect())
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        let mut s = self.lock();
        s.begin()?;
        let i = s.inode_of(name)?;
        Ok(s.inodes[&i].data.clone())
    }

    fn create(&self, name: &str) -> io::Result<FaultFile> {
        let mut s = self.lock();
        s.begin()?;
        if s.names.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                name.to_owned(),
            ));
        }
        let inode = s.next_inode;
        s.next_inode += 1;
        s.inodes.insert(inode, Inode::default());
        s.names.insert(name.to_owned(), inode);
        s.pending.push(NameChange::Link(name.to_owned(), inode));
        Ok(FaultFile {
            state: Arc::clone(&self.state),
            inode,
        })
    }

    fn open_append(&self, name: &str) -> io::Result<FaultFile> {
        let mut s = self.lock();
        s.begin()?;
        let inode = s.inode_of(name)?;
        Ok(FaultFile {
            state: Arc::clone(&self.state),
            inode,
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut s = self.lock();
        s.begin()?;
        let inode = s.inode_of(from)?;
        let change = NameChange::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
            inode,
        };
        change.apply(&mut s.names);
        s.pending.push(change);
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut s = self.lock();
        s.begin()?;
        s.inode_of(name)?;
        let change = NameChange::Unlink(name.to_owned());
        change.apply(&mut s.names);
        s.pending.push(change);
        Ok(())
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut s = self.lock();
        s.begin()?;
        s.durable_names = s.names.clone();
        s.pending.clear();
        Ok(())
    }
}

/// A file of a [`FaultFs`].
#[derive(Debug)]
pub struct FaultFile {
    state: Arc<Mutex<State>>,
    inode: u64,
}

impl FaultFile {
    fn with<T>(&mut self, f: impl FnOnce(&mut Inode) -> T) -> io::Result<T> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.begin()?;
        let inode = s.inodes.get_mut(&self.inode).expect("open inode");
        Ok(f(inode))
    }
}

impl FsFile for FaultFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.with(|i| i.data.extend_from_slice(bytes))
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let len = usize::try_from(len).map_err(io::Error::other)?;
        self.with(|i| i.data.resize(len, 0))
    }

    fn sync(&mut self) -> io::Result<()> {
        self.with(|i| i.durable.clone_from(&i.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a fake that makes unsynced appends or names durable (every crash test
    /// over it would pass vacuously), a crash that keeps more than `keep` torn bytes,
    /// a restart that makes what the process wrote durable or hides it, and a fault
    /// that does not stop later operations.
    #[test]
    fn crash_keeps_only_what_was_synced() {
        let fs = FaultFs::new();
        let mut a = fs.create("a").unwrap();
        a.append(b"one").unwrap();
        a.sync().unwrap();
        fs.sync_dir().unwrap();
        a.append(b"two").unwrap();
        let mut b = fs.create("b").unwrap();
        b.append(b"x").unwrap();
        b.sync().unwrap();
        fs.rename("a", "c").unwrap();
        assert_eq!(fs.unsynced(), 3);
        assert_eq!(fs.crash(0).contents("a").unwrap(), b"one");
        assert_eq!(fs.crash(2).contents("a").unwrap(), b"onetw");
        assert_eq!(fs.crash(9).contents("a").unwrap(), b"onetwo");
        assert_eq!(fs.crash(9).contents("b"), None);
        assert_eq!(fs.crash(9).contents("c"), None);
        assert_eq!(fs.contents("c").unwrap(), b"onetwo");
        let restarted = fs.restart();
        assert_eq!(restarted.contents("c").unwrap(), b"onetwo");
        assert_eq!(restarted.crash(0).contents("a").unwrap(), b"one");
        assert_eq!(restarted.crash(0).contents("c"), None);

        fs.sync_dir().unwrap();
        fs.remove("b").unwrap();
        assert_eq!(fs.crash(0).contents("b").unwrap(), b"x");
        let mut c = fs.open_append("c").unwrap();
        c.truncate(2).unwrap();
        assert_eq!(
            fs.crash(9).contents("c").unwrap(),
            b"one",
            "a truncation is not an append"
        );
        c.sync().unwrap();
        assert_eq!(fs.crash(9).contents("c").unwrap(), b"on");

        let op = fs.ops();
        fs.fail_at(op, Fault::Error);
        fs.fail_at(op + 2, Fault::Crash);
        assert!(fs.list().is_err());
        assert_eq!(fs.read("c").unwrap(), b"on");
        assert!(c.append(b"z").is_err());
        assert!(fs.read("c").is_err(), "nothing runs after a crash");
        assert!(fs.create("d").is_err());
        assert_eq!(fs.contents("c").unwrap(), b"on");
    }

    /// Catches: a fake whose power cut applies the name changes since the last
    /// directory sync all or nothing (a store that removes files before the rename
    /// that replaces them is durable would pass every crash test), keeps a change its
    /// mask drops, loses the file a kept rename or create names, or forgets the
    /// pending changes across a restart.
    #[test]
    fn a_power_cut_keeps_any_subset_of_the_name_changes() {
        let fs = FaultFs::new();
        for (name, bytes) in [("a", b"old"), ("b", b"bbb")] {
            let mut f = fs.create(name).unwrap();
            f.append(bytes).unwrap();
            f.sync().unwrap();
        }
        fs.sync_dir().unwrap();
        assert_eq!(fs.pending_names(), 0);
        let mut t = fs.create("t").unwrap();
        t.append(b"new").unwrap();
        t.sync().unwrap();
        fs.rename("t", "a").unwrap();
        fs.remove("b").unwrap();
        let mut u = fs.create("u").unwrap();
        u.append(b"xyz").unwrap();
        assert_eq!(fs.pending_names(), 4);
        assert_eq!(fs.unsynced(), 3, "a pending create's unsynced bytes count");
        let names = |mask: u64| {
            let mut v: Vec<(String, Vec<u8>)> = Vec::new();
            let cut = fs.crash_reordered(2, mask);
            for n in cut.list().unwrap() {
                let bytes = cut.read(&n).unwrap();
                v.push((n, bytes));
            }
            v
        };
        let own = |pairs: &[(&str, &[u8])]| -> Vec<(String, Vec<u8>)> {
            pairs
                .iter()
                .map(|(n, b)| ((*n).to_owned(), b.to_vec()))
                .collect()
        };
        assert_eq!(names(0), own(&[("a", b"old"), ("b", b"bbb")]));
        assert_eq!(
            names(0b0001),
            own(&[("a", b"old"), ("b", b"bbb"), ("t", b"new")])
        );
        assert_eq!(names(0b0010), own(&[("a", b"new"), ("b", b"bbb")]));
        assert_eq!(names(0b0100), own(&[("a", b"old")]));
        assert_eq!(
            names(0b1000),
            own(&[("a", b"old"), ("b", b"bbb"), ("u", b"xy")])
        );
        assert_eq!(names(0b1111), own(&[("a", b"new"), ("u", b"xy")]));
        assert_eq!(names(0), names(1 << 63), "bits past the changes do nothing");
        let restarted = fs.restart();
        assert_eq!(restarted.pending_names(), 4);
        assert_eq!(restarted.crash_reordered(0, 0b0100).contents("b"), None);
        fs.sync_dir().unwrap();
        assert_eq!(fs.pending_names(), 0);
        assert_eq!(fs.crash(0).contents("a").unwrap(), b"new");
    }

    /// Catches: name errors that a store relies on going unreported (a create over an
    /// existing name, an open, rename or removal of a missing one), and a flip that
    /// does not reach the durable bytes.
    #[test]
    fn name_errors_and_flips() {
        let fs = FaultFs::new();
        fs.put("p", b"abc");
        assert_eq!(
            fs.create("p").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs.open_append("q").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs.read("q").unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(
            fs.rename("q", "r").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs.remove("q").unwrap_err().kind(), io::ErrorKind::NotFound);
        fs.flip("p", 1);
        assert_eq!(fs.crash(0).contents("p").unwrap(), [b'a', !b'b', b'c']);
        fs.open_append("p").unwrap().append(b"d").unwrap();
        fs.flip("p", 3);
        assert_eq!(fs.contents("p").unwrap(), [b'a', !b'b', b'c', !b'd']);
        assert_eq!(
            fs.crash(9).contents("p").unwrap(),
            [b'a', !b'b', b'c', !b'd']
        );
        assert_eq!(fs.crash(0).contents("p").unwrap(), [b'a', !b'b', b'c']);
        assert_eq!(fs.list().unwrap(), ["p"]);
    }
}

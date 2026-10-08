//! The ledger: every lease id the helper ever made a user for, kept across reboots.
//!
//! A lease user's name derives from its lease id, and an id is used once: a second
//! `user-create` for it is refused, so a name is never reused and a gate grant (which
//! names one lease) is single-use. The ledger also holds each lease's uid, so `run`,
//! `kill-uid` and `user-delete` act on the uid the helper handed out rather than on
//! whatever a directory lookup says, and the uid a deleted lease held is free again
//! only once its deletion is recorded.
//!
//! It is an append-only text file, one record per line, each written with a single
//! `write` and then `fsync`ed before the helper acts on it:
//!
//! ```text
//! created <term>.<seq> <uid>
//! deleted <term>.<seq>
//! ```
//!
//! A crash can leave a last line without its newline. That record's action never
//! started (it follows the `fsync`), so on opening the fragment is cut off. Any other
//! line that does not parse stops the helper from starting: the ledger is root's own
//! file, and guessing past corruption could reuse a name or a uid.
//!
//! The file grows by about 30 bytes per lease and is never compacted (a later change
//! can drop records of deleted leases older than any grant once ids are known to grow).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use kbf_types::LeaseId;

use crate::lease::parse_lease;

/// One lease's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub uid: u32,
    pub deleted: bool,
}

/// The ledger file and what it says.
#[derive(Debug)]
pub struct Ledger {
    file: File,
    leases: BTreeMap<LeaseId, Entry>,
    /// The uid of the last `created` record: allocation continues after it.
    last_uid: Option<u32>,
}

impl Ledger {
    /// Opens (creating, mode 0600) and reads the ledger at `path`.
    ///
    /// # Errors
    /// The file cannot be opened, read or trimmed, or a complete line does not parse.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let whole = text.rfind('\n').map_or(0, |i| i + 1);
        if whole < text.len() {
            tracing::warn!(
                fragment = &text[whole..],
                "dropping a ledger record cut short by a crash"
            );
            file.set_len(whole as u64)?;
            file.sync_all()?;
        }
        let mut ledger = Self {
            file,
            leases: BTreeMap::new(),
            last_uid: None,
        };
        for (number, line) in text[..whole].lines().enumerate() {
            let record = Record::parse(line)
                .and_then(|record| ledger.admit(record).map(|()| record))
                .map_err(|why| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: line {}: {why}", path.display(), number + 1),
                    )
                })?;
            ledger.commit(record);
        }
        Ok(ledger)
    }

    /// Whether `record` may follow what the ledger holds.
    fn admit(&self, record: Record) -> Result<(), String> {
        match record {
            Record::Created(lease, _) if self.leases.contains_key(&lease) => {
                Err(format!("lease {lease} created twice"))
            }
            Record::Deleted(lease) if self.get(lease).is_none_or(|entry| entry.deleted) => {
                Err(format!("lease {lease} deleted but not live"))
            }
            _ => Ok(()),
        }
    }

    fn commit(&mut self, record: Record) {
        match record {
            Record::Created(lease, uid) => {
                let entry = Entry {
                    uid,
                    deleted: false,
                };
                self.leases.insert(lease, entry);
                self.last_uid = Some(uid);
            }
            Record::Deleted(lease) => {
                // Admitted, so the lease is live.
                self.leases
                    .entry(lease)
                    .and_modify(|entry| entry.deleted = true);
            }
        }
    }

    /// Writes `record` and its newline in one `write`, `fsync`s, then applies it.
    fn append(&mut self, record: Record) -> io::Result<()> {
        self.admit(record)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        self.file
            .write_all(format!("{record}\n").as_bytes())
            .and_then(|()| self.file.sync_all())?;
        self.commit(record);
        Ok(())
    }

    /// The lease's record, if it was ever created.
    #[must_use]
    pub fn get(&self, lease: LeaseId) -> Option<Entry> {
        self.leases.get(&lease).copied()
    }

    /// The uid handed out last.
    #[must_use]
    pub fn last_uid(&self) -> Option<u32> {
        self.last_uid
    }

    /// The uids of leases created and not yet deleted.
    #[must_use]
    pub fn held_uids(&self) -> BTreeSet<u32> {
        self.leases
            .values()
            .filter(|entry| !entry.deleted)
            .map(|entry| entry.uid)
            .collect()
    }

    /// Makes every later write fail, as a full or failing disk would.
    #[cfg(test)]
    pub(crate) fn fail_writes(&mut self, path: &Path) {
        self.file = File::open(path).unwrap();
    }

    /// Records that `lease` got `uid`, durably.
    ///
    /// # Errors
    /// The lease was created before, or the write or `fsync` failed.
    pub fn record_created(&mut self, lease: LeaseId, uid: u32) -> io::Result<()> {
        self.append(Record::Created(lease, uid))
    }

    /// Records that `lease`'s user is gone, durably.
    ///
    /// # Errors
    /// The lease is not live, or the write or `fsync` failed.
    pub fn record_deleted(&mut self, lease: LeaseId) -> io::Result<()> {
        self.append(Record::Deleted(lease))
    }
}

/// One line of the ledger.
#[derive(Clone, Copy, Debug)]
enum Record {
    Created(LeaseId, u32),
    Deleted(LeaseId),
}

impl Record {
    fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["created", lease, uid] => Ok(Self::Created(
                parse_lease(lease)?,
                uid.parse().map_err(|_| format!("bad uid {uid:?}"))?,
            )),
            ["deleted", lease] => Ok(Self::Deleted(parse_lease(lease)?)),
            _ => Err(format!("unknown record {line:?}")),
        }
    }
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created(lease, uid) => write!(f, "created {lease} {uid}"),
            Self::Deleted(lease) => write!(f, "deleted {lease}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kbf-mac-session-ledger-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Catches: a ledger kept only in memory, under which a restart (or reboot)
    /// forgets used lease ids and a grant or a name can be used again.
    #[test]
    fn records_survive_reopening() {
        let path = dir("reopen").join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(1, 1), 600).unwrap();
        ledger.record_created(LeaseId::new(1, 2), 601).unwrap();
        ledger.record_deleted(LeaseId::new(1, 1)).unwrap();
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(
            ledger.get(LeaseId::new(1, 1)),
            Some(Entry {
                uid: 600,
                deleted: true
            })
        );
        assert_eq!(
            ledger.get(LeaseId::new(1, 2)),
            Some(Entry {
                uid: 601,
                deleted: false
            })
        );
        assert_eq!(ledger.get(LeaseId::new(1, 3)), None);
        assert_eq!(ledger.last_uid(), Some(601));
        assert_eq!(ledger.held_uids(), BTreeSet::from([601]));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 1.1 600\ncreated 1.2 601\ndeleted 1.1\n");
    }

    /// Catches: a second `created` for one lease accepted (a reused name or grant),
    /// or a deletion of a lease that is not live.
    #[test]
    fn a_lease_is_created_once_and_deleted_once() {
        let path = dir("once").join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(2, 1), 600).unwrap();
        let again = ledger.record_created(LeaseId::new(2, 1), 601).unwrap_err();
        assert!(again.to_string().contains("created twice"), "{again}");
        ledger.record_deleted(LeaseId::new(2, 1)).unwrap();
        let twice = ledger.record_deleted(LeaseId::new(2, 1)).unwrap_err();
        assert!(twice.to_string().contains("not live"), "{twice}");
        let never = ledger.record_deleted(LeaseId::new(2, 9)).unwrap_err();
        assert!(never.to_string().contains("not live"), "{never}");
        // The refused records were not written, and nothing changed in memory.
        assert_eq!(ledger.last_uid(), Some(600));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 2.1 600\ndeleted 2.1\n");
    }

    /// Catches: a torn last record kept (it would glue onto the next record and make
    /// the ledger unreadable), or a corrupt complete line skipped.
    #[test]
    fn a_torn_last_line_is_cut_and_a_corrupt_line_refused() {
        let path = dir("torn").join("ledger");
        std::fs::write(&path, "created 3.1 600\ncreated 3.").unwrap();
        let mut ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.get(LeaseId::new(3, 1)).map(|e| e.uid), Some(600));
        ledger.record_created(LeaseId::new(3, 2), 601).unwrap();
        drop(ledger);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "created 3.1 600\ncreated 3.2 601\n");

        for bad in [
            "created 3.1\n",
            "created 3.1 six\n",
            "created 03.1 600\n",
            "removed 3.1\n",
            "deleted 3.1\n",
            "created 3.1 600\ncreated 3.1 601\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            let error = Ledger::open(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bad:?}");
            assert!(error.to_string().contains("line "), "{bad:?}: {error}");
        }
    }

    /// Catches: the ledger opened through a symbolic link planted at its path.
    #[test]
    fn a_symlink_at_the_ledger_path_is_refused() {
        let dir = dir("link");
        std::fs::write(dir.join("elsewhere"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("ledger")).unwrap();
        assert!(Ledger::open(&dir.join("ledger")).is_err());
    }

    /// Catches: memory updated although the record never reached the disk.
    #[test]
    fn a_failed_write_changes_nothing() {
        let path = dir("fail").join("ledger");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.record_created(LeaseId::new(5, 1), 600).unwrap();
        ledger.fail_writes(&path);
        assert!(ledger.record_created(LeaseId::new(5, 2), 601).is_err());
        assert_eq!(ledger.get(LeaseId::new(5, 2)), None);
        assert_eq!(ledger.last_uid(), Some(600));
    }
}

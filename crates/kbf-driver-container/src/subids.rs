//! The node check the container driver needs before its first lease: the daemon's
//! user owns a range of subordinate uids and gids.
//!
//! Every container runs with `--userns=nomap` (see `podman::create_args`): its ids
//! map only to the daemon user's subordinate ids, never to the user itself. A user
//! with no range in `/etc/subuid` or `/etc/subgid` gets no container at all, and one
//! with fewer than [`MIN_SUBORDINATE_IDS`] gets containers in which an image's files
//! owned by a high id (65534, `nobody`, is common) cannot be unpacked. So the daemon
//! refuses to start, naming the file and the fix, rather than fail every lease.
//!
//! An entry is `<user>:<first id>:<count>`, `<user>` being a name or a numeric uid
//! (shadow's `subuid(5)`). The name is read from the passwd file; a user that only a
//! directory service knows is matched by number.

use std::path::{Path, PathBuf};

/// The fewest subordinate ids the daemon's user may have: a full 16-bit range, what
/// `useradd` gives a new user.
pub const MIN_SUBORDINATE_IDS: u64 = 65536;

/// Where the check reads the user database and the subordinate id ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdFiles {
    pub passwd: PathBuf,
    pub subuid: PathBuf,
    pub subgid: PathBuf,
}

impl IdFiles {
    /// `/etc/passwd`, `/etc/subuid` and `/etc/subgid`.
    #[must_use]
    pub fn system() -> Self {
        Self {
            passwd: PathBuf::from("/etc/passwd"),
            subuid: PathBuf::from("/etc/subuid"),
            subgid: PathBuf::from("/etc/subgid"),
        }
    }

    /// `passwd`, `subuid` and `subgid` in `dir`: stand-ins for the system files, for a
    /// test of the daemon's startup (`kbf-daemon --id-files`).
    #[must_use]
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            passwd: dir.join("passwd"),
            subuid: dir.join("subuid"),
            subgid: dir.join("subgid"),
        }
    }
}

/// Why the daemon's user cannot run containers.
#[derive(Debug, thiserror::Error)]
pub enum SubidError {
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{file} has no range for the daemon's user {user}: the container driver runs \
         every container with --userns=nomap, whose ids map only to subordinate ids. \
         Add one (usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \
         <user>), then run `podman system migrate` as that user"
    )]
    Missing { file: PathBuf, user: String },
    #[error(
        "{file} gives the daemon's user {user} {count} subordinate ids; at least \
         {MIN_SUBORDINATE_IDS} are needed so that every id an image uses maps"
    )]
    TooFew {
        file: PathBuf,
        user: String,
        count: u64,
    },
}

/// Checks that the user `uid` has at least [`MIN_SUBORDINATE_IDS`] subordinate uids
/// and gids.
pub fn check_subordinate_ids(files: &IdFiles, uid: u32) -> Result<(), SubidError> {
    let passwd = read(&files.passwd)?;
    let name = user_name(&passwd, uid);
    let user = name.map_or_else(|| format!("uid {uid}"), |n| format!("{n} (uid {uid})"));
    for file in [&files.subuid, &files.subgid] {
        // A missing file is a user with no range.
        let text = match std::fs::read_to_string(file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            other => other.map_err(|source| SubidError::Read {
                path: file.clone(),
                source,
            })?,
        };
        let count = subordinate_count(&text, name, uid);
        if count == 0 {
            return Err(SubidError::Missing {
                file: file.clone(),
                user,
            });
        }
        if count < MIN_SUBORDINATE_IDS {
            return Err(SubidError::TooFew {
                file: file.clone(),
                user,
                count,
            });
        }
    }
    Ok(())
}

/// [`check_subordinate_ids`] for this process's user, on `files` (the daemon's are
/// [`IdFiles::system`] unless a test names others).
pub fn check_daemon_user(files: &IdFiles) -> Result<(), SubidError> {
    check_subordinate_ids(files, rustix::process::getuid().as_raw())
}

fn read(path: &Path) -> Result<String, SubidError> {
    std::fs::read_to_string(path).map_err(|source| SubidError::Read {
        path: path.to_owned(),
        source,
    })
}

/// The name of user `uid` in passwd-format `text`.
fn user_name(text: &str, uid: u32) -> Option<&str> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, id, ..] = fields[..] else {
            return None;
        };
        (id.parse() == Ok(uid)).then_some(name)
    })
}

/// How many distinct ids the entries of subuid-format `text` give the user `name` (if
/// known) or `uid`. A malformed line gives none. Ranges are merged before counting, so
/// one range listed under both the name and the uid, or two that overlap, count once;
/// a range running past `u64::MAX` ends there, and the total saturates.
fn subordinate_count(text: &str, name: Option<&str>, uid: u32) -> u64 {
    let uid = uid.to_string();
    let mut ranges: Vec<(u64, u64)> = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.trim().split(':').collect();
            let [owner, first, count] = fields[..] else {
                return None;
            };
            let ours = owner == uid || Some(owner) == name;
            let (first, count) = (first.parse::<u64>().ok()?, count.parse::<u64>().ok()?);
            ours.then(|| (first, first.saturating_add(count)))
        })
        .collect();
    ranges.sort_unstable();
    let (mut total, mut counted_to) = (0u64, 0u64);
    for (start, end) in ranges {
        let start = start.max(counted_to);
        if end > start {
            total = total.saturating_add(end - start);
            counted_to = end;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With a line too short to name a uid, which no lookup may stop at.
    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n+short:x\nkbf:x:990:990::/var/lib/kbf:/usr/sbin/nologin\n";

    /// One test's files, beside the test binary.
    fn files(name: &str, subuid: Option<&str>, subgid: Option<&str>) -> IdFiles {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let files = IdFiles {
            passwd: dir.join("passwd"),
            subuid: dir.join("subuid"),
            subgid: dir.join("subgid"),
        };
        std::fs::write(&files.passwd, PASSWD).expect("passwd");
        for (path, text) in [(&files.subuid, subuid), (&files.subgid, subgid)] {
            if let Some(text) = text {
                std::fs::write(path, text).expect("write");
            }
        }
        files
    }

    /// Catches a user with a full range, by name or by number, or in two halves,
    /// being refused (the daemon would never start on a correctly set up node).
    #[test]
    fn a_full_range_by_name_or_number_passes() {
        for (name, text) in [
            ("by-name", "kbf:100000:65536\n"),
            ("by-number", "990:100000:65536\n"),
            (
                "in-halves",
                "other:1:5\nkbf:100000:32768\n990:200000:32768\n",
            ),
        ] {
            let files = files(name, Some(text), Some(text));
            check_subordinate_ids(&files, 990).expect(name);
        }
    }

    /// Catches the "skip the check" mutant: a user with no range, in either file (or
    /// with no file at all), starts a daemon whose every container fails. The message
    /// names the file, the user and the fix.
    #[test]
    fn a_user_with_no_range_is_refused_by_file_and_name() {
        let other = "other:100000:65536\n";
        let full = "kbf:100000:65536\n";
        for (name, subuid, subgid, missing) in [
            ("no-subuid", Some(other), Some(full), "subuid"),
            ("no-subgid", Some(full), None, "subgid"),
        ] {
            let files = files(name, subuid, subgid);
            let error = check_subordinate_ids(&files, 990).expect_err(name);
            let text = error.to_string();
            assert!(
                matches!(error, SubidError::Missing { .. }),
                "{name}: {text}"
            );
            let file = files.subuid.with_file_name(missing);
            assert!(
                text.starts_with(&format!("{} has no range", file.display())),
                "{text}"
            );
            assert!(text.contains("kbf (uid 990)"), "{text}");
            assert!(text.contains("--userns=nomap"), "{text}");
            assert!(text.contains("usermod --add-subuids"), "{text}");
        }
        let files = files("unknown-user", Some(full), Some(full));
        let error = check_subordinate_ids(&files, 4242).expect_err("not kbf");
        assert!(error.to_string().contains(" uid 4242:"), "{error}");
    }

    /// Catches ids counted twice (one range listed under both the name and the uid,
    /// or two overlapping ranges, passing with fewer distinct ids than needed), and a
    /// total that wraps past `u64::MAX` to a small count instead of saturating.
    #[test]
    fn a_range_counts_its_distinct_ids_once() {
        for (name, text, count) in [
            ("twice", "kbf:100000:32768\n990:100000:32768\n", 32768),
            (
                "overlapping",
                "kbf:100000:40000\n990:120000:40000\nkbf:130000:5\n",
                60000,
            ),
        ] {
            let files = files(name, Some(text), Some(text));
            let error = check_subordinate_ids(&files, 990).expect_err(name);
            // Only `TooFew` says "subordinate ids;", after the count.
            let says = format!("(uid 990) {count} subordinate ids;");
            assert!(error.to_string().contains(&says), "{name}: {error}");
        }
        let huge = format!("kbf:1:{max}\n990:0:2\n", max = u64::MAX);
        let files = files("huge", Some(&huge), Some(&huge));
        check_subordinate_ids(&files, 990).expect("saturates");
    }

    /// Catches `--id-files` reading names other than the system files' in its directory.
    #[test]
    fn a_directory_holds_the_three_files() {
        assert_eq!(
            IdFiles::in_dir(Path::new("/d")),
            IdFiles {
                passwd: PathBuf::from("/d/passwd"),
                subuid: PathBuf::from("/d/subuid"),
                subgid: PathBuf::from("/d/subgid"),
            }
        );
    }

    /// Catches a range too small for an image's high ids (`nobody`), and malformed
    /// lines counting towards a range.
    #[test]
    fn a_short_or_malformed_range_is_refused() {
        let short = "kbf:100000:65535\nkbf:x:9\nkbf:1:2:3\nkbf:1:many\nkbf\n";
        let files = files("short", Some(short), Some(short));
        let error = check_subordinate_ids(&files, 990).expect_err("short");
        assert!(
            matches!(error, SubidError::TooFew { count: 65535, .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("65535 subordinate ids"),
            "{error}"
        );
    }

    /// Catches an unreadable passwd or subordinate id file being taken as "no range"
    /// (or passing): the error names the file.
    #[test]
    fn an_unreadable_file_names_itself() {
        let mut no_passwd = files("unreadable", Some(""), Some(""));
        no_passwd.passwd = no_passwd.passwd.with_file_name("absent");
        let error = check_subordinate_ids(&no_passwd, 990).expect_err("no passwd");
        assert!(matches!(error, SubidError::Read { .. }), "{error}");
        assert!(error.to_string().contains("absent"), "{error}");

        let mut files = files("unreadable-subuid", Some(""), Some(""));
        files.subuid = files.subuid.parent().expect("dir").to_owned();
        let error = check_subordinate_ids(&files, 990).expect_err("a directory");
        assert!(matches!(error, SubidError::Read { .. }), "{error}");
    }

    /// The system files are where the daemon reads them, and this process's user is
    /// the one checked: the result is the host's, but never a read error (every Linux
    /// host has /etc/passwd, and missing subordinate files mean no range).
    #[test]
    fn the_daemon_user_is_checked_against_the_system_files() {
        assert_eq!(
            IdFiles::system(),
            IdFiles {
                passwd: PathBuf::from("/etc/passwd"),
                subuid: PathBuf::from("/etc/subuid"),
                subgid: PathBuf::from("/etc/subgid"),
            }
        );
        let checked = check_daemon_user(&IdFiles::system());
        assert!(
            !matches!(checked, Err(SubidError::Read { .. })),
            "{checked:?}"
        );
    }
}

//! The nodes the server expects: the file `--expected-nodes` names.
//!
//! The server keeps what it knows of nodes in memory, so after a restart it lists only
//! the nodes that have registered since. A node that never comes back would vanish
//! from `GET /v1/nodes` without a trace. With this file, every node it lists that has
//! not registered since the server started is shown as `absent`, with the time the
//! server began expecting it ([`crate::fleet::with_expected`]).
//!
//! **The format.** UTF-8 text. Each line is blank, a `#` comment, or one node id, as
//! its daemon registers it, optionally followed by a `#` comment:
//!
//! ```text
//! # the Linux hosts
//! linux-1
//! linux-2      # back from repair
//! mac-07
//! ```
//!
//! A line with more than one word, or a node listed twice, is refused. The file must
//! be a regular file of at most [`MAX_EXPECTED_NODES_BYTES`]; it is opened without
//! blocking, so a FIFO is refused at once rather than waited on.
//!
//! **Reloads.** The file is read once at start; a file that cannot be read or parsed
//! then stops the server. After that, each `GET /v1/nodes` (and each write's answer)
//! takes the file's metadata, and reads it again if its size, inode, modification
//! time or change time differ from the last read. A node a reload adds is expected
//! from that reload on; a node it removes is no longer listed unless it registered. A
//! reload that fails (the file is gone, unreadable or does not parse) keeps the last
//! list read, so no node is dropped, and the reason is shown in the answer's
//! `expected_nodes_error` until a read succeeds.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The most bytes the file may hold.
pub const MAX_EXPECTED_NODES_BYTES: usize = 1 << 20;

/// Why the file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ExpectedNodesError {
    /// It cannot be read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// It is not a regular file, or is too large.
    #[error("{path} must be a regular file of at most {MAX_EXPECTED_NODES_BYTES} bytes")]
    NotAFile { path: PathBuf },
    /// It is not UTF-8.
    #[error("{path} is not UTF-8 text")]
    NotText { path: PathBuf },
    /// A line is not a node id.
    #[error("{path}:{line}: {why}")]
    Parse {
        path: PathBuf,
        line: usize,
        why: String,
    },
}

/// What the file lists now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expected {
    /// Each node listed, and when this server began expecting it: milliseconds since
    /// the Unix epoch, server clock.
    pub listed: BTreeMap<String, u64>,
    /// Why the newest reload failed, if it did (`listed` is then the last list read).
    pub error: Option<String>,
}

/// The file's metadata as of a read: what a reload compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    dev: u64,
    ino: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

#[derive(Debug)]
struct Loaded {
    /// The metadata of the newest read, whether it succeeded or not; `None` if the
    /// metadata could not be taken.
    stamp: Option<Stamp>,
    expected: Expected,
}

/// The `--expected-nodes` file, read again whenever its metadata changes.
#[derive(Debug)]
pub struct ExpectedNodes {
    path: PathBuf,
    loaded: Mutex<Loaded>,
}

impl ExpectedNodes {
    /// The file at `path`, read now, so that a bad file stops the server at start.
    /// Every node it lists is expected from now on.
    ///
    /// # Errors
    /// The file cannot be read, is not a regular file, is too large, is not UTF-8, or
    /// does not parse.
    pub fn open(path: &Path) -> Result<Self, ExpectedNodesError> {
        let (stamp, ids) = read(path)?;
        let now = unix_ms();
        let listed = ids.into_iter().map(|id| (id, now)).collect();
        Ok(Self {
            path: path.to_owned(),
            loaded: Mutex::new(Loaded {
                stamp: Some(stamp),
                expected: Expected {
                    listed,
                    error: None,
                },
            }),
        })
    }

    /// What the file lists now: read again first if its metadata changed since the
    /// last read (see the module docs).
    pub fn current(&self) -> Expected {
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        let stamp = std::fs::metadata(&self.path).ok().map(|m| stamp(&m));
        if stamp.is_some() && stamp == loaded.stamp {
            return loaded.expected.clone();
        }
        loaded.stamp = stamp;
        match read(&self.path) {
            Ok((read_stamp, ids)) => {
                loaded.stamp = Some(read_stamp);
                let now = unix_ms();
                let before = std::mem::take(&mut loaded.expected.listed);
                let listed = ids
                    .into_iter()
                    .map(|id| {
                        let since = before.get(&id).copied().unwrap_or(now);
                        (id, since)
                    })
                    .collect();
                if loaded.expected.error.take().is_some() {
                    tracing::info!(path = %self.path.display(), "--expected-nodes is readable again");
                }
                loaded.expected.listed = listed;
            }
            Err(e) => {
                let why = e.to_string();
                if loaded.expected.error.as_deref() != Some(why.as_str()) {
                    tracing::error!(error = %why, "--expected-nodes cannot be read; the last list read is kept");
                }
                loaded.expected.error = Some(why);
            }
        }
        loaded.expected.clone()
    }
}

/// The file's metadata, and the node ids it lists.
fn read(path: &Path) -> Result<(Stamp, Vec<String>), ExpectedNodesError> {
    use std::os::unix::fs::OpenOptionsExt;

    let unreadable = |source| ExpectedNodesError::Read {
        path: path.to_owned(),
        source,
    };
    // Non-blocking: opening a FIFO for reading would otherwise wait for a writer.
    let nonblocking = rustix::fs::OFlags::NONBLOCK.bits().cast_signed();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path)
        .map_err(unreadable)?;
    // Taken from the open file, so that it describes the bytes read.
    let meta = file.metadata().map_err(unreadable)?;
    let not_a_file = || ExpectedNodesError::NotAFile {
        path: path.to_owned(),
    };
    if !meta.is_file() {
        return Err(not_a_file());
    }
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_EXPECTED_NODES_BYTES + 1).unwrap_or(u64::MAX);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > MAX_EXPECTED_NODES_BYTES {
        return Err(not_a_file());
    }
    let text = String::from_utf8(bytes).map_err(|_| ExpectedNodesError::NotText {
        path: path.to_owned(),
    })?;
    let ids = parse(&text).map_err(|(line, why)| ExpectedNodesError::Parse {
        path: path.to_owned(),
        line,
        why,
    })?;
    Ok((stamp(&meta), ids))
}

/// The node ids `text` lists, in file order (format in the module docs).
///
/// # Errors
/// The 1-based number of the first bad line, and what is wrong with it.
pub fn parse(text: &str) -> Result<Vec<String>, (usize, String)> {
    let mut first_line: BTreeMap<&str, usize> = BTreeMap::new();
    let mut ids = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let line_no = at + 1;
        let content = line.split('#').next().unwrap_or_default();
        let mut words = content.split_whitespace();
        let (Some(id), None) = (words.next(), words.next()) else {
            if content.trim().is_empty() {
                continue;
            }
            return Err((line_no, "a line holds one node id".to_owned()));
        };
        if let Some(first) = first_line.insert(id, line_no) {
            return Err((line_no, format!("{id:?} is already listed on line {first}")));
        }
        ids.push(id.to_owned());
    }
    Ok(ids)
}

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let ns = |secs: i64, nanos: i64| i128::from(secs) * 1_000_000_000 + i128::from(nanos);
    Stamp {
        len: meta.len(),
        dev: meta.dev(),
        ino: meta.ino(),
        mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
    }
}

/// Now on the wall clock, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a comment or blank line taken as a node, a trailing comment kept as
    /// part of the id, and the file order lost.
    #[test]
    fn ids_comments_and_blank_lines() {
        let text = "# hosts\n\nlinux-2\n  mac-07   # repaired\n\t\nlinux-1#x\n";
        assert_eq!(
            parse(text),
            Ok(vec![
                "linux-2".to_owned(),
                "mac-07".to_owned(),
                "linux-1".to_owned()
            ])
        );
        assert_eq!(parse(""), Ok(Vec::new()));
    }

    /// Catches: a line of two words read as its first word (a typo such as
    /// `linux-1 linux-2` would silently expect one node), and a duplicate accepted.
    #[test]
    fn two_words_or_a_duplicate_are_refused_with_their_line() {
        assert_eq!(
            parse("linux-1\nlinux-2 linux-3\n"),
            Err((2, "a line holds one node id".to_owned()))
        );
        assert_eq!(
            parse("linux-1\n# x\nlinux-1 # again\n"),
            Err((3, "\"linux-1\" is already listed on line 1".to_owned()))
        );
    }
}

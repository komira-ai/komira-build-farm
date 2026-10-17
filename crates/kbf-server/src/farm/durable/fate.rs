//! The value of a `fate/<operation>` row: what a memory kill changed in the scheduler
//! ([`kbf_sched::Fate`]), written with the requeue that follows the kill.
//!
//! A version byte (1), the raised booking (8 bytes, big-endian), the number of memory
//! runs (4), then each run: the worker's name (2-byte length, then the bytes), its
//! booking (8), and how it was killed (0 under node pressure; 1 and the cap, 8, past its
//! own limit). A row of another version is refused, not guessed at.

use kbf_sched::Fate;
use kbf_types::{MemoryKill, MemoryRun, WorkerId};

const VERSION: u8 = 1;
const PRESSURE: u8 = 0;
const OWN_LIMIT: u8 = 1;

/// `fate` as a row value, or `None` if a worker's name is too long for it.
pub(super) fn encode(fate: &Fate) -> Option<Vec<u8>> {
    let mut row = vec![VERSION];
    row.extend(fate.booked_bytes.to_be_bytes());
    row.extend(u32::try_from(fate.memory_runs.len()).ok()?.to_be_bytes());
    for run in &fate.memory_runs {
        let name = run.worker.as_str().as_bytes();
        row.extend(u16::try_from(name.len()).ok()?.to_be_bytes());
        row.extend(name);
        row.extend(run.booked.to_be_bytes());
        match run.kill {
            MemoryKill::NodePressure => row.push(PRESSURE),
            MemoryKill::OwnLimit { cap } => {
                row.push(OWN_LIMIT);
                row.extend(cap.to_be_bytes());
            }
        }
    }
    Some(row)
}

/// A row value as a [`Fate`], or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Fate, &'static str> {
    let mut at = Reader(row);
    if at.take(1)? != [VERSION] {
        return Err("not a version 1 fate");
    }
    let booked_bytes = at.number()?;
    let count = u32::from_be_bytes(at.array()?);
    let mut memory_runs = Vec::new();
    for _ in 0..count {
        let len = usize::from(u16::from_be_bytes(at.array()?));
        let worker = std::str::from_utf8(at.take(len)?).map_err(|_| "a worker name is not text")?;
        let booked = at.number()?;
        let kill = match at.take(1)? {
            [PRESSURE] => MemoryKill::NodePressure,
            [OWN_LIMIT] => MemoryKill::OwnLimit { cap: at.number()? },
            _ => return Err("not a kind of memory kill"),
        };
        memory_runs.push(MemoryRun {
            worker: WorkerId::new(worker),
            booked,
            kill,
        });
    }
    if !at.0.is_empty() {
        return Err("bytes after the last run");
    }
    Ok(Fate {
        booked_bytes,
        memory_runs,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let (head, rest) = self.0.split_at_checked(n).ok_or("the row ends early")?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let head = self.take(N)?;
        Ok(<[u8; N]>::try_from(head).expect("take returns N bytes"))
    }

    fn number(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_be_bytes(self.array()?))
    }
}

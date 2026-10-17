//! The value of a `fate/<operation>` row: what an operation's faults changed in the
//! scheduler ([`kbf_sched::Fate`]), written with the requeue that follows each.
//!
//! A version byte, then the raised booking (8 bytes, big-endian), the number of memory
//! runs (4), then each run: the worker's name (2-byte length, then the bytes), its
//! booking (8), and how it was killed (0 under node pressure; 1 and the cap, 8, past its
//! own limit). Version 1 ends there. Version 2 goes on with the farm runs (a count, 4,
//! then each: the worker's name and how it ended, 0 under node pressure, 1 and the reason
//! the farm lost it, 2 for an infra failure), the workers placement avoids (a count, 4,
//! then each name) and the leases given up as not started (4). A version 1 row (written
//! before the budgets were) has none of those. A row of another version is refused, not
//! guessed at.

use std::collections::BTreeSet;

use kbf_sched::Fate;
use kbf_types::{FarmFault, FarmRun, LostWhy, MemoryKill, MemoryRun, WorkerId};

const VERSION: u8 = 2;
const FIRST_VERSION: u8 = 1;
const PRESSURE: u8 = 0;
const OWN_LIMIT: u8 = 1;
const FAULT_PRESSURE: u8 = 0;
const FAULT_LOST: u8 = 1;
const FAULT_INFRA: u8 = 2;
const DAEMON_RESTARTED: u8 = 0;
const FENCED: u8 = 1;

fn name(row: &mut Vec<u8>, worker: &WorkerId) -> Option<()> {
    let bytes = worker.as_str().as_bytes();
    row.extend(u16::try_from(bytes.len()).ok()?.to_be_bytes());
    row.extend(bytes);
    Some(())
}

fn count(row: &mut Vec<u8>, n: usize) -> Option<()> {
    row.extend(u32::try_from(n).ok()?.to_be_bytes());
    Some(())
}

/// `fate` as a row value, or `None` if a worker's name or a list is too long for it.
pub(super) fn encode(fate: &Fate) -> Option<Vec<u8>> {
    let mut row = vec![VERSION];
    row.extend(fate.booked_bytes.to_be_bytes());
    count(&mut row, fate.memory_runs.len())?;
    for run in &fate.memory_runs {
        name(&mut row, &run.worker)?;
        row.extend(run.booked.to_be_bytes());
        match run.kill {
            MemoryKill::NodePressure => row.push(PRESSURE),
            MemoryKill::OwnLimit { cap } => {
                row.push(OWN_LIMIT);
                row.extend(cap.to_be_bytes());
            }
        }
    }
    count(&mut row, fate.farm_runs.len())?;
    for run in &fate.farm_runs {
        name(&mut row, &run.worker)?;
        match run.fault {
            FarmFault::NodeMemoryPressure => row.push(FAULT_PRESSURE),
            FarmFault::Lost(why) => {
                row.push(FAULT_LOST);
                row.push(match why {
                    LostWhy::DaemonRestarted => DAEMON_RESTARTED,
                    LostWhy::Fenced => FENCED,
                });
            }
            FarmFault::Infra => row.push(FAULT_INFRA),
        }
    }
    count(&mut row, fate.avoid.len())?;
    for worker in &fate.avoid {
        name(&mut row, worker)?;
    }
    row.extend(fate.not_started.to_be_bytes());
    Some(row)
}

/// A row value as a [`Fate`], or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Fate, &'static str> {
    let mut at = Reader(row);
    let version = at.array::<1>()?[0];
    if version != VERSION && version != FIRST_VERSION {
        return Err("not a version 1 or 2 fate");
    }
    let booked_bytes = at.number()?;
    let mut memory_runs = Vec::new();
    for _ in 0..u32::from_be_bytes(at.array()?) {
        let worker = at.worker()?;
        let booked = at.number()?;
        let kill = match at.take(1)? {
            [PRESSURE] => MemoryKill::NodePressure,
            [OWN_LIMIT] => MemoryKill::OwnLimit { cap: at.number()? },
            _ => return Err("not a kind of memory kill"),
        };
        memory_runs.push(MemoryRun {
            worker,
            booked,
            kill,
        });
    }
    let mut fate = Fate {
        booked_bytes,
        memory_runs,
        ..Fate::default()
    };
    if version == VERSION {
        for _ in 0..u32::from_be_bytes(at.array()?) {
            let worker = at.worker()?;
            let fault = match at.take(1)? {
                [FAULT_PRESSURE] => FarmFault::NodeMemoryPressure,
                [FAULT_LOST] => FarmFault::Lost(match at.take(1)? {
                    [DAEMON_RESTARTED] => LostWhy::DaemonRestarted,
                    [FENCED] => LostWhy::Fenced,
                    _ => return Err("not a reason the farm lost a run"),
                }),
                [FAULT_INFRA] => FarmFault::Infra,
                _ => return Err("not a kind of farm fault"),
            };
            fate.farm_runs.push(FarmRun { worker, fault });
        }
        let mut avoid = BTreeSet::new();
        for _ in 0..u32::from_be_bytes(at.array()?) {
            avoid.insert(at.worker()?);
        }
        fate.avoid = avoid;
        fate.not_started = u32::from_be_bytes(at.array()?);
    }
    if !at.0.is_empty() {
        return Err("bytes after the last field");
    }
    Ok(fate)
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

    fn worker(&mut self) -> Result<WorkerId, &'static str> {
        let len = usize::from(u16::from_be_bytes(self.array()?));
        let text = std::str::from_utf8(self.take(len)?).map_err(|_| "a worker name is not text")?;
        Ok(WorkerId::new(text))
    }
}

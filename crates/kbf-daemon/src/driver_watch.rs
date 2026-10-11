//! One driver watch with a part per source: the [`DriverReport`] channel the daemon
//! takes ([`crate::Daemon::with_driver_report`]) when more than one survey feeds it,
//! as the native driver's Xcode survey does now and its iOS device survey will
//! (`docs/design/ios-devices.md`, section 7.5).
//!
//! [`DriverWatch::new`] makes the channel. Each source takes the part named for it
//! ([`DriverWatch::part`]; a name has at most one part, so two producers can never
//! overwrite each other) and sends its own report through it ([`DriverPart::send`]).
//! A send replaces that part's report alone, and the channel then carries the merge of
//! every part's newest report: their entries and their Xcodes, part by part in the
//! order of the sources' names, each part's in the order it sent them. A part that has
//! not sent yet adds nothing; a part that is dropped keeps its last report in the
//! merge. Every send is sent on to the daemon, changed or not: the sources send only
//! what differs from their last survey.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use crate::status::DriverReport;

/// The sender side of the daemon's driver channel, split into parts by source. See the
/// module documentation.
#[derive(Debug)]
pub struct DriverWatch {
    shared: Arc<Shared>,
}

/// What every part shares: each source's newest report, and the channel their merge
/// goes out on.
#[derive(Debug)]
struct Shared {
    parts: Mutex<BTreeMap<&'static str, DriverReport>>,
    send: watch::Sender<DriverReport>,
}

impl Shared {
    fn parts(&self) -> MutexGuard<'_, BTreeMap<&'static str, DriverReport>> {
        // Each update is one insert, so the map is whole after a panic.
        self.parts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl DriverWatch {
    /// A watch with no part yet, and the channel the daemon takes, which carries an
    /// empty report until a part sends.
    #[must_use]
    pub fn new() -> (Self, watch::Receiver<DriverReport>) {
        let (send, receive) = watch::channel(DriverReport::default());
        let shared = Arc::new(Shared {
            parts: Mutex::default(),
            send,
        });
        (Self { shared }, receive)
    }

    /// The part of `source`, or `None` when `source` has one already.
    #[must_use]
    pub fn part(&self, source: &'static str) -> Option<DriverPart> {
        let mut parts = self.shared.parts();
        if parts.contains_key(source) {
            return None;
        }
        parts.insert(source, DriverReport::default());
        Some(DriverPart {
            source,
            shared: Arc::clone(&self.shared),
        })
    }
}

/// One source's part of a [`DriverWatch`].
#[derive(Debug)]
pub struct DriverPart {
    source: &'static str,
    shared: Arc<Shared>,
}

impl DriverPart {
    /// The source this part is named for.
    #[must_use]
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// Makes `report` this part's newest and sends the daemon the merge of every
    /// part's newest report.
    pub fn send(&self, report: DriverReport) {
        let mut parts = self.shared.parts();
        parts.insert(self.source, report);
        // Under the lock: of two parts sending at once, the one that stores its report
        // second also sends second, so the channel ends with the merge of both.
        self.shared.send.send_replace(merge(&parts));
    }

    /// Whether every receiver of the daemon's channel is gone.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.send.is_closed()
    }
}

/// Every part's entries and Xcodes, part by part in the order of the sources' names.
fn merge(parts: &BTreeMap<&'static str, DriverReport>) -> DriverReport {
    let mut merged = DriverReport::default();
    for part in parts.values() {
        merged.entries.extend(part.entries.iter().cloned());
        merged.xcodes.extend(part.xcodes.iter().cloned());
    }
    merged
}

#[cfg(test)]
mod tests {
    use kbf_proto::worker::{XcodeState, XcodeStatus};

    use super::*;

    fn entry(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn xcode(build: &str, state: XcodeState) -> XcodeStatus {
        XcodeStatus {
            app: format!("/Applications/Xcode_{build}.app"),
            build: build.to_owned(),
            state: state.into(),
            ..XcodeStatus::default()
        }
    }

    /// Catches: one source's send replacing the whole report, so the other source's
    /// entries (here the Xcode build the node advertises) leave the Hello; a second
    /// source's send dropping the first's Xcodes, so the server sees the not-ready
    /// Xcode gone and clears its attention item; a part's new report added to its old
    /// one instead of replacing it; and the merge out of the sources' order.
    #[test]
    fn two_sources_update_independently() {
        let (watch, mut reports) = DriverWatch::new();
        assert_eq!(*reports.borrow_and_update(), DriverReport::default());
        let xcodes = watch.part("xcode").expect("the xcode part");
        let devices = watch.part("devices").expect("the devices part");
        assert_eq!((xcodes.source(), devices.source()), ("xcode", "devices"));
        // Taking a part sends nothing.
        assert!(!reports.has_changed().expect("open"));

        let licence = xcode("17A1", XcodeState::LicenseNotAccepted);
        let ready = xcode("16B40", XcodeState::Ready);
        xcodes.send(DriverReport {
            entries: vec![entry("xcode", "16B40")],
            xcodes: vec![ready.clone(), licence.clone()],
        });
        assert!(reports.has_changed().expect("open"));
        assert_eq!(
            *reports.borrow_and_update(),
            DriverReport {
                entries: vec![entry("xcode", "16B40")],
                xcodes: vec![ready.clone(), licence.clone()],
            }
        );

        let phone = entry("ios.device", "00008110-001A");
        devices.send(DriverReport {
            entries: vec![phone.clone()],
            xcodes: Vec::new(),
        });
        assert_eq!(
            *reports.borrow_and_update(),
            DriverReport {
                entries: vec![phone.clone(), entry("xcode", "16B40")],
                xcodes: vec![ready.clone(), licence.clone()],
            },
            "the devices part joins the xcode part, in the sources' order"
        );

        let accepted = xcode("17A1", XcodeState::Ready);
        xcodes.send(DriverReport {
            entries: vec![entry("xcode", "16B40"), entry("xcode", "17A1")],
            xcodes: vec![ready.clone(), accepted.clone()],
        });
        assert_eq!(
            *reports.borrow_and_update(),
            DriverReport {
                entries: vec![
                    phone.clone(),
                    entry("xcode", "16B40"),
                    entry("xcode", "17A1"),
                ],
                xcodes: vec![ready.clone(), accepted.clone()],
            },
            "the xcode part replaced, the devices part kept"
        );

        devices.send(DriverReport::default());
        drop(devices);
        assert_eq!(
            *reports.borrow_and_update(),
            DriverReport {
                entries: vec![entry("xcode", "16B40"), entry("xcode", "17A1")],
                xcodes: vec![ready, accepted],
            },
            "the devices part emptied, the xcode part kept"
        );
    }

    /// Catches: a second part for a source that has one (two producers of one part
    /// overwrite each other, the defect the parts exist to prevent), and a part that is
    /// dropped taking its report out of the merge or freeing its name.
    #[test]
    fn a_source_has_one_part() {
        let (watch, reports) = DriverWatch::new();
        let xcodes = watch.part("xcode").expect("the xcode part");
        assert!(watch.part("xcode").is_none(), "a second xcode part");
        xcodes.send(DriverReport {
            entries: vec![entry("xcode", "16B40")],
            xcodes: Vec::new(),
        });
        drop(xcodes);
        assert!(watch.part("xcode").is_none(), "the name freed by a drop");
        let devices = watch.part("devices").expect("the devices part");
        devices.send(DriverReport::default());
        assert_eq!(reports.borrow().entries, [entry("xcode", "16B40")]);
    }

    /// Catches: `is_closed` true while the daemon's receiver lives (the Xcode watch's
    /// thread would end at once) or false once every receiver is gone (the thread
    /// would outlive the daemon).
    #[test]
    fn a_part_sees_the_receivers_go() {
        let (watch, reports) = DriverWatch::new();
        let part = watch.part("xcode").expect("the xcode part");
        let other = reports.clone();
        drop(reports);
        assert!(!part.is_closed());
        drop(other);
        assert!(part.is_closed());
    }
}

//! Watching a macOS update through the gate's DDM status, startup reconciliation, and
//! what the catalogue offers a Mac. The gate here is in memory; time is tokio's
//! paused clock and a counter the fake advances on every poll.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_mdm_api::catalogue::CatalogueEntry;
use kbf_mdm_api::names::{Date, LocalDateTime, OsVersion, Serial, Sha256Hex};
use kbf_server::mdm::progress::{
    EXPIRY_WARNING_DAYS, POLL_INTERVAL, Progress, Watch, WatchEnd, assess, expires_soon, reconcile,
    stale_enforcements, updates_for,
};
use kbf_server::mdm::{
    Catalogue, EnforceOrder, Enforcement, GateError, GateFuture, Inventory, MacStatus, MdmGate,
    SharedGate, SoftwareUpdateStatus,
};

fn serial(s: &str) -> Serial {
    Serial::new(s).unwrap()
}

fn enforcement(s: &str, build: &str) -> Enforcement {
    Enforcement {
        declaration_identifier: format!("kbf.osupdate.{s}"),
        target_os_version: OsVersion::parse("27.0.1").unwrap(),
        target_build: build.to_owned(),
        target_local_date_time: LocalDateTime::parse("2026-10-08T14:05:00").unwrap(),
    }
}

fn mac(s: &str, os_version: &str, os_build: &str, e: Option<Enforcement>) -> MacStatus {
    MacStatus {
        serial: serial(s),
        platform_uuid: String::new(),
        pool: "mac".to_owned(),
        enrolled: true,
        supervised: true,
        bootstrap_token_escrowed: true,
        last_check_in_unix_ms: Some(1),
        os_version: os_version.to_owned(),
        os_build: os_build.to_owned(),
        supplemental_build: String::new(),
        software_update: SoftwareUpdateStatus::default(),
        profiles: vec![],
        enforcement: e,
    }
}

fn inventory(macs: Vec<MacStatus>) -> Inventory {
    Inventory {
        macs,
        catalogue: Catalogue::default(),
    }
}

/// A Mac being updated to 26A434, with DDM's install state and failure reason.
fn updating(state: &str, failure: &str) -> Inventory {
    let mut m = mac(
        "C02X",
        "27.0",
        "26A400",
        Some(enforcement("C02X", "26A434")),
    );
    m.software_update = SoftwareUpdateStatus {
        install_state: state.to_owned(),
        pending_build: "26A434".to_owned(),
        failure_reason: failure.to_owned(),
        failure_count: u32::from(!failure.is_empty()),
        ..SoftwareUpdateStatus::default()
    };
    inventory(vec![m])
}

/// A gate in memory: `status` answers from a queue (the last answer repeats) and
/// advances the clock by `step_ms`; `withdraw` records the serial, failing for one.
struct Memory {
    answers: Mutex<VecDeque<Result<Inventory, GateError>>>,
    clock: Arc<AtomicU64>,
    step_ms: u64,
    withdrawn: Mutex<Vec<Serial>>,
    fail_withdraw: Option<Serial>,
    asked: Mutex<Vec<Vec<Serial>>>,
}

impl Memory {
    fn new(answers: Vec<Result<Inventory, GateError>>, step_ms: u64) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            clock: Arc::new(AtomicU64::new(0)),
            step_ms,
            withdrawn: Mutex::new(vec![]),
            fail_withdraw: None,
            asked: Mutex::new(vec![]),
        }
    }

    fn polls(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl MdmGate for Memory {
    fn status<'a>(&'a self, serials: &'a [Serial]) -> GateFuture<'a, Inventory> {
        self.asked.lock().unwrap().push(serials.to_vec());
        self.clock.fetch_add(self.step_ms, Ordering::SeqCst);
        let mut answers = self.answers.lock().unwrap();
        let answer = if answers.len() > 1 {
            answers.pop_front()
        } else {
            answers.front().cloned()
        };
        Box::pin(async move { answer.expect("an answer is queued") })
    }
    fn enforce<'a>(&'a self, _: &'a EnforceOrder) -> GateFuture<'a, Enforcement> {
        Box::pin(async { Err(GateError::Unavailable("not used here".to_owned())) })
    }
    fn withdraw<'a>(&'a self, serial: &'a Serial) -> GateFuture<'a, Option<Enforcement>> {
        let result = if self.fail_withdraw.as_ref() == Some(serial) {
            Err(GateError::Unavailable("down".to_owned()))
        } else {
            self.withdrawn.lock().unwrap().push(serial.clone());
            Ok(None)
        };
        Box::pin(async move { result })
    }
    fn install_profile<'a>(&'a self, _: &'a Serial, _: &'a Sha256Hex) -> GateFuture<'a, String> {
        Box::pin(async { Err(GateError::Unavailable("not used here".to_owned())) })
    }
}

/// Catches: DDM reporting the target build taken as the done signal (on macOS 27 only
/// the node's Hello is), a failure-reason ignored, an enforcement for another build
/// or none at all taken as progress, and a missing Mac taken as pending.
#[test]
fn assess_reads_one_answer() {
    let s = serial("C02X");
    assert_eq!(
        assess(&updating("installing", ""), &s, "26A434"),
        Progress::Pending {
            install_state: "installing".to_owned(),
            pending_build: "26A434".to_owned(),
            os_build: "26A400".to_owned(),
        }
    );
    // DDM already shows the target build: still pending.
    let mut done_by_ddm = updating("none", "");
    done_by_ddm.macs[0].os_build = "26A434".to_owned();
    assert!(matches!(
        assess(&done_by_ddm, &s, "26A434"),
        Progress::Pending { .. }
    ));

    assert_eq!(
        assess(&updating("failed", "InsufficientDiskSpace"), &s, "26A434"),
        Progress::Failed {
            reason: "InsufficientDiskSpace".to_owned(),
            count: 1
        }
    );
    assert_eq!(
        assess(&updating("installing", ""), &s, "26A999"),
        Progress::OtherTarget(enforcement("C02X", "26A434"))
    );
    let none = inventory(vec![mac("C02X", "27.0", "26A400", None)]);
    assert_eq!(assess(&none, &s, "26A434"), Progress::NotEnforced);
    assert_eq!(assess(&none, &serial("C02Y"), "26A434"), Progress::Missing);
}

fn watch_for(give_up_at_unix_ms: u64) -> Watch {
    Watch {
        serial: serial("C02X"),
        target_build: "26A434".to_owned(),
        give_up_at_unix_ms,
        interval: POLL_INTERVAL,
    }
}

/// Runs `watch` with `done` never firing unless `done_after_polls` polls happened;
/// returns its end and every report.
async fn run(gate: &Memory, w: &Watch, done_after_polls: Option<usize>) -> (WatchEnd, Vec<String>) {
    let mut reports = Vec::new();
    let clock = Arc::clone(&gate.clock);
    let done = async {
        match done_after_polls {
            Some(n) => {
                while gate.polls() < n {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            None => std::future::pending().await,
        }
    };
    let watching = kbf_server::mdm::progress::watch(
        gate,
        w,
        done,
        || clock.load(Ordering::SeqCst),
        |r| {
            reports.push(match r {
                Ok(p) => format!("{p:?}"),
                Err(e) => e.to_string(),
            });
        },
    );
    // A watch that never ends fails here (after a day of paused time), not by hanging.
    let end = tokio::time::timeout(Duration::from_secs(86_400), watching)
        .await
        .expect("the watch ended within a day");
    (end, reports)
}

/// Catches: polling that stops at the first pending answer, never polls again, or
/// treats an unreachable gate as an end; a done signal that is not honoured.
#[tokio::test(start_paused = true)]
async fn watch_polls_until_done_and_survives_an_unreachable_gate() {
    let gate = Memory::new(
        vec![
            Ok(updating("downloading", "")),
            Err(GateError::Unavailable("gate restarting".to_owned())),
            Ok(updating("installing", "")),
        ],
        1,
    );
    let start = tokio::time::Instant::now();
    let (end, reports) = run(&gate, &watch_for(u64::MAX), Some(4)).await;
    assert_eq!(end, WatchEnd::Done);
    assert_eq!(gate.polls(), 4);
    assert_eq!(reports.len(), 4);
    assert!(reports[0].contains("downloading"), "{reports:?}");
    assert!(reports[1].contains("gate restarting"), "{reports:?}");
    assert!(reports[2].contains("installing"), "{reports:?}");
    // Polled once at once, then once per interval.
    assert!(
        start.elapsed() >= POLL_INTERVAL * 3,
        "{:?}",
        start.elapsed()
    );
    assert!(start.elapsed() < POLL_INTERVAL * 4, "{:?}", start.elapsed());
    assert_eq!(gate.asked.lock().unwrap()[0], vec![serial("C02X")]);
}

/// Catches: a DDM failure-reason that does not end the watch (it must hold the
/// rollout), and a watch that waits only for the return deadline.
#[tokio::test(start_paused = true)]
async fn a_failure_reason_ends_the_watch_at_once() {
    let gate = Memory::new(
        vec![
            Ok(updating("installing", "")),
            Ok(updating("failed", "NoNetwork")),
        ],
        1,
    );
    let (end, reports) = run(&gate, &watch_for(u64::MAX), None).await;
    assert_eq!(
        end,
        WatchEnd::Failed {
            reason: "NoNetwork".to_owned()
        }
    );
    assert_eq!((gate.polls(), reports.len()), (2, 2));
}

/// Catches: a watch that never gives up when no Hello arrives by the deadline plus
/// the return deadline, or gives up early.
#[tokio::test(start_paused = true)]
async fn the_watch_gives_up_at_the_deadline() {
    // Each poll advances the clock 1000 ms; the give-up time is reached after 3 polls.
    let gate = Memory::new(vec![Ok(updating("installing", ""))], 1000);
    let (end, reports) = run(&gate, &watch_for(3000), None).await;
    assert_eq!(end, WatchEnd::Overdue);
    assert_eq!((gate.polls(), reports.len()), (3, 3));
}

/// Catches: DDM reporting the target build taken as completion. On macOS 27 nothing
/// announces it; only the node's Hello is the done signal, so with no Hello the watch
/// runs on to the deadline.
#[tokio::test(start_paused = true)]
async fn ddm_showing_the_target_build_is_not_done() {
    let mut answer = updating("none", "");
    answer.macs[0].os_build = "26A434".to_owned();
    let gate = Memory::new(vec![Ok(answer)], 1000);
    let (end, reports) = run(&gate, &watch_for(3000), None).await;
    assert_eq!(end, WatchEnd::Overdue);
    assert_eq!(reports.len(), 3);
}

/// Catches: an enforcement withdrawn or replaced, or a Mac gone from the inventory,
/// read as still pending.
#[tokio::test(start_paused = true)]
async fn a_lost_enforcement_ends_the_watch() {
    let withdrawn = inventory(vec![mac("C02X", "27.0", "26A400", None)]);
    for (answer, want) in [
        (withdrawn, Progress::NotEnforced),
        (inventory(vec![]), Progress::Missing),
        (
            inventory(vec![mac(
                "C02X",
                "27.0",
                "26A400",
                Some(enforcement("C02X", "26A999")),
            )]),
            Progress::OtherTarget(enforcement("C02X", "26A999")),
        ),
    ] {
        let gate = Memory::new(vec![Ok(updating("downloading", "")), Ok(answer)], 1);
        let (end, _) = run(&gate, &watch_for(u64::MAX), None).await;
        assert_eq!(end, WatchEnd::Lost(want));
    }
}

/// Catches: reconciliation that withdraws an enforcement a durable step expects, keeps
/// one for a build no step names, or one for a Mac no step names; and one that
/// reports success after a failed withdrawal or a failed status.
#[tokio::test]
async fn reconcile_withdraws_only_unexpected_enforcements() {
    let inv = inventory(vec![
        mac("A1", "27.0", "26A400", Some(enforcement("A1", "26A434"))),
        mac("B2", "27.0", "26A400", Some(enforcement("B2", "26A434"))),
        mac("C3", "27.0", "26A400", Some(enforcement("C3", "26A434"))),
        mac("D4", "27.0", "26A400", None),
    ]);
    let expected: BTreeMap<Serial, String> = [
        (serial("A1"), "26A434".to_owned()),
        (serial("B2"), "26A999".to_owned()),
        (serial("D4"), "26A434".to_owned()),
    ]
    .into();
    assert_eq!(
        stale_enforcements(&inv, &expected),
        vec![serial("B2"), serial("C3")]
    );

    let gate = Memory::new(vec![Ok(inv.clone())], 0);
    assert_eq!(
        reconcile(&gate, &expected).await,
        Ok(vec![serial("B2"), serial("C3")])
    );
    assert_eq!(
        *gate.withdrawn.lock().unwrap(),
        vec![serial("B2"), serial("C3")]
    );
    assert_eq!(gate.asked.lock().unwrap()[0], Vec::<Serial>::new());

    let mut failing = Memory::new(vec![Ok(inv)], 0);
    failing.fail_withdraw = Some(serial("B2"));
    assert_eq!(
        reconcile(&failing, &expected).await,
        Err(GateError::Unavailable("down".to_owned()))
    );
    assert!(failing.withdrawn.lock().unwrap().is_empty());

    let down = Memory::new(vec![Err(GateError::Unavailable("gate down".to_owned()))], 0);
    assert_eq!(
        reconcile(&down, &expected).await,
        Err(GateError::Unavailable("gate down".to_owned()))
    );

    // The trait is object-safe: the server shares one gate.
    let shared: SharedGate = Arc::new(Memory::new(vec![Ok(inventory(vec![]))], 0));
    assert_eq!(
        reconcile(shared.as_ref(), &BTreeMap::new()).await,
        Ok(vec![])
    );
}

fn entry(version: &str, build: &str, expires: &str, public: bool) -> CatalogueEntry {
    CatalogueEntry {
        product_version: OsVersion::parse(version).unwrap(),
        build: build.to_owned(),
        posting_date: Date::parse("2026-09-01").unwrap(),
        expiration_date: Date::parse(expires).unwrap(),
        supported_devices: vec![],
        public,
    }
}

/// Catches: older or equal releases offered as updates, versions compared as text,
/// a release listed twice (public and managed) offered twice, the wrong expiry for
/// the Mac's own build, and an unknown version taken as "everything is newer".
#[test]
fn updates_for_a_mac_come_from_the_catalogue() {
    let catalogue = Catalogue {
        fetched_at_unix_ms: Some(1),
        entries: vec![
            entry("27.0.1", "26A434", "2027-01-06", true),
            entry("26.7.1", "25G241", "2027-01-06", true),
            entry("27.10", "26X1", "2027-03-01", true),
            entry("27.0", "26A400", "2026-10-25", false),
            entry("27.0", "26A400", "2026-10-20", true),
            entry("27.0.1", "26A434", "2027-01-06", false),
        ],
    };
    let m = mac("C02X", "27.0", "26A400", None);
    let u = updates_for(&m, &catalogue);
    let newer: Vec<(&str, bool)> = u
        .newer
        .iter()
        .map(|e| (e.build.as_str(), e.public))
        .collect();
    assert_eq!(newer, vec![("26X1", true), ("26A434", true)]);
    assert_eq!(
        u.current_build_expires,
        Some(Date::parse("2026-10-25").unwrap())
    );

    let unknown = mac("C02X", "", "", None);
    let u = updates_for(&unknown, &catalogue);
    assert!(u.newer.is_empty());
    assert_eq!(u.current_build_expires, None);
}

/// Catches: the 14-day alert fired a day late or early, or never after expiry.
#[test]
fn a_pinned_build_alerts_14_days_before_it_expires() {
    let expires = Date::parse("2026-10-25").unwrap();
    let day = |d: i64| Date::from_days_since_epoch(expires.days_since_epoch() - d);
    assert!(!expires_soon(expires, day(EXPIRY_WARNING_DAYS + 1)));
    assert!(expires_soon(expires, day(EXPIRY_WARNING_DAYS)));
    assert!(expires_soon(expires, day(0)));
    assert!(expires_soon(expires, day(-3)));
}

/// Catches: a release offered twice when one version has two builds and Apple's feed
/// lists both in both lists (`[v b1 public, v b2 public, v b1, v b2]`, the real
/// feed's shape), so the copies of one build are not neighbours after a sort by
/// version; and the managed copy kept where a public one exists.
#[test]
fn a_build_listed_in_both_lists_is_offered_once_preferring_the_public_entry() {
    let catalogue = Catalogue {
        fetched_at_unix_ms: Some(1),
        entries: vec![
            entry("27.0.1", "26A434", "2027-01-06", true),
            entry("27.0.1", "26A5434", "2027-01-06", true),
            entry("27.0.1", "26A434", "2027-01-06", false),
            entry("27.0.1", "26A5434", "2027-01-06", false),
            entry("27.0", "26A400", "2027-01-06", false),
            entry("27.0", "26A400", "2027-01-06", true),
        ],
    };
    let newer: Vec<(String, String, bool)> =
        updates_for(&mac("C02X", "26.7", "25G1", None), &catalogue)
            .newer
            .into_iter()
            .map(|e| (e.product_version.to_string(), e.build, e.public))
            .collect();
    let public = |v: &str, b: &str| (v.to_owned(), b.to_owned(), true);
    assert_eq!(
        newer,
        vec![
            public("27.0.1", "26A434"),
            public("27.0.1", "26A5434"),
            public("27.0", "26A400"),
        ]
    );
}

/// Catches: an enforcement kept although Apple's catalogue no longer lists its build
/// (`fleet-updates.md` 7.2: a declaration naming a version no longer available is
/// removed), and one withdrawn for that reason when the gate has never read the
/// catalogue (an empty catalogue proves nothing).
#[tokio::test]
async fn reconcile_withdraws_an_enforcement_whose_build_left_the_catalogue() {
    let macs = vec![
        mac("A1", "27.0", "26A400", Some(enforcement("A1", "26A434"))),
        mac("B2", "27.0", "26A400", Some(enforcement("B2", "26A440"))),
    ];
    let expected: BTreeMap<Serial, String> = [
        (serial("A1"), "26A434".to_owned()),
        (serial("B2"), "26A440".to_owned()),
    ]
    .into();
    let read = Inventory {
        macs: macs.clone(),
        catalogue: Catalogue {
            fetched_at_unix_ms: Some(1),
            entries: vec![entry("27.0.1", "26A434", "2027-01-06", false)],
        },
    };
    assert_eq!(stale_enforcements(&read, &expected), vec![serial("B2")]);
    let gate = Memory::new(vec![Ok(read)], 0);
    assert_eq!(reconcile(&gate, &expected).await, Ok(vec![serial("B2")]));
    assert_eq!(*gate.withdrawn.lock().unwrap(), vec![serial("B2")]);

    let never_read = Inventory {
        macs,
        catalogue: Catalogue::default(),
    };
    assert!(stale_enforcements(&never_read, &expected).is_empty());
}

//! The helper's passes on a scratch host: the real installer verbs over a real file tree,
//! with the one thing a test cannot run (the attestation check of a stage) played by a
//! closure, and the service restart recorded as a step.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::{Root, Step};
use kbf_installer::activate;
use kbf_installer::attest::SourceRef;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::refusal::{InboxFault, Refusal};
use kbf_installer::stage::Staged;
use kbf_installer::state::{Locked, State};
use kbf_installer::testkit::{A, B, c};

use crate::cycle::{Config, Helper};
use crate::restart::Restart;

const UNIT: &str = "kbf.service";
const T0: u64 = 1_800_000_000;

struct Rig {
    helper: Helper<ScratchHost>,
    root: PathBuf,
    layout: Layout,
    /// The restarts the start-up made (the links were missing and were made).
    base: usize,
}

fn order_json(id: u64, verb: &str, commit: &str) -> String {
    format!(
        "{{\"order_id\":{id},\"epoch\":1,\"verb\":\"{verb}\",\"commit\":\"{commit}\",\"window_s\":600}}"
    )
}

impl Rig {
    /// A host running release A, with release B installed beside it, an empty inbox, and
    /// the service restart recorded.
    fn new(name: &str) -> Rig {
        Rig::on(name, Platform::Linux)
    }

    /// The same host on `platform`: macOS keeps its state and lock elsewhere.
    fn on(name: &str, platform: Platform) -> Rig {
        let mut host = ScratchHost::new(&format!("helper-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/run",
            "var/db/kbf-deploy",
            "var/lib/kbf-deploy",
            "var/lib/kbf-updater/inbox/orders",
            "usr/local/kbf/releases",
        ] {
            fs::create_dir_all(root.join(sub)).expect("seed");
        }
        let dir = root.join("usr/local/kbf/releases").join(A);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        host.answer = Box::new(|_| Ok(reply(0, "", "")));
        let layout = Layout::new(Root::new(&root).expect("absolute"), platform);
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(A)),
                ..State::default()
            })
            .expect("state");
        let cfg = Config {
            update_dir: kbf_host::HostPath::new("/var/lib/kbf-updater").expect("path"),
            restart: Restart::Systemd(UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        };
        let mut helper = Helper::new(layout.clone(), host, cfg);
        helper.recover().expect("recover");
        let mut rig = Rig {
            helper,
            root,
            layout,
            base: 0,
        };
        rig.cycle(T0 - 100);
        rig.base = rig.restarts();
        rig
    }

    fn orders(&self) -> PathBuf {
        self.root.join("var/lib/kbf-updater/inbox/orders")
    }

    fn put_order(&self, id: u64, body: &str) {
        fs::write(self.orders().join(format!("{id:020}.json")), body).expect("order");
    }

    fn status(&self) -> serde_json::Value {
        let bytes = fs::read(self.root.join("var/lib/kbf-updater/status.json")).expect("status");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn state(&self) -> State {
        kbf_installer::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    fn restarts(&self) -> usize {
        self.helper
            .exec()
            .log
            .iter()
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", "--", UNIT]))
            .count()
            - self.base
    }

    fn cycle(&mut self, now: u64) {
        let mut never = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
            panic!("no offer is expected")
        };
        self.helper.cycle(now, &mut never).expect("a pass");
    }

    /// An inbox entry for `commit`, as the node's client names it.
    fn put_offer(&self, generation: u64, commit: &str) -> String {
        let name = format!("{generation:020}-{commit}");
        fs::create_dir_all(self.root.join("var/lib/kbf-updater/inbox").join(&name)).expect("entry");
        name
    }
}

fn staged(commit: &str) -> Staged {
    Staged {
        commit: c(commit),
        components: vec!["kbf"],
        source_ref: SourceRef::new("refs/heads/main").expect("ref"),
        protocol: 7,
        min_protocol: 6,
        state_schema: 3,
        version: "0.1.0".to_owned(),
        log: String::new(),
    }
}

/// A staged release B: the stage's own work is the installer's (its tests); here it marks
/// B staged in the state, as `release::stage` does.
fn stage_b(
    layout: Layout,
) -> impl FnMut(&mut ScratchHost, &std::path::Path) -> Result<Staged, Refusal> {
    move |_, _| {
        let dir = layout.join(&kbf_installer::layout::release_dir(&c(B)));
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        let lock = Locked::acquire(&layout).expect("lock");
        let mut state = lock.read().expect("reads").unwrap_or_default();
        state.staged = Some(c(B));
        lock.write(&state).expect("writes");
        Ok(staged(B))
    }
}

/// Catches an offer staged more than once: a refused entry would otherwise be fetched,
/// verified and refused again every few seconds, and an accepted one re-installed.
///
/// Mutant seen red: `handled_entry` not recorded (the stager runs on every pass).
#[test]
fn the_newest_entry_is_staged_once_and_a_refusal_is_reported_not_retried() {
    let mut rig = Rig::new("stage-once");
    let older = rig.put_offer(1, A);
    let newest = rig.put_offer(2, B);
    assert_ne!(older, newest);
    let mut calls = Vec::new();
    let mut inner = stage_b(rig.layout.clone());
    let mut stager = |exec: &mut ScratchHost, entry: &std::path::Path| {
        calls.push(
            entry
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
        );
        inner(exec, entry)
    };
    rig.helper.cycle(T0, &mut stager).expect("pass");
    rig.helper.cycle(T0 + 3, &mut stager).expect("pass");
    assert_eq!(
        calls,
        std::slice::from_ref(&newest),
        "only the newest, and only once"
    );
    assert_eq!(rig.status()["state"], "staged");
    assert_eq!(rig.status()["staged_commit"], B);
    assert_eq!(rig.status()["staged"]["protocol"], 7);

    let mut refuse = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
        Err(Refusal::Inbox {
            name: "offer.json".to_owned(),
            fault: InboxFault::Missing,
        })
    };
    let newer = format!("{:020}-{}", 3, "d".repeat(40));
    fs::create_dir(rig.root.join("var/lib/kbf-updater/inbox").join(&newer)).expect("entry");
    rig.helper.cycle(T0 + 6, &mut refuse).expect("pass");
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("inbox")
    );
    // The same entry again: not looked at (a refuse that panicked would show it).
    let mut again = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
        panic!("a refused entry is not staged again")
    };
    rig.helper.cycle(T0 + 9, &mut again).expect("pass");
}

/// Catches the swap being skipped, the restart being skipped or repeated, and an order
/// acted on twice: the activate order moves the links, restarts the unit once, reports a
/// swap on trial, and the same order file put back (a replay) does nothing.
///
/// Mutants seen red: no restart step; the acknowledged id not recorded (the replay
/// restarts again); the order file not removed.
#[test]
fn an_activate_order_swaps_restarts_once_and_is_not_replayed() {
    let mut rig = Rig::new("activate");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    let links = |rig: &Rig, name: &str| {
        fs::read_link(rig.root.join("usr/local/kbf").join(name)).expect("link")
    };
    assert_eq!(
        links(&rig, "current"),
        PathBuf::from(format!("releases/{B}"))
    );
    assert_eq!(
        links(&rig, "previous"),
        PathBuf::from(format!("releases/{A}"))
    );
    assert_eq!(rig.restarts(), 1);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert_eq!(rig.status()["installed_commit"], B);
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert!(
        !rig.orders().join(format!("{:020}.json", 5)).exists(),
        "taken"
    );
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.restarts(), 1, "a replayed order restarts nothing");
    assert!(
        !rig.orders().join(format!("{:020}.json", 5)).exists(),
        "removed unread"
    );
}

/// Catches a confirm that does not keep the swap, and one that names the wrong swap: the
/// confirm order carries the commit, the helper finds the activation's own id.
///
/// Mutant seen red: the confirm passing the confirm order's id instead of the pending
/// swap's (NotPending-style refusal).
#[test]
fn a_confirm_order_keeps_the_swap_and_a_revert_order_goes_back() {
    let mut rig = Rig::new("confirm");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    rig.put_order(6, &order_json(6, "confirm", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.status()["state"], "confirmed");
    assert_eq!(rig.restarts(), 1);

    rig.put_order(7, &order_json(7, "revert", A));
    rig.cycle(T0 + 9);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.restarts(), 2, "a revert restarts the unit");
    assert_eq!(rig.status()["state"], "reverted");
}

/// Catches a swap left on trial forever: the Linux boot check starts the window at the
/// first start; the pass after the window reverts and restarts the unit.
///
/// Mutant seen red: `expire` not called in the pass.
#[test]
fn a_swap_not_confirmed_in_its_window_is_reverted() {
    let mut rig = Rig::new("expire");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    // The first start of B (the unit's ExecStartPre) starts the window.
    let layout = rig.layout.clone();
    let mut out = Vec::new();
    let decision = activate::boot_check(rig.helper.exec_mut(), &layout, T0 + 10, 600, &mut out)
        .expect("checks");
    assert!(matches!(decision, activate::Boot::Start { .. }));
    rig.cycle(T0 + 500);
    assert_eq!(rig.state().current, Some(c(B)), "inside the window");
    rig.cycle(T0 + 10 + 601);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.status()["state"], "reverted");
    assert_eq!(rig.restarts(), 2);
}

/// Catches a restart lost on the way: when the restart fails, the owed restart stays in
/// `helper.json` and the next pass retries, and the node is told why in the meantime.
///
/// Mutant seen red: `restart_due` cleared before the restart is tried.
#[test]
fn a_failed_restart_is_owed_until_it_succeeds() {
    let mut rig = Rig::new("restart-owed");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    // Fail the first restart only.
    let tries = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = tries.clone();
    rig.helper_exec_mut().answer = Box::new(move |_| {
        seen.set(seen.get() + 1);
        if seen.get() == 1 {
            Ok(reply(1, "", "unit not found"))
        } else {
            Ok(reply(0, "", ""))
        }
    });
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("restart failed")
    );
    rig.cycle(T0 + 6);
    assert_eq!(tries.get(), 2, "retried on the next pass, once");
    rig.cycle(T0 + 9);
    assert_eq!(tries.get(), 2, "and not again");
}

impl Rig {
    fn helper_exec_mut(&mut self) -> &mut ScratchHost {
        self.helper.exec_mut()
    }
}

/// Catches a floor moved by garbage: a file that is not an order is refused, removed and
/// does not acknowledge anything, so the real order after it (a lower id) still runs. A
/// link in the orders directory is removed as a link; what it points at is untouched.
///
/// Mutants seen red: acknowledging an unparsable file's id (the real order is then
/// ignored as old); removing through the link's target (the secret goes).
#[test]
fn a_file_that_is_not_an_order_moves_nothing() {
    let mut rig = Rig::new("garbage");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    let secret = rig.root.join("secret");
    fs::write(&secret, b"not yours").expect("secret");
    rig.put_order(900, "not json");
    symlink(&secret, rig.orders().join(format!("{:020}.json", 901))).expect("link");
    rig.put_order(902, &order_json(903, "activate", B));
    rig.put_order(903, &order_json(903, "reboot", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.status()["acked_order_id"], 0);
    assert_eq!(fs::read(&secret).expect("kept"), b"not yours");
    assert_eq!(
        fs::read_dir(rig.orders()).expect("dir").count(),
        0,
        "all removed"
    );
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.restarts(), 1);
}

/// Catches the node never reading the helper: what the helper writes is what `kbf-update`
/// reads, in every state the helper reports.
#[test]
fn status_json_is_read_by_the_node_client() {
    let mut rig = Rig::new("status-contract");
    let status = rig.root.join("var/lib/kbf-updater/status.json");
    let read = || kbf_update::helper::read(&status);
    rig.cycle(T0);
    let kbf_update::helper::Helper::Reported(idle) = read() else {
        panic!("reported")
    };
    assert_eq!(idle.state, kbf_update::helper::HelperState::Idle);
    assert_eq!(idle.installed.as_deref(), Some(A));
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0 + 3, &mut stager).expect("stage");
    let kbf_update::helper::Helper::Reported(staged) = read() else {
        panic!("reported")
    };
    assert_eq!(staged.state, kbf_update::helper::HelperState::Staged);
    assert_eq!(staged.staged.as_deref(), Some(B));
    assert_eq!(staged.staged_facts.expect("facts").protocol, 7);
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    let kbf_update::helper::Helper::Reported(on_trial) = read() else {
        panic!("reported")
    };
    assert_eq!(
        on_trial.state,
        kbf_update::helper::HelperState::ActivatedUnconfirmed
    );
    assert_eq!(on_trial.acked_order_id, 5);
    assert!(on_trial.holds(B) && on_trial.holds(A));
}

/// Catches a restart that runs the wrong program or the wrong unit: argv is fixed by the
/// helper's own flag, an absolute program, and nothing else (a unit name with a space or
/// an option-looking prefix stays one argument); a host with no restart runs nothing.
///
/// Mutant seen red: the step returned as `None` (also `a_failed_restart_is_owed...`).
#[test]
fn the_restart_is_one_fixed_command() {
    let program = |r: Restart| match r.step() {
        Some(Step::Run(cmd)) => (cmd.program.as_str().to_owned(), cmd.args),
        other => panic!("not a command: {other:?}"),
    };
    assert_eq!(
        program(Restart::Systemd("kbf.service".to_owned())),
        (
            "/usr/bin/systemctl".to_owned(),
            vec![
                "restart".to_owned(),
                "--".to_owned(),
                "kbf.service".to_owned()
            ]
        )
    );
    assert_eq!(
        program(Restart::Launchd("com.komira.kbf-daemon".to_owned())),
        (
            "/bin/launchctl".to_owned(),
            vec![
                "kickstart".to_owned(),
                "-k".to_owned(),
                "system/com.komira.kbf-daemon".to_owned()
            ]
        )
    );
    assert!(Restart::Off.step().is_none());
}

/// Catches a confirm window the order chooses: on macOS the window starts at the swap and
/// the helper clamps the order's `window_s` to its own 120..=1800 seconds, so neither a
/// tiny window (reverts a healthy update) nor a year (never reverts a bad one) gets
/// through.
///
/// Mutants seen red: the clamp dropped; the bounds swapped.
#[test]
fn the_window_an_order_asks_for_is_clamped_on_macos() {
    for (asked, kept) in [(5, 120), (600, 600), (99_999, 1800)] {
        let mut rig = Rig::on(&format!("window-{asked}"), Platform::MacOs);
        rig.put_offer(1, B);
        let mut stager = stage_b(rig.layout.clone());
        rig.helper.cycle(T0, &mut stager).expect("stage");
        let body = order_json(5, "activate", B).replace("600", &asked.to_string());
        rig.put_order(5, &body);
        rig.cycle(T0 + 3);
        let pending = rig.state().pending_confirm.expect("on trial");
        assert_eq!(pending.deadline, Some(T0 + 3 + kept), "asked {asked}");
    }
}

/// Catches orders taken in the order the directory lists them: three orders in one pass
/// (activate, confirm, revert) end on the old release only if run by id.
///
/// Mutant seen red: the sort dropped (with the files made in reverse id order).
#[test]
fn orders_are_acted_on_in_id_order() {
    let mut rig = Rig::new("order-sort");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(7, &order_json(7, "revert", A));
    rig.put_order(6, &order_json(6, "confirm", B));
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.status()["acked_order_id"], 7);
    assert_eq!(
        rig.restarts(),
        1,
        "one restart for the pass, onto where it ended"
    );
}

/// Catches an order id that wedges the helper: one far above the last acknowledged id is
/// refused and removed without being acknowledged, so the real orders after it still run.
///
/// Mutant seen red: the jump check dropped (u64::MAX is acknowledged; the real order is
/// then below the floor).
#[test]
fn an_order_id_far_ahead_does_not_move_the_floor() {
    let mut rig = Rig::new("order-jump");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(
        crate::order::MAX_JUMP + 1,
        &order_json(crate::order::MAX_JUMP + 1, "revert", A),
    );
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("too far ahead")
    );
    assert_eq!(rig.status()["acked_order_id"], 0);
    assert_eq!(fs::read_dir(rig.orders()).expect("dir").count(), 0);
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(B)));
}

/// Catches helper.json readable by the service user, and status.json not (the node reads
/// it as that user). The modes are set by the write itself, not by the umask of whoever
/// runs the helper.
///
/// Mutant seen red: either mode swapped.
#[test]
fn the_helpers_files_have_their_modes() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("modes");
    let mode = |p: &str| {
        fs::metadata(rig.root.join(p))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("var/lib/kbf-deploy/helper.json"), 0o600);
    assert_eq!(mode("var/lib/kbf-updater/status.json"), 0o644);
}

/// Catches staged facts shown for a release that is not the staged one (an old fact kept
/// after the staged slot was cleared or replaced): only the release the state says is
/// staged brings its protocol numbers.
///
/// Mutant seen red: the filter dropped.
#[test]
fn staged_facts_are_shown_only_for_the_staged_release() {
    use crate::memory::{Memory, StagedFacts};
    let memory = Memory {
        staged: Some(StagedFacts {
            commit: c(B),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
        }),
        ..Memory::default()
    };
    let render = |staged: Option<&str>| {
        let state = State {
            current: Some(c(A)),
            staged: staged.map(c),
            ..State::default()
        };
        let json: serde_json::Value =
            serde_json::from_slice(&crate::status::render(&state, &memory)).expect("json");
        json
    };
    assert_eq!(render(Some(B))["staged"]["protocol"], 7);
    assert!(render(None)["staged"].is_null());
    assert!(render(Some(A))["staged"].is_null());
}

/// Catches an order file accepted in a shape the client never writes: an extra field, a
/// missing one, a name that is not twenty digits and `.json` (a shorter or longer number,
/// a letter, another suffix), or an id the file's name does not carry.
///
/// Mutants seen red: `deny_unknown_fields` dropped; the 20-digit length check dropped.
#[test]
fn only_the_clients_shape_of_order_is_read() {
    use crate::order::{id_of, parse};
    let ok = order_json(5, "activate", B);
    assert!(parse(5, ok.as_bytes()).is_ok());
    let extra = ok.replace('}', ",\"path\":\"/etc\"}");
    assert!(parse(5, extra.as_bytes()).is_err());
    assert!(
        parse(
            5,
            br#"{"order_id":5,"epoch":1,"verb":"activate","commit":"x"}"#
        )
        .is_err()
    );
    assert!(parse(6, ok.as_bytes()).is_err(), "named for another id");
    assert_eq!(id_of("00000000000000000005.json"), Some(5));
    for bad in [
        "5.json",
        "000000000000000000005.json",
        "0000000000000000005.json",
        "0000000000000000000a.json",
        "00000000000000000005.txt",
        "00000000000000000005",
        "99999999999999999999.json",
    ] {
        assert_eq!(id_of(bad), None, "{bad}");
    }
}

/// A stager that must not run.
fn no_stage() -> impl FnMut(&mut ScratchHost, &std::path::Path) -> Result<Staged, Refusal> {
    |_, _| panic!("nothing is to be staged")
}

fn inbox_of(rig: &Rig) -> PathBuf {
    rig.root.join("var/lib/kbf-updater/inbox")
}

/// Catches a helper that dies, or acts, on an inbox it cannot make sense of: no inbox, an
/// inbox without orders, an inbox the service user stuffed with entries (more than the
/// installer reads), an entry for the release already running, an entry whose name is
/// not `<generation>-<commit>`. Each is quietly skipped or refused with a reason; none is
/// staged.
///
/// Mutants seen red: the entry-count refusal turned into a panic; the already-running
/// entry staged.
#[test]
fn an_inbox_that_is_missing_stuffed_or_for_the_running_release_stages_nothing() {
    let mut rig = Rig::new("odd-inbox");
    let inbox = inbox_of(&rig);
    // An entry for the running release, and names that are not offers.
    fs::create_dir(inbox.join(format!("{:020}-{A}", 3))).expect("entry");
    fs::create_dir(inbox.join(format!("short-{B}"))).expect("entry");
    rig.helper.cycle(T0, &mut no_stage()).expect("pass");
    assert_eq!(rig.status()["state"], "idle");
    // More entries than the installer reads.
    for n in 0..20 {
        fs::create_dir(inbox.join(format!("junk{n}"))).expect("entry");
    }
    rig.helper.cycle(T0 + 3, &mut no_stage()).expect("pass");
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("inbox")
    );
    // No orders directory, then no inbox at all.
    fs::remove_dir_all(inbox.join("orders")).expect("remove");
    rig.helper.cycle(T0 + 6, &mut no_stage()).expect("pass");
    fs::remove_dir_all(&inbox).expect("remove");
    rig.helper.cycle(T0 + 9, &mut no_stage()).expect("pass");
}

/// Catches the same for orders: a directory stuffed past the installer's file limit is
/// refused as a whole (and says so), and an order file that is a directory is neither
/// read nor removed through, and is reported.
#[test]
fn an_orders_directory_that_is_stuffed_or_holds_a_directory_is_refused() {
    let mut rig = Rig::new("odd-orders");
    fs::create_dir(rig.orders().join(format!("{:020}.json", 904))).expect("dir");
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.orders().join(format!("{:020}.json", 904)).is_dir(),
        "left alone"
    );
    for n in 0..20 {
        fs::write(rig.orders().join(format!("{:020}.json", 100 + n)), "x").expect("file");
    }
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("inbox")
    );
}

/// Catches the helper's own files being wrong ending the run in silence or in a guess:
/// a state.json that cannot be read is a refusal the node can read (at start), a
/// helper.json that cannot be read stops the pass with an error.
#[test]
fn unreadable_state_is_reported_and_unreadable_memory_stops_the_pass() {
    let mut rig = Rig::new("unreadable");
    // Links already as the state says: nothing moved, nothing owed.
    rig.helper.recover().expect("recovers");
    fs::write(rig.root.join("var/lib/kbf-deploy/state.json"), b"not json").expect("ruin");
    rig.helper.recover().expect("recovers into a refusal");
    let memory =
        fs::read_to_string(rig.root.join("var/lib/kbf-deploy/helper.json")).expect("memory");
    assert!(memory.contains("refused"), "{memory}");
    let path = rig.root.join("var/lib/kbf-deploy/helper.json");
    fs::write(&path, b"not json").expect("ruin");
    let error = rig
        .helper
        .cycle(T0, &mut no_stage())
        .expect_err("cannot parse it");
    assert!(error.to_string().contains("helper.json"), "{error}");
    fs::remove_file(&path).expect("remove");
    fs::create_dir(&path).expect("a directory where the file belongs");
    let error = rig
        .helper
        .cycle(T0, &mut no_stage())
        .expect_err("cannot read it");
    assert!(
        error.to_string().contains("Is a directory") || error.to_string().contains("directory"),
        "{error}"
    );
}

/// Catches orders that claim more than they can: an activate of the release already
/// running changes nothing and restarts nothing; one for a release that is not installed
/// is refused; a confirm with nothing on trial is refused.
#[test]
fn orders_the_host_cannot_carry_out_are_refused_and_change_nothing() {
    let mut rig = Rig::new("cannot");
    rig.put_order(5, &order_json(5, "activate", A));
    rig.cycle(T0);
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.status()["acked_order_id"], 5);
    rig.put_order(6, &order_json(6, "activate", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.state().current, Some(c(A)));
    rig.put_order(7, &order_json(7, "confirm", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.restarts(), 0);
}

/// Catches a revert that cannot happen being hidden: the swap's window runs out but the
/// release to go back to is gone; the helper keeps the swap, says why in status.json, and
/// does not restart onto nothing.
#[test]
fn an_expiry_that_cannot_revert_is_reported() {
    let mut rig = Rig::new("expire-fails");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    let layout = rig.layout.clone();
    activate::boot_check(
        rig.helper.exec_mut(),
        &layout,
        T0 + 10,
        600,
        &mut Vec::new(),
    )
    .expect("starts");
    fs::remove_file(rig.root.join("usr/local/kbf/releases").join(A).join("kbf")).expect("gone");
    let before = rig.restarts();
    rig.cycle(T0 + 10 + 601);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(!rig.status()["refusal"].as_str().expect("text").is_empty());
    assert_eq!(rig.restarts(), before);
}

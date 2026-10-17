//! The helper killed at every step of a pass, then restarted: the links, the state and the
//! restarts must agree afterwards, for an activate, a revert and an expiry. (Adopted from
//! the reviewer's probe of #929: it was red at revert steps 4 and 5 and expiry steps 4 and 5
//! before the restart was owed ahead of the verb.)
use std::cell::{Cell, RefCell};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use kbf_host::exec::Exec;
use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::{Cmd, HostPath, Root, Step};
use kbf_installer::activate;
use kbf_installer::layout::Layout;
use kbf_installer::state::{Locked, State};
use kbf_installer::testkit::{A, B, c};

use crate::cycle::{Config, Helper};
use crate::restart::Restart;

const UNIT: &str = "kbf.service";
const T0: u64 = 1_800_000_000;

struct Crashy {
    inner: Rc<RefCell<ScratchHost>>,
    counter: Rc<Cell<usize>>,
    crash_at: Rc<Cell<usize>>, // 0 = never; odd = before step, even = after
}

impl Exec for Crashy {
    fn euid(&self) -> u32 {
        self.inner.borrow().euid()
    }
    fn inspect(&self, p: &HostPath) -> std::io::Result<Option<kbf_host::Entry>> {
        self.inner.borrow().inspect(p)
    }
    fn stat(&self, p: &HostPath) -> std::io::Result<Option<kbf_host::Meta>> {
        self.inner.borrow().stat(p)
    }
    fn query(&self, c: &Cmd) -> std::io::Result<std::process::Output> {
        self.inner.borrow().query(c)
    }
    fn http_status(&self, u: &kbf_host::HttpUrl) -> std::io::Result<u16> {
        self.inner.borrow().http_status(u)
    }
    fn apply(&mut self, s: &Step) -> std::io::Result<()> {
        let n = self.counter.get() + 1;
        self.counter.set(n);
        let at = self.crash_at.get();
        if at != 0 && n == at.div_ceil(2) && !at.is_multiple_of(2) {
            panic!("killed before step {n}");
        }
        let r = self.inner.borrow_mut().apply(s);
        if at != 0 && n == at / 2 && at.is_multiple_of(2) {
            panic!("killed after step {n}");
        }
        r
    }
    fn check_binary(&self) -> Result<(), kbf_host::trust::Untrusted> {
        Ok(())
    }
}

struct World {
    host: Rc<RefCell<ScratchHost>>,
    root: PathBuf,
    layout: Layout,
    counter: Rc<Cell<usize>>,
    crash_at: Rc<Cell<usize>>,
}

impl World {
    fn new(name: &str) -> World {
        let mut host = ScratchHost::new(&format!("probe-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/lib/kbf-deploy",
            "var/lib/kbf-updater/inbox/orders",
            "usr/local/kbf/releases",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for r in [A, B] {
            let d = root.join("usr/local/kbf/releases").join(r);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("kbf"), b"binary").unwrap();
        }
        host.answer = Box::new(|_| Ok(reply(0, "", "")));
        let layout = Layout::linux(Root::new(&root).unwrap());
        Locked::acquire(&layout)
            .unwrap()
            .write(&State {
                current: Some(c(A)),
                staged: Some(c(B)),
                ..State::default()
            })
            .unwrap();
        World {
            host: Rc::new(RefCell::new(host)),
            root,
            layout,
            counter: Rc::new(Cell::new(0)),
            crash_at: Rc::new(Cell::new(0)),
        }
    }
    fn helper(&self) -> Helper<Crashy> {
        let cfg = Config {
            update_dir: HostPath::new("/var/lib/kbf-updater").unwrap(),
            restart: Restart::Systemd(UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        };
        Helper::new(
            self.layout.clone(),
            Crashy {
                inner: self.host.clone(),
                counter: self.counter.clone(),
                crash_at: self.crash_at.clone(),
            },
            cfg,
        )
    }
    fn order(&self, id: u64, verb: &str, commit: &str) {
        fs::write(self.root.join("var/lib/kbf-updater/inbox/orders").join(format!("{id:020}.json")),
            format!("{{\"order_id\":{id},\"epoch\":1,\"verb\":\"{verb}\",\"commit\":\"{commit}\",\"window_s\":600}}")).unwrap();
    }
    fn restarts(&self) -> usize {
        self.host
            .borrow()
            .log
            .iter()
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", "--", UNIT]))
            .count()
    }
    fn link(&self) -> String {
        fs::read_link(self.root.join("usr/local/kbf/current"))
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }
    fn state(&self) -> State {
        kbf_installer::state::read_unlocked(&self.layout)
            .unwrap()
            .unwrap()
    }
    fn orders_left(&self) -> usize {
        fs::read_dir(self.root.join("var/lib/kbf-updater/inbox/orders"))
            .unwrap()
            .count()
    }
}

fn pass(w: &World, now: u64) {
    let mut h = w.helper();
    let mut never = |_: &mut Crashy,
                     _: &Path|
     -> Result<kbf_installer::stage::Staged, kbf_installer::refusal::Refusal> {
        panic!("no offer")
    };
    h.recover().unwrap();
    h.cycle(now, &mut never).unwrap();
}

fn crash_pass(w: &World, now: u64, at: usize) -> bool {
    w.counter.set(0);
    w.crash_at.set(at);
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut h = w.helper();
        let mut never = |_: &mut Crashy,
                         _: &Path|
         -> Result<
            kbf_installer::stage::Staged,
            kbf_installer::refusal::Refusal,
        > { panic!("no offer") };
        h.cycle(now, &mut never).unwrap();
    }));
    w.crash_at.set(0);
    r.is_err()
}

fn steps_in_clean_pass(scn: &str) -> usize {
    let w = prep(scn, "count");
    w.counter.set(0);
    pass(&w, T0 + 700);
    w.counter.get()
}

fn prep(scn: &str, tag: &str) -> World {
    let w = World::new(&format!("{scn}-{tag}"));
    pass(&w, T0 - 100);
    match scn {
        "activate" => {
            w.order(5, "activate", B);
        }
        "revert" => {
            w.order(5, "activate", B);
            pass(&w, T0);
            w.order(6, "confirm", B);
            pass(&w, T0 + 3);
            w.order(7, "revert", A);
        }
        "expire" => {
            w.order(5, "activate", B);
            pass(&w, T0);
            let mut out = Vec::new();
            let mut ex = Crashy {
                inner: w.host.clone(),
                counter: w.counter.clone(),
                crash_at: w.crash_at.clone(),
            };
            activate::boot_check(&mut ex, &w.layout, T0 + 10, 600, &mut out).unwrap();
        }
        _ => unreachable!(),
    }
    w
}

/// Catches a restart lost by a kill between the links moving and the pass's next save.
fn kill_at_every_step_of(scn: &str) {
    let mut bad = Vec::new();
    {
        let n = steps_in_clean_pass(scn);
        eprintln!("{scn}: {n} steps in a clean pass");
        for at in 1..=(2 * n) {
            let w = prep(scn, &format!("k{at}"));
            let before = w.restarts();
            let died = crash_pass(&w, T0 + 700, at);
            for i in 0..3 {
                pass(&w, T0 + 800 + i * 3);
            }
            let st = w.state();
            let link = w.link();
            let cur = st
                .current
                .as_ref()
                .map(|x| x.as_str().to_owned())
                .unwrap_or_default();
            let restarts = w.restarts() - before;
            let mut why = Vec::new();
            if !link.ends_with(&cur) {
                why.push(format!("link {link} != state {cur}"));
            }
            let moved = (scn == "activate" && cur == B) || (scn != "activate" && cur == A);
            if moved && restarts == 0 {
                why.push("links moved, service never restarted".to_owned());
            }
            if restarts > 2 {
                why.push(format!("{restarts} restarts"));
            }
            if w.orders_left() != 0 {
                why.push("order file left".to_owned());
            }
            if !why.is_empty() {
                bad.push(format!("{scn} step {at} (died {died}): {}", why.join("; ")));
            }
        }
    }
    for b in &bad {
        eprintln!("BAD {b}");
    }
    assert!(bad.is_empty(), "{} bad", bad.len());
}

#[test]
fn kill_at_every_step_of_an_activate() {
    kill_at_every_step_of("activate");
}

#[test]
fn kill_at_every_step_of_a_revert() {
    kill_at_every_step_of("revert");
}

#[test]
fn kill_at_every_step_of_an_expiry() {
    kill_at_every_step_of("expire");
}

/// Catches orders read through a linked `inbox`: the service user owns that directory and
/// could point it at one of its own anywhere. Mutant seen red: `orders` opened by its full
/// path (the parent followed): the helper acted on the order and removed the file.
#[test]
fn inbox_replaced_by_a_link_is_not_followed_for_orders() {
    let w = World::new("inboxlink");
    pass(&w, T0 - 100);
    let base = w.root.join("var/lib/kbf-updater");
    let elsewhere = w.root.join("var/lib/elsewhere");
    fs::create_dir_all(elsewhere.join("orders")).unwrap();
    fs::remove_dir_all(base.join("inbox")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("inbox")).unwrap();
    let f = elsewhere.join("orders").join(format!("{:020}.json", 5));
    fs::write(&f, format!("{{\"order_id\":5,\"epoch\":1,\"verb\":\"activate\",\"commit\":\"{B}\",\"window_s\":600}}")).unwrap();
    pass(&w, T0);
    eprintln!("current={} order file exists={}", w.link(), f.exists());
    assert!(
        w.link().ends_with(A),
        "helper acted on an order through a linked inbox"
    );
    assert!(f.exists(), "helper unlinked a file through a linked inbox");
}

//! The faults a seed's swarm and scenario inject: deaths and returns, mass
//! reconnects and report waves, reboots, daemon restarts, partitions, node report
//! changes, and F4.4's maintenance.

use super::*;

impl World {
    pub(super) fn faults(&mut self) {
        let t = self.t;
        if self.scenario == Scenario::Churn && t > 0 && t.is_multiple_of(60) {
            let mut pick: Vec<usize> = (0..self.nodes.len()).collect();
            self.rng.shuffle(&mut pick);
            pick.truncate(self.nodes.len().div_ceil(20));
            for i in pick {
                match self.nodes[i].link {
                    Link::Up => {
                        self.kill_runs(i);
                        self.nodes[i].link = Link::Down {
                            until: u64::MAX,
                            keeps_runs: false,
                        };
                        self.check.hit("worker died");
                    }
                    Link::Down {
                        until: u64::MAX, ..
                    } => {
                        self.nodes[i].link = Link::Down {
                            until: t,
                            keeps_runs: false,
                        };
                        self.check.hit("worker returned");
                    }
                    _ => {}
                }
            }
        }
        if self.scenario == Scenario::MassReconnect && t == self.maintenance.1 {
            // A fleet-wide change of node reports (an Xcode build rolled out to every
            // Mac): every node resends `Hello` on its stream, which opens no session.
            self.check.hit("report wave");
            for i in 0..self.nodes.len() {
                if self.nodes[i].link == Link::Up {
                    self.change_report(i, false);
                }
            }
        }
        if self.scenario == Scenario::MassReconnect && self.mass_at.contains(&t) {
            self.check.hit("mass reconnect");
            // Every node comes back in the same second: most of them rebooted.
            let until = t + self.rng.between(1, 4);
            let restarts = Chance::percent(u32::try_from(self.rng.below(50)).expect("small"));
            for i in 0..self.nodes.len() {
                if self.nodes[i].link == Link::Up {
                    let keeps_runs = self.rng.chance(restarts);
                    if !keeps_runs {
                        self.kill_runs(i);
                    }
                    self.nodes[i].link = Link::Down { until, keeps_runs };
                }
            }
        }
        if self.scenario == Scenario::OperatorStorm {
            self.maintain();
        }
        if self.rng.chance(self.swarm.reboot)
            && let Some(i) = self.up_node()
        {
            self.kill_runs(i);
            let until = t + self.rng.between(1, 20);
            self.nodes[i].link = Link::Down {
                until,
                keeps_runs: false,
            };
        }
        if self.rng.chance(self.swarm.restart)
            && let Some(i) = self.up_node()
        {
            let until = t + self.rng.between(1, 10);
            self.nodes[i].link = Link::Down {
                until,
                keeps_runs: true,
            };
        }
        if self.rng.chance(self.swarm.partition)
            && let Some(i) = self.up_node()
        {
            let until = t + self.rng.between(1, 2 * GRACE_S);
            self.nodes[i].link = Link::Cut { from: t, until };
        }
        if self.rng.chance(self.swarm.capacity)
            && let Some(i) = self.up_node()
        {
            self.change_report(i, true);
        }
    }

    /// F4.4's maintenance. The operator drains the release pool and four other
    /// nodes, and then they all go offline for longer than G and the unservable wait
    /// together. Work for the release pool waits for the cordon first, then for no
    /// live worker, and is refused a full wait after the pool stopped being live.
    /// The operator ends the maintenance of the other four while they are still
    /// offline, ten seconds before the scheduler gives the first of them up, and the
    /// control log stalls for a few seconds across that moment: work is granted to
    /// the silent nodes, given up at G and granted again (to one given up later, or
    /// elsewhere), and the first grant commits after the second (superseded grants,
    /// I2).
    pub(super) fn maintain(&mut self) {
        let (drain_at, offline_at) = self.maintenance;
        let t = self.t;
        let down: Vec<usize> = self.storm.iter().take(2 * RELEASE_NODES).copied().collect();
        if t == drain_at {
            self.check.hit("pool maintenance");
            for &i in &down {
                let deadline = FarmTime::from_millis((t + self.rng.between(5, 60)) * 1_000);
                let worker = self.nodes[i].name.clone();
                self.input(Event::Drain { worker, deadline });
            }
        }
        if t == offline_at {
            let until = t + GRACE_S + WAIT_S + self.rng.between(10, 60);
            let mut heard = None;
            for (k, &i) in down.iter().enumerate() {
                if self.nodes[i].link == Link::Up {
                    self.nodes[i].link = Link::Cut { from: t, until };
                    if k >= RELEASE_NODES {
                        let at = self.nodes[i].last_ack;
                        heard = Some(heard.map_or(at, |h: u64| h.min(at)));
                    }
                }
            }
            self.early_uncordon_at = heard.map(|h| h + GRACE_S - 10);
        }
        if self.early_uncordon_at == Some(t) {
            self.check.hit("nodes uncordoned while silent");
            self.stall = (t + 9, t + 9 + self.rng.between(4, 8));
            for &i in &down[RELEASE_NODES..] {
                let worker = self.nodes[i].name.clone();
                self.input(Event::Uncordon { worker });
            }
        }
    }

    /// Node `i` resends `Hello` on its stream because its node report changed: a Mac
    /// with an Xcode build added or removed.
    /// With `rescale`, its capacity also changes.
    pub(super) fn change_report(&mut self, i: usize, rescale: bool) {
        let percent = self.rng.between(25, 150);
        let n = &mut self.nodes[i];
        if rescale {
            let cpu = (n.base.cpu_millis * percent / 100).max(1_000);
            let mem = (n.base.memory_bytes * percent / 100).max(GIB);
            n.capacity = Resources::new(cpu, mem).with_gpus(n.base.gpus);
        }
        if n.report.iter().any(|(k, v)| *k == "os" && v == "macos") {
            let had = n.report.iter().any(|(k, v)| *k == "xcode" && v == "15F31d");
            if had {
                n.report.retain(|(k, v)| !(*k == "xcode" && v == "15F31d"));
            } else {
                n.report.push(("xcode", "15F31d".to_owned()));
            }
            n.caps = caps_of(&n.report);
        }
        n.last_ack = self.t;
        let event = Event::Capacity {
            worker: n.name.clone(),
            capacity: n.capacity,
            caps: n.caps.clone(),
        };
        self.input(event);
    }

    pub(super) fn up_node(&mut self) -> Option<usize> {
        let i = usize::try_from(self.rng.below(self.nodes.len() as u64)).expect("small");
        (self.nodes[i].link == Link::Up).then_some(i)
    }

    /// Ends every run on node `i`: the machine went away.
    pub(super) fn kill_runs(&mut self, i: usize) {
        let runs = std::mem::take(&mut self.nodes[i].runs);
        for run in runs.values() {
            self.check.run_ends(run.op, run.fence);
        }
        self.nodes[i].unacked.clear();
    }
}

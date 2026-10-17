//! How the helper restarts the one service it was configured for. The unit or label is
//! the helper's own flag (root's), never anything an order carries.

use kbf_host::{Cmd, HostPath, Step};

/// What to run to restart the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// `systemctl restart <unit>`.
    Systemd(String),
    /// `launchctl kickstart -k system/<label>`.
    Launchd(String),
    /// Nothing: a host whose service is restarted by other means (or a test).
    Off,
}

impl Restart {
    /// The step that restarts the service, if there is one.
    pub fn step(&self) -> Option<Step> {
        let (program, args): (&str, Vec<String>) = match self {
            Restart::Systemd(unit) => (
                "/usr/bin/systemctl",
                vec!["restart".to_owned(), "--".to_owned(), unit.clone()],
            ),
            Restart::Launchd(label) => (
                "/bin/launchctl",
                vec![
                    "kickstart".to_owned(),
                    "-k".to_owned(),
                    format!("system/{label}"),
                ],
            ),
            Restart::Off => return None,
        };
        let program = HostPath::new(program).expect("an absolute path");
        let cmd = args.into_iter().fold(Cmd::new(program), Cmd::arg);
        Some(Step::Run(cmd))
    }
}

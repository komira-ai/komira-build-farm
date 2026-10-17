//! The table of modes (`kbf_cli::names`) against everything that spells a mode as text.

use kbf_cli::names::{self, MODES};

/// Catches a CLI whose modes are not exactly the table's (a renamed constant with a
/// literal left behind, or a mode added to one and not the other), and a constant
/// that is not in the table.
#[test]
fn the_cli_has_exactly_the_tables_modes() {
    let cmd = kbf_cli::command();
    let have: Vec<&str> = cmd.get_subcommands().map(clap::Command::get_name).collect();
    let want: Vec<&str> = MODES.iter().map(|m| m.name).collect();
    assert_eq!(have, want);
    let constants = [
        names::FRONTEND,
        names::STORAGE,
        names::DAEMON,
        names::STORAGE_AND_DAEMON,
        names::ALL_IN_ONE,
    ];
    assert_eq!(constants.to_vec(), want);
}

/// Catches a mode whose help says it is not built, though every mode runs.
#[test]
fn no_modes_help_says_it_is_not_built() {
    let cmd = kbf_cli::command();
    for mode in MODES {
        let about = cmd
            .find_subcommand(mode.name)
            .expect("mode")
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(!about.contains("Not built"), "{}: {about}", mode.name);
    }
}

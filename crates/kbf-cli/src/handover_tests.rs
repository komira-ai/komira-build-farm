//! `kbf frontend handover`'s decision and defaults; the command against a fake
//! `systemctl` is `tests/frontend_handover.rs`.

use super::*;

fn states(a: State, b: State) -> [(String, State); 2] {
    [(unit("a"), a), (unit("b"), b)]
}

/// Catches a decision that hands over from the wrong instance, starts the second instance
/// with none up, or starts anything when both serve or one is changing; the states are
/// systemd's `ActiveState` words.
#[test]
fn the_states_of_the_instances_decide_what_is_started() {
    use State::{Active, Changing, Stopped};
    let handover = |old: &str, new: &str| {
        Ok(Step::Handover {
            old: unit(old),
            new: unit(new),
        })
    };
    assert_eq!(decide(&states(Active, Stopped)), handover("a", "b"));
    assert_eq!(decide(&states(Stopped, Active)), handover("b", "a"));
    assert_eq!(
        decide(&states(Stopped, Stopped)),
        Ok(Step::Start { new: unit("a") })
    );
    assert_eq!(
        decide(&states(Active, Active)),
        Err(Refusal::Draining {
            a: unit("a"),
            b: unit("b")
        })
    );
    for (a, b, who, word) in [
        (Active, Changing("activating".into()), "b", "activating"),
        (
            Changing("deactivating".into()),
            Stopped,
            "a",
            "deactivating",
        ),
        (Changing("reloading".into()), Active, "a", "reloading"),
    ] {
        assert_eq!(
            decide(&states(a, b)),
            Err(Refusal::Changing {
                unit: unit(who),
                state: word.into()
            })
        );
    }
}

/// Catches `failed` taken for a unit that is up (a crashed instance would block every
/// handover), and an unknown word taken for one that is down.
#[test]
fn systemds_words_map_to_up_down_and_changing() {
    assert_eq!(State::of("active\n"), State::Active);
    assert_eq!(State::of("inactive"), State::Stopped);
    assert_eq!(State::of("failed"), State::Stopped);
    assert_eq!(
        State::of("maintenance"),
        State::Changing("maintenance".into())
    );
}

/// Catches the unit names the command starts drifting from the template's, and a default
/// that reads the wrong `systemctl`.
#[test]
fn the_defaults_name_the_template_and_the_systemctl_on_the_path() {
    use clap::FromArgMatches as _;
    let cmd = Handover::augment_args(clap::Command::new("handover"));
    let m = cmd.try_get_matches_from(["handover"]).expect("parses");
    let h = Handover::from_arg_matches(&m).expect("flags");
    assert_eq!(
        h,
        Handover {
            systemctl: PathBuf::from("systemctl"),
            wait: false,
            poll_ms: 1000
        }
    );
    assert_eq!(unit("a"), "kbf-frontend@a.service");
}

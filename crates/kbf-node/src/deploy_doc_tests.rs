//! The daemon units in `docs/deploy/*.md` against `kbf-daemon`'s flag parser. The
//! server's units are checked in `kbf-it` (`tests/deploy_units.rs`); this half lives
//! here because the daemon's command line, [`Cli`], is private to this binary.

use std::path::Path;

use clap::Parser as _;
use kbf_it::deploy_units::{Unit, units};

use super::Cli;

#[test]
fn every_daemon_units_flags_parse_with_kbf_daemons_parser() {
    // Catches: a doc example naming a flag `kbf-daemon` does not have (a misspelling,
    // or one renamed in code), giving one a value it refuses
    // (`--actions-memory-max-gib=0`), or leaving out `--driver`; and the check passing
    // vacuously because the docs or their units moved.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/deploy");
    let mut checked = Vec::new();
    for entry in std::fs::read_dir(dir).expect("docs/deploy") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&path).expect("read doc");
        let found = units(&text);
        assert_eq!(found.as_ref().err(), None, "{name}");
        let daemons = found
            .expect("checked above")
            .iter()
            .filter_map(Unit::exec_start)
            .map(|argv| argv.expect("every placeholder has a sample value"))
            .filter(|argv| argv[0] == "/usr/local/bin/kbf-daemon")
            .collect::<Vec<_>>();
        for argv in daemons {
            let refused = Cli::try_parse_from(&argv).err();
            let refused = refused.as_ref().map(ToString::to_string);
            assert_eq!(refused, None, "{name}: kbf-daemon refuses {argv:?}");
            checked.push(name.clone());
        }
    }
    checked.sort();
    // The build host's and the converged host's daemon units.
    assert_eq!(checked, ["linux-build-host.md", "storage-node.md"]);
}

#[test]
fn the_rendered_daemon_unit_parses_with_kbf_daemons_parser() {
    // Catches: a `kbf-deploy linux` template naming a kbf-daemon flag the binary does
    // not have or a value it refuses, and the pinned images rendered in a form the
    // daemon refuses at start (a bare `<repo>@sha256:<digest>`), which makes it
    // exit 2. The flag parser takes `--image` as text; the daemon checks the form at
    // start with `ImageRef::parse`, which this test runs on each value.
    use kbf_deploy::linux::{LinuxConfig, Templates, render::render};
    use kbf_deploy::unit::UnitFile;

    let example = include_str!("../../kbf-deploy/templates/linux/config.example");
    let image = format!("docker.io/library/busybox@sha256:{}", "a".repeat(64));
    let text = example.replace("KBF_IMAGES=\n", &format!("KBF_IMAGES={image}\n"));
    assert_ne!(
        text, example,
        "config.example has no empty KBF_IMAGES= line"
    );
    let c = LinuxConfig::parse(&text).expect("valid settings");
    let units = render(&c, Some(1500), &Templates::BUILT_IN).expect("renders");
    let daemon = UnitFile::parse(&units.daemon).expect("a unit");
    let argv = daemon.exec_start();
    assert_eq!(argv[0], "/usr/local/bin/kbf-daemon");
    let cli = Cli::try_parse_from(&argv);
    let refused = cli.as_ref().err().map(ToString::to_string);
    assert_eq!(refused, None, "kbf-daemon refuses {argv:?}");
    assert_eq!(daemon.flag_values("image"), [format!("docker://{image}")]);
    for value in daemon.flag_values("image") {
        assert_eq!(kbf_types::ImageRef::parse(value).err(), None, "{value}");
    }
}

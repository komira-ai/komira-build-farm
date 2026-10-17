//! The `runtime` rule of `check-units`: a unit that runs builds runs them with `crun`
//! and with nothing else.
//!
//! Podman was measured at about twenty seconds to launch a container, `crun` at
//! milliseconds, and was dropped (it is not a runtime `kbf-daemon` can be given). A unit
//! of the old templates says nothing about the runtime and so ran Podman by default; a
//! unit of an old setup may still name it. So a build node's unit must say, in its
//! `ExecStart=`, `--container-runtime=oci` and the `--oci-runtime=` program, and no file
//! of the unit (its drop-ins too) may name Podman at all, in a flag, an environment or a
//! comment: a unit that still mentions it is a unit nobody finished converting.

use crate::unit::UnitFile;

/// What is wrong with the runtime of `unit` (named `name`), read from `texts` (each file
/// of it, by name): one message per defect, none when it is crun's alone.
pub(super) fn check(name: &str, unit: &UnitFile, texts: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for (file, text) in texts {
        let named = text
            .lines()
            .position(|line| line.to_ascii_lowercase().contains("podman"));
        if let Some(line) = named {
            out.push(format!(
                "{file} line {} names Podman: kbf runs containers with crun only",
                line + 1
            ));
        }
    }
    match unit.flag("container-runtime") {
        Some("oci") => {}
        Some(other) => out.push(format!(
            "{name} passes --container-runtime={other}: kbf-daemon runs containers with crun only (oci)"
        )),
        None => out.push(format!(
            "{name} does not pass --container-runtime=oci: an older kbf-daemon ran Podman by default"
        )),
    }
    if unit.flag("oci-runtime").is_none_or(str::is_empty) {
        out.push(format!(
            "{name} does not pass --oci-runtime=<the crun program>"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::config::LinuxConfig;
    use crate::linux::host::CRUN;
    use crate::linux::modes::{self, Mode};

    /// Catches Podman in a rendered unit, in any of the files `render-units` writes
    /// (converged or per mode) and in the example settings that render them: no file names
    /// it, and every unit that runs builds passes the runtime flags, so a node set up from
    /// them runs `crun` and nothing else.
    #[test]
    fn no_rendered_unit_names_podman_and_every_build_unit_passes_the_runtime() {
        let example = include_str!("../../templates/linux/config.example");
        let config = LinuxConfig::parse(example).expect("valid settings");
        let names_podman = |text: &str| text.to_ascii_lowercase().contains("podman");
        assert!(!names_podman(example), "config.example names Podman");
        let converged = crate::linux::render::render(
            &config,
            Some(1500),
            &crate::linux::render::Templates::BUILT_IN,
        )
        .expect("renders");
        for (name, text) in [
            ("kbf.slice", &converged.slice),
            ("minio", &converged.minio),
            ("kbf-server.service", &converged.server),
            ("kbf-daemon.service", &converged.daemon),
        ] {
            assert!(!names_podman(text), "{name} names Podman");
        }
        let daemon = UnitFile::parse(&converged.daemon).expect("unit");
        assert_eq!(daemon.flag("container-runtime"), Some("oci"));
        for mode in Mode::ALL {
            let text = modes::render(&config, Some(1500), mode).expect("renders");
            assert!(!names_podman(&text), "{mode} names Podman");
            let unit = UnitFile::parse(&text).expect("unit");
            let expected = mode.runs_builds().then_some("oci");
            assert_eq!(unit.flag("container-runtime"), expected, "{mode}");
        }
    }

    /// Catches the runtime path the units pass drifting from the one `install-units`
    /// checks is installed: a node would pass the install and fail to start.
    #[test]
    fn the_units_name_the_crun_the_installer_looks_for() {
        let example = include_str!("../../templates/linux/config.example");
        let config = LinuxConfig::parse(example).expect("valid settings");
        for mode in [Mode::Daemon, Mode::StorageAndDaemon, Mode::AllInOne] {
            let text = modes::render(&config, Some(1500), mode).expect("renders");
            let unit = UnitFile::parse(&text).expect("unit");
            assert_eq!(unit.flag("oci-runtime"), Some(CRUN), "{mode}");
        }
    }

    fn unit(exec: &str) -> UnitFile {
        UnitFile::parse(&format!("[Service]\nExecStart={exec}\n")).expect("unit")
    }

    fn failures(exec: &str, extra: &str) -> Vec<String> {
        let text = format!("[Service]\n{extra}ExecStart={exec}\n");
        check(
            "kbf-daemon.service",
            &unit(exec),
            &[("kbf-daemon.service", &text)],
        )
    }

    /// Catches a unit of crun alone being refused, and each way of still running or
    /// naming Podman, or of saying nothing about the runtime, passing: a runtime other
    /// than oci, no runtime flag (an old binary's default was Podman), no runtime
    /// program, an empty one, and the name in any file or comment.
    #[test]
    fn a_build_unit_runs_crun_and_names_no_podman() {
        let good = "/usr/bin/kbf daemon --container-runtime=oci --oci-runtime=/usr/bin/crun";
        assert_eq!(failures(good, ""), Vec::<String>::new());
        let podman = failures(
            "/usr/bin/kbf daemon --container-runtime=podman --oci-runtime=/usr/bin/crun",
            "",
        );
        assert_eq!(podman.len(), 2, "{podman:?}");
        assert!(podman[0].contains("names Podman"), "{podman:?}");
        assert!(
            podman[1].starts_with("kbf-daemon.service passes --container-runtime=podman"),
            "{podman:?}"
        );
        let silent = failures("/usr/bin/kbf daemon --oci-runtime=/usr/bin/crun", "");
        assert_eq!(silent.len(), 1, "{silent:?}");
        assert!(
            silent[0].contains("does not pass --container-runtime=oci"),
            "{silent:?}"
        );
        for exec in [
            "/usr/bin/kbf daemon --container-runtime=oci",
            "/usr/bin/kbf daemon --container-runtime=oci --oci-runtime=",
        ] {
            let missing = failures(exec, "");
            assert_eq!(missing.len(), 1, "{exec}: {missing:?}");
            assert!(missing[0].contains("--oci-runtime"), "{missing:?}");
        }
        let comment = failures(good, "# runs PodMan rootless\n");
        assert_eq!(comment.len(), 1, "{comment:?}");
        assert!(
            comment[0].starts_with("kbf-daemon.service line 2 names Podman"),
            "{comment:?}"
        );
        let env = failures(good, "Environment=PODMAN_IGNORE_CGROUPSV1_WARNING=1\n");
        assert_eq!(env.len(), 1, "{env:?}");
    }

    /// Catches only the first file of a unit being read: a drop-in that names Podman is
    /// reported under its own name.
    #[test]
    fn a_drop_in_that_names_podman_is_named() {
        let unit = unit("/usr/bin/kbf daemon --container-runtime=oci --oci-runtime=/usr/bin/crun");
        let texts = [
            ("kbf-daemon.service", "[Service]\n"),
            (
                "kbf-daemon.service.d/old.conf",
                "[Service]\nEnvironment=CONTAINERS=podman\n",
            ),
        ];
        let found = check("kbf-daemon.service", &unit, &texts);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("kbf-daemon.service.d/old.conf line 2 names Podman"),
            "{found:?}"
        );
    }
}

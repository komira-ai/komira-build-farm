//! The `runtime` rule of `check-units`: a unit that runs builds runs them with `crun`.
//!
//! `kbf-daemon` runs containers with `crun` only (a launch takes milliseconds) and has
//! no flag to choose another runtime. A unit of an older setup may still pass
//! `--container-runtime=...`, which the daemon now refuses at start; this rule says so
//! before the unit is installed. A build node's unit must also say, in its
//! `ExecStart=`, the `--oci-runtime=` program.

use crate::unit::UnitFile;

/// What is wrong with the runtime of `unit` (named `name`): one message per defect,
/// none when it names the `crun` program and nothing else.
pub(super) fn check(name: &str, unit: &UnitFile) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(flag) = unit.flag("container-runtime") {
        out.push(format!(
            "{name} passes --container-runtime={flag}: kbf-daemon has no such flag (crun is its only runtime); remove it"
        ));
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

    /// Catches a rendered unit (converged or per mode) that does not pass the `crun`
    /// program, or that passes a runtime flag the daemon no longer has: every unit that
    /// runs builds passes `--oci-runtime=`, so a node set up from them runs `crun`.
    #[test]
    fn every_rendered_build_unit_passes_the_crun_program_and_no_runtime_flag() {
        let example = include_str!("../../templates/linux/config.example");
        let config = LinuxConfig::parse(example).expect("valid settings");
        let converged = crate::linux::render::render(
            &config,
            Some(1500),
            &crate::linux::render::Templates::BUILT_IN,
        )
        .expect("renders");
        let daemon = UnitFile::parse(&converged.daemon).expect("unit");
        assert_eq!(check("kbf-daemon.service", &daemon), Vec::<String>::new());
        for mode in Mode::ALL {
            let text = modes::render(&config, Some(1500), mode).expect("renders");
            let unit = UnitFile::parse(&text).expect("unit");
            assert_eq!(unit.flag("container-runtime"), None, "{mode}");
            let expected = mode.runs_builds().then_some(CRUN);
            assert_eq!(unit.flag("oci-runtime"), expected, "{mode}");
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

    fn failures(exec: &str) -> Vec<String> {
        check("kbf-daemon.service", &unit(exec))
    }

    /// Catches a unit of crun alone being refused, and each way of naming a runtime flag
    /// the daemon no longer has, or no runtime program, passing: any
    /// `--container-runtime=` value, no runtime program, and an empty one.
    #[test]
    fn a_build_unit_names_the_crun_program_and_no_runtime_flag() {
        let good = "/usr/bin/kbf daemon --oci-runtime=/usr/bin/crun";
        assert_eq!(failures(good), Vec::<String>::new());
        for value in ["oci", "runc"] {
            let old = failures(&format!(
                "/usr/bin/kbf daemon --container-runtime={value} --oci-runtime=/usr/bin/crun"
            ));
            assert_eq!(old.len(), 1, "{old:?}");
            assert!(
                old[0].starts_with(&format!(
                    "kbf-daemon.service passes --container-runtime={value}: kbf-daemon has no such flag"
                )),
                "{old:?}"
            );
        }
        for exec in ["/usr/bin/kbf daemon", "/usr/bin/kbf daemon --oci-runtime="] {
            let missing = failures(exec);
            assert_eq!(missing.len(), 1, "{exec}: {missing:?}");
            assert!(missing[0].contains("--oci-runtime"), "{missing:?}");
        }
    }
}

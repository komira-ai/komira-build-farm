use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;

use super::*;
use kbf_testkit::dir::TestDir;

mod kbf_tests;

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-pkg-unit"), name)
}

/// The inputs a package is built from, in `dir`.
struct Tree {
    dir: TestDir,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let dir = scratch(name);
        let t = Tree { dir };
        t.put("kbf-installer", "binary");
        t.put("kbf", "kbf binary");
        t.put("units/kbf-node.service", "node unit");
        t.put("units/kbf-installer.service", "installer unit");
        for unit in KBF_UNITS {
            t.put(&format!("units/{unit}"), unit);
        }
        for (n, _) in TRUST_FILES {
            t.put(&format!("trust/{n}"), n);
        }
        t.put("sysusers.conf", "u kbf");
        t.put("tmpfiles.conf", "d /x");
        t.put("needrestart.conf", "# n");
        t
    }

    fn put(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("file");
    }

    fn inputs(&self) -> Inputs<'_> {
        // The leaked paths live as long as the test.
        let at = |rel: &str| -> &'static Path { Box::leak(self.dir.join(rel).into_boxed_path()) };
        Inputs {
            binary: at("kbf-installer"),
            kbf: at("kbf"),
            units: at("units"),
            trust: at("trust"),
            sysusers: at("sysusers.conf"),
            tmpfiles: at("tmpfiles.conf"),
            needrestart: at("needrestart.conf"),
            platform: Platform::LinuxX86_64,
            version: "0.0.0+ci",
        }
    }
}

fn refused(r: Result<Vec<Entry>, Error>) -> String {
    match r {
        Err(Error::Refused(problems)) => problems.join("\n"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_plan_is_the_contract_paths_with_their_modes() {
    let t = Tree::new("plan");
    let got: Vec<(String, u32, bool)> = plan(&t.inputs())
        .expect("plan")
        .into_iter()
        .map(|e| (e.dst, e.mode, e.sha256.is_some()))
        .collect();
    let want = [
        ("/etc/needrestart/conf.d/kbf.conf", 0o644, true),
        ("/usr/bin/kbf-installer", 0o755, true),
        ("/usr/lib/systemd/system/kbf-installer.service", 0o644, true),
        ("/usr/lib/systemd/system/kbf-node.service", 0o644, true),
        ("/usr/lib/sysusers.d/kbf-installer.conf", 0o644, true),
        ("/usr/lib/tmpfiles.d/kbf-installer.conf", 0o644, true),
        ("/usr/share/kbf-installer", 0o755, false),
        ("/usr/share/kbf-installer/trust", 0o755, false),
        ("/usr/share/kbf-installer/trust/gh", 0o755, true),
        ("/usr/share/kbf-installer/trust/installer.conf", 0o644, true),
        (
            "/usr/share/kbf-installer/trust/trusted_root.jsonl",
            0o644,
            true,
        ),
    ];
    let want: Vec<(String, u32, bool)> = want
        .iter()
        .map(|(p, m, f)| ((*p).to_owned(), *m, *f))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn the_plan_refuses_what_the_package_does_not_ship() {
    // Each case changes the tree in one way and names what the refusal must say.
    type Change = fn(&Tree) -> Inputs<'_>;
    let cases: [(&str, Change, &str); 11] = [
        (
            "a unit not named kbf-*.service",
            |t| {
                t.put("units/other.service", "x");
                t.inputs()
            },
            "other.service: a package unit is named kbf-*.service or kbf-*.socket",
        ),
        (
            "a unit that is a timer",
            |t| {
                t.put("units/kbf-gc.timer", "x");
                t.inputs()
            },
            "kbf-gc.timer: a package unit is named kbf-*.service or kbf-*.socket",
        ),
        (
            "a directory among the units",
            |t| {
                t.put("units/sub/kbf-x.service", "x");
                t.inputs()
            },
            "is not a regular file",
        ),
        (
            "no unit",
            |t| {
                std::fs::remove_file(t.dir.join("units/kbf-node.service")).expect("rm");
                std::fs::remove_file(t.dir.join("units/kbf-installer.service")).expect("rm");
                t.inputs()
            },
            "no unit files in",
        ),
        (
            "a missing trust file",
            |t| {
                std::fs::remove_file(t.dir.join("trust/gh")).expect("rm");
                t.inputs()
            },
            "trust file gh is missing",
        ),
        (
            "an extra trust file",
            |t| {
                t.put("trust/extra.pem", "x");
                t.inputs()
            },
            "extra.pem: the package ships no trust file of that name",
        ),
        (
            "a missing binary",
            |t| {
                std::fs::remove_file(t.dir.join("kbf-installer")).expect("rm");
                t.inputs()
            },
            "kbf-installer",
        ),
        (
            "a unit directory that is not there",
            |t| {
                let mut i = t.inputs();
                i.units = Path::new("/nonexistent/units");
                i
            },
            "/nonexistent/units cannot be read as a directory",
        ),
        (
            "a version with a space",
            |t| {
                let mut i = t.inputs();
                i.version = "1 0";
                i
            },
            "version \"1 0\" is not digits first",
        ),
        (
            "a version that starts with a letter",
            |t| {
                let mut i = t.inputs();
                i.version = "v1";
                i
            },
            "version \"v1\" is not digits first",
        ),
        (
            "a macOS platform",
            |t| {
                let mut i = t.inputs();
                i.platform = Platform::DarwinArm64;
                i
            },
            "darwin-arm64 is not a Linux platform",
        ),
    ];
    for (n, (what, change, says)) in cases.iter().enumerate() {
        let t = Tree::new(&format!("refuse-{n}"));
        let text = refused(plan(&change(&t)));
        assert!(text.contains(says), "{what}: {text}");
    }
}

#[test]
fn the_plan_collects_every_problem() {
    let t = Tree::new("collects");
    t.put("units/other.service", "x");
    std::fs::remove_file(t.dir.join("trust/gh")).expect("rm");
    let text = refused(plan(&t.inputs()));
    assert_eq!(text.lines().count(), 2, "{text}");
}

#[test]
fn the_config_has_the_contract_dependencies_no_script_and_root_files() {
    let t = Tree::new("config");
    let entries = plan(&t.inputs()).expect("plan");
    let text = config(
        Package::Installer,
        &entries,
        Platform::LinuxX86_64,
        "1.2.3+ci",
    );
    assert!(text.contains("arch: \"amd64\""), "{text}");
    assert!(text.contains("version: \"1.2.3+ci\""), "{text}");
    assert!(
        text.contains(
            "depends:\n  - \"crun\"\n  - \"uidmap\"\n  - \"systemd\"\n  - \"ca-certificates\"\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "overrides:\n  rpm:\n    depends:\n      - \"crun\"\n      - \"shadow-utils\""
        ),
        "{text}"
    );
    assert!(text.contains("buildhost: \"kbf-build\""), "{text}");
    for banned in [
        "podman",
        "envoy",
        "scripts",
        "postinstall",
        "preinstall",
        "mtime",
    ] {
        assert!(!text.contains(banned), "{banned} in {text}");
    }
    assert_eq!(
        text.matches("owner: root, group: root").count(),
        entries.len()
    );
    assert!(
        text.contains("  - dst: \"/usr/bin/kbf-installer\"\n    src: "),
        "{text}"
    );
    assert!(
        text.contains("file_info: {mode: 0755, owner: root, group: root}"),
        "{text}"
    );
    assert!(
        text.contains("file_info: {mode: 0644, owner: root, group: root}"),
        "{text}"
    );
    assert!(
        text.contains("  - dst: \"/usr/share/kbf-installer/trust\"\n    type: \"dir\"\n"),
        "{text}"
    );
    let arm = config(Package::Installer, &entries, Platform::LinuxArm64, "1.2.3");
    assert!(arm.contains("arch: \"arm64\""), "{arm}");
}

#[test]
fn the_config_is_the_same_text_for_the_same_inputs() {
    let t = Tree::new("same-config");
    let a = config(
        Package::Installer,
        &plan(&t.inputs()).expect("plan"),
        Platform::LinuxX86_64,
        "1.0.0",
    );
    // A touched source changes nothing in the text.
    stamp(&sources(&a).expect("sources"), 42).expect("stamp");
    let b = config(
        Package::Installer,
        &plan(&t.inputs()).expect("plan"),
        Platform::LinuxX86_64,
        "1.0.0",
    );
    assert_eq!(a, b);
}

#[test]
fn the_listing_round_trips() {
    let t = Tree::new("listing");
    let entries = plan(&t.inputs()).expect("plan");
    let text = listing(&entries);
    assert!(
        text.contains("dir 0755 /usr/share/kbf-installer -\n"),
        "{text}"
    );
    let parsed = parse_listing(&text).expect("parse");
    assert_eq!(parsed.len(), entries.len());
    for (p, e) in parsed.iter().zip(&entries) {
        assert_eq!((&p.dst, p.mode, p.sha256), (&e.dst, e.mode, e.sha256));
        assert!(p.src.as_os_str().is_empty());
    }
}

#[test]
fn a_listing_line_that_is_not_one_is_refused() {
    let sha = "0".repeat(64);
    for line in [
        "file 0644 /a".to_owned(),
        format!("dir 0755 /a {sha}"),
        "file 0644 /a nothex".to_owned(),
        format!("file 9 /a {sha}"),
        format!("link 0644 /a {sha}"),
    ] {
        let err = parse_listing(&line).expect_err(&line);
        assert_eq!(err, Error::Failed(format!("not a listing line: {line:?}")));
    }
}

/// Asserts that `write_config` writes the configuration and the listing of each package, the
/// `kbf-installer` pair in the directory it is given and the `kbf` pair in `kbf` below it, says
/// so once per file, and that each listing is the entries of its own package.
#[test]
fn write_config_writes_both_files_of_both_packages_and_says_so() {
    let t = Tree::new("write");
    let out = t.dir.join("out/nested");
    let printed = write_config(&t.inputs(), &out).expect("write");
    assert_eq!(printed.len(), 4);
    let yaml = std::fs::read_to_string(out.join("nfpm.yaml")).expect("yaml");
    let entries = parse_listing(&std::fs::read_to_string(out.join("listing.txt")).expect("l"))
        .expect("listing");
    assert!(yaml.contains(&entries[0].dst));
    assert!(yaml.contains("name: \"kbf-installer\""), "{yaml}");
    let kbf_yaml = std::fs::read_to_string(out.join("kbf/nfpm.yaml")).expect("kbf yaml");
    let kbf_listing = std::fs::read_to_string(out.join("kbf/listing.txt")).expect("kbf listing");
    let kbf_entries = parse_listing(&kbf_listing).expect("kbf listing");
    assert!(kbf_yaml.contains("name: \"kbf\""), "{kbf_yaml}");
    assert_eq!(kbf_entries.len(), 1 + KBF_UNITS.len());
    assert_eq!(kbf_entries[0].dst, KBF_PATH);
    for line in &printed {
        assert!(line.starts_with("wrote "), "{line}");
    }
}

#[test]
fn write_config_reports_a_directory_it_cannot_make_and_a_file_it_cannot_write() {
    let t = Tree::new("write-fails");
    let in_the_way = t.dir.join("file");
    std::fs::write(&in_the_way, "x").expect("file");
    let err = write_config(&t.inputs(), &in_the_way).expect_err("a file is no directory");
    assert!(err.to_string().contains("create"), "{err}");
    let out = t.dir.join("out");
    std::fs::create_dir_all(out.join("nfpm.yaml")).expect("dir");
    let err = write_config(&t.inputs(), &out).expect_err("a directory is no file");
    assert!(err.to_string().contains("write"), "{err}");
}

#[test]
fn the_real_sysusers_and_tmpfiles_files_say_what_the_contract_says() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let lines = |name: &str| -> Vec<Vec<String>> {
        std::fs::read_to_string(root.join(name))
            .expect("file")
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| l.split_whitespace().map(str::to_owned).collect())
            .collect()
    };
    let all = lines("sysusers.d/kbf-installer.conf");
    // The signer's socket group: made, with the signer (which gives files to it) and the
    // frontend's user (which connects as a member) in it, and no other member.
    let (entries, others): (Vec<_>, Vec<_>) = all.into_iter().partition(|f| f[0] == "u");
    let want_others: Vec<Vec<String>> = [
        ["g", "kbf-signer-clients", "-"],
        ["m", "kbf-signer", "kbf-signer-clients"],
        ["m", "kbf-server", "kbf-signer-clients"],
    ]
    .iter()
    .map(|f| f.iter().map(|x| (*x).to_owned()).collect())
    .collect();
    assert_eq!(others, want_others);
    // Design section 3: `kbf-signer` owns the CA key and has no home, no login and a
    // system id with a group of its own name; there is no `kbf-verify` user.
    let signer: Vec<&Vec<String>> = entries.iter().filter(|f| f[1] == "kbf-signer").collect();
    assert_eq!(signer.len(), 1, "exactly one kbf-signer entry: {entries:?}");
    let f = signer[0];
    assert_eq!(
        [
            f[0].as_str(),
            f[2].as_str(),
            f[f.len() - 2].as_str(),
            f[f.len() - 1].as_str()
        ],
        ["u", "-", "/nonexistent", "/usr/sbin/nologin"],
        "{f:?}"
    );
    assert!(
        entries.iter().all(|f| f[1] != "kbf-verify"),
        "section 3: there is no kbf-verify user: {entries:?}"
    );
    let users: Vec<String> = entries
        .into_iter()
        .map(|f| {
            assert_eq!((f[0].as_str(), f[3].starts_with('"')), ("u", true), "{f:?}");
            assert_eq!(
                f.last().map(String::as_str),
                Some("/usr/sbin/nologin"),
                "no login: {f:?}"
            );
            f[1].clone()
        })
        .collect();
    assert_eq!(users, USERS);
    let dirs: Vec<(String, String)> = lines("tmpfiles.d/kbf-installer.conf")
        .into_iter()
        .map(|f| {
            assert_eq!([&f[0], &f[3], &f[4]], ["d", "root", "root"], "{f:?}");
            (f[1].clone(), f[2].trim_start_matches('0').to_owned())
        })
        .collect();
    let want: Vec<(String, String)> = TMPFILES_DIRS
        .iter()
        .map(|(p, m)| ((*p).to_owned(), (*m).to_owned()))
        .collect();
    assert_eq!(dirs, want);
}

#[test]
fn the_fixtures_make_a_plan_with_the_real_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let binary = root.join("testdata/units/kbf-node.service");
    let plan = plan(&Inputs {
        binary: &binary,
        kbf: &binary,
        units: &root.join("testdata/units"),
        trust: &root.join("testdata/trust"),
        sysusers: &root.join("sysusers.d/kbf-installer.conf"),
        tmpfiles: &root.join("tmpfiles.d/kbf-installer.conf"),
        needrestart: &root.join("needrestart/kbf.conf"),
        platform: Platform::LinuxX86_64,
        version: "0.0.0+ci",
    })
    .expect("the fixtures are a plan");
    assert!(plan.iter().any(|e| e.dst.ends_with("kbf-node.service")));
    assert!(
        plan.iter()
            .any(|e| e.dst.ends_with("kbf-installer.service"))
    );
}

/// The environment and arguments of one call of the model.
type Call = (Vec<(String, String)>, Vec<String>);

/// A model of nfpm: records its calls and writes `<dir>/kbf-installer.<format>` holding
/// what `bytes` says of the call.
struct Model {
    calls: RefCell<Vec<Call>>,
}

impl Model {
    fn new() -> Model {
        Model {
            calls: RefCell::new(Vec::new()),
        }
    }

    /// The nfpm closure; `leave` says what a call leaves in its target directory:
    /// the file names, with the bytes the source's time and the environment give.
    fn nfpm<'a>(
        &'a self,
        source: &'a Path,
        leave: impl Fn(&str, &str) -> Vec<(String, String)> + 'a,
    ) -> impl FnMut(&[(&str, &str)], &[&str]) -> Result<Out, Error> + 'a {
        move |env, args| {
            let env: Vec<(String, String)> = env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            self.calls
                .borrow_mut()
                .push((env.clone(), args.iter().map(|a| (*a).to_owned()).collect()));
            let at = args.iter().position(|a| *a == "-p").expect("-p");
            let format = args[at + 1];
            let dir = Path::new(args[args.len() - 1]);
            let mtime = std::fs::metadata(source)
                .expect("source")
                .mtime()
                .to_string();
            let epoch = env[0].1.clone();
            for (name, text) in leave(format, &format!("{epoch}/{mtime}")) {
                std::fs::write(dir.join(name), text).expect("package");
            }
            Ok(Out {
                ok: true,
                ..Out::default()
            })
        }
    }
}

/// A config naming one source file, and the build over it.
fn built(
    name: &str,
    leave: impl Fn(&str, &str) -> Vec<(String, String)>,
) -> (Result<Vec<String>, Error>, TestDir, Model) {
    let dir = scratch(name);
    let source = dir.join("kbf-installer");
    std::fs::write(&source, "binary").expect("source");
    let config_path = dir.join("nfpm.yaml");
    let config = format!(
        "contents:\n  - dst: \"/x\"\n    src: {}\n",
        quoted(&source.to_string_lossy())
    );
    std::fs::write(&config_path, config).expect("config");
    let model = Model::new();
    let out = dir.join("out");
    let build = Build {
        config: &config_path,
        out: &out,
        epoch: 77,
    };
    let result = super::build(&mut model.nfpm(&source, leave), &build);
    // The caller holds the directory, which the output is in, until it is done.
    (result, dir, model)
}

/// Package bytes that depend on the environment's time only.
fn by_epoch(format: &str, seen: &str) -> Vec<(String, String)> {
    let epoch = seen.split('/').next().expect("epoch");
    vec![(
        format!("kbf-installer.{format}"),
        format!("{format} at {epoch}"),
    )]
}

#[test]
fn a_package_that_depends_on_the_epoch_alone_is_built_twice_and_the_same() {
    let (result, dir, model) = built("build-same", by_epoch);
    let out = dir.join("out");
    let printed = result.expect("reproducible");
    assert_eq!(printed.len(), 2);
    assert!(printed[0].ends_with("kbf-installer.deb"), "{printed:?}");
    assert!(printed[1].ends_with("kbf-installer.rpm"), "{printed:?}");
    let calls = model.calls.borrow();
    assert_eq!(calls.len(), 4, "two formats, two builds each");
    for (env, args) in calls.iter() {
        assert_eq!(env, &[("SOURCE_DATE_EPOCH".to_owned(), "77".to_owned())]);
        assert_eq!(&args[..3], ["package", "-f", args[2].as_str()]);
    }
    assert!(out.join("kbf-installer.deb").is_file());
    assert!(out.join("kbf-installer.rpm").is_file());
    assert!(
        !out.join("again").exists(),
        "the second builds are not left"
    );
}

#[test]
fn a_package_that_carries_the_time_of_its_sources_is_refused() {
    // The defect `SOURCE_DATE_EPOCH` is for: nfpm taking the sources' own times.
    let (result, _, _) = built("build-mtime", |format, seen| {
        let mtime = seen.split('/').nth(1).expect("mtime");
        vec![(
            format!("kbf-installer.{format}"),
            format!("{format} at {mtime}"),
        )]
    });
    let err = result.expect_err("not reproducible").to_string();
    assert!(
        err.contains("kbf-installer.deb is not reproducible"),
        "{err}"
    );
}

#[test]
fn nfpm_failing_or_leaving_the_wrong_files_is_refused() {
    let (result, _, _) = built("build-none", |_, _| Vec::new());
    assert!(
        result
            .expect_err("none")
            .to_string()
            .contains("expected nfpm to leave one .deb")
    );
    let (result, _, _) = built("build-two", |format, _| {
        vec![
            (format!("a.{format}"), String::new()),
            (format!("b.{format}"), String::new()),
        ]
    });
    assert!(
        result
            .expect_err("two")
            .to_string()
            .contains("expected nfpm to leave one .deb")
    );
}

#[test]
fn nfpm_that_fails_is_reported_with_its_message() {
    let dir = scratch("build-fails");
    let config = dir.join("nfpm.yaml");
    std::fs::write(&config, "contents: []\n").expect("config");
    let mut fails = |_: &[(&str, &str)], _: &[&str]| {
        Ok(Out {
            ok: false,
            stdout: String::new(),
            stderr: " bad yaml \n".to_owned(),
        })
    };
    let err = build(
        &mut fails,
        &Build {
            config: &config,
            out: &dir.join("out"),
            epoch: 1,
        },
    )
    .expect_err("fails");
    assert_eq!(
        err,
        Error::Failed("nfpm package -p deb: bad yaml".to_owned())
    );
}

#[test]
fn a_config_with_a_broken_src_line_or_a_missing_source_is_refused() {
    let dir = scratch("build-config");
    let config = dir.join("nfpm.yaml");
    let mut never = |_: &[(&str, &str)], _: &[&str]| -> Result<Out, Error> { panic!("not run") };
    let build_with = |text: &str, never: &mut Nfpm| {
        std::fs::write(&config, text).expect("config");
        build(
            never,
            &Build {
                config: &config,
                out: &dir.join("out"),
                epoch: 1,
            },
        )
        .expect_err("refused")
        .to_string()
    };
    assert!(build_with("    src: not json\n", &mut never).contains("not a src line"));
    assert!(
        build_with("    src: \"/nonexistent/x\"\n", &mut never)
            .contains("set the time of /nonexistent/x")
    );
    let missing = dir.join("absent.yaml");
    let err = build(
        &mut never,
        &Build {
            config: &missing,
            out: &dir,
            epoch: 1,
        },
    )
    .expect_err("absent");
    assert!(err.to_string().contains("absent.yaml"), "{err}");
}

#[test]
fn the_nfpm_process_gets_the_given_environment_and_nothing_else() {
    let mut env = nfpm_process(Path::new("/usr/bin/env"));
    let out = env(&[("A", "1")], &[]).expect("env");
    assert!(out.ok);
    assert_eq!(out.stdout, "A=1\n");
    let mut no = nfpm_process(Path::new("/nonexistent/nfpm"));
    let err = no(&[], &[]).expect_err("not a program");
    assert!(err.to_string().contains("/nonexistent/nfpm"), "{err}");
    let mut fails = nfpm_process(Path::new("/usr/bin/false"));
    assert!(!fails(&[], &[]).expect("false").ok);
}

/// Asserts that the two packages together ship exactly the files of `packaging/linux/units`:
/// a unit added there is packaged, and nothing else is.
#[test]
fn the_packages_ship_exactly_the_units_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let held = |dir: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root.join(dir))
            .expect("units directory")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let units = held("units");
    assert!(
        units.iter().any(|n| n == "kbf-installer.service"),
        "{units:?}"
    );
    let inputs = Inputs {
        binary: &root.join("units/kbf-installer.service"),
        kbf: &root.join("units/kbf-daemon.service"),
        units: &root.join("units"),
        trust: &root.join("testdata/trust"),
        sysusers: &root.join("sysusers.d/kbf-installer.conf"),
        tmpfiles: &root.join("tmpfiles.d/kbf-installer.conf"),
        needrestart: &root.join("needrestart/kbf.conf"),
        platform: Platform::LinuxX86_64,
        version: "0.0.0+ci",
    };
    let units_of = |entries: Vec<Entry>| -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| e.dst.strip_prefix(&format!("{UNIT_DIR}/")))
            .map(str::to_owned)
            .collect()
    };
    let installer = units_of(plan(&inputs).expect("the real units are a plan"));
    let kbf = units_of(kbf_plan(&inputs).expect("the real units hold the mode units"));
    let mut shipped = [installer, kbf].concat();
    shipped.sort();
    assert_eq!(shipped, units);
}

/// The kbf-installer package depends on no Envoy: Envoy runs on frontend hosts only, and a
/// hard dependency would put it on every node.
#[test]
fn no_dependency_of_either_package_names_envoy() {
    for dep in DEB_DEPENDS.iter().chain(RPM_DEPENDS.iter()) {
        assert!(!dep.to_lowercase().contains("envoy"), "{dep}");
    }
}

/// The package suggests a Postgres server and does not depend on it: node hosts never use
/// one, and apt installs `Recommends` by default, which would start a database on every node.
/// Catches Postgres moving into `depends` (every node gets a server) and the hint vanishing
/// (the frontend operator is not told what to install).
#[test]
fn postgres_is_suggested_not_required() {
    for dep in DEB_DEPENDS.iter().chain(RPM_DEPENDS.iter()) {
        assert!(!dep.to_lowercase().contains("postgres"), "{dep}");
    }
    let cfg = config(Package::Installer, &[], Platform::LinuxX86_64, "0.0.0+ci");
    let top: Vec<&str> = cfg.lines().collect();
    let at = |key: &str| top.iter().position(|l| *l == key).expect(key);
    assert!(
        top[at("suggests:") + 1].contains("\"postgresql\""),
        "the deb suggests postgresql: {cfg}"
    );
    assert!(
        cfg.contains("    suggests:\n      - \"postgresql-server\"\n"),
        "the rpm suggests postgresql-server: {cfg}"
    );
    assert!(!cfg.contains("recommends"), "{cfg}");
}

/// The slice is the one non-`.service` unit; any other name is still refused.
#[test]
fn a_unit_that_is_neither_a_service_nor_the_slice_is_refused() {
    let t = Tree::new("slice");
    t.put("units/kbf.slice", "[Slice]");
    assert!(plan(&t.inputs()).is_ok(), "the slice is packaged");
    t.put("units/kbf-x.timer", "[Timer]");
    let got = refused(plan(&t.inputs()));
    assert!(got.contains("kbf-x.timer"), "{got}");
    assert!(!got.contains("kbf.slice:"), "{got}");
}

/// A `kbf-*.socket` is packaged beside its service, in the same directory and mode.
#[test]
fn a_socket_unit_is_packaged_beside_the_services_with_the_same_mode() {
    let t = Tree::new("socket");
    t.put("units/kbf-x.socket", "[Socket]");
    t.put("units/other.socket", "[Socket]");
    let got = refused(plan(&t.inputs()));
    assert!(
        got.contains("other.socket: a package unit is named"),
        "{got}"
    );
    assert!(!got.contains("kbf-x.socket:"), "{got}");
    std::fs::remove_file(t.dir.join("units/other.socket")).expect("rm");
    let entries = plan(&t.inputs()).expect("a socket is a plan");
    let find = |n: &str| {
        entries
            .iter()
            .find(|e| e.dst.ends_with(&format!("/{n}")))
            .map(|e| (e.dst.clone(), e.mode))
            .expect(n)
    };
    let (sock, sock_mode) = find("kbf-x.socket");
    let (svc, svc_mode) = find("kbf-node.service");
    assert_eq!(sock_mode, svc_mode);
    assert_eq!(
        sock.rsplit_once('/').map(|p| p.0),
        svc.rsplit_once('/').map(|p| p.0)
    );
}

#[test]
fn unit_accounts_reads_user_and_group_lines_only() {
    let text = "# User=commented\n[Service]\nUser=kbf-server\nGroup=kbf\nUser=\nExecStart=/bin/x User=no\nSupplementaryGroups=other\n";
    assert_eq!(
        unit_accounts(text),
        [
            ("User", "kbf-server".to_owned()),
            ("Group", "kbf".to_owned())
        ]
    );
}

#[test]
fn every_user_and_group_a_packaged_unit_names_is_made_by_sysusers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let mut seen = 0;
    for e in std::fs::read_dir(root.join("units")).expect("units") {
        let path = e.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("unit");
        for (_, account) in unit_accounts(&text) {
            seen += 1;
            assert!(
                USERS.contains(&account.as_str()),
                "{}: {account}",
                path.display()
            );
        }
    }
    assert!(seen > 0);
}

/// The unit-name pattern of the needrestart drop-in's override line, `^kbf[-.@]`, read
/// as the plain check it is: a literal prefix, then one character of a class.
fn needrestart_pattern(text: &str) -> (String, Vec<char>) {
    let line = text
        .lines()
        .find(|l| l.starts_with("$nrconf{override_rc}{qr("))
        .expect("an override_rc line");
    assert!(line.ends_with(")} = 0;"), "{line:?}");
    let pattern = line
        .strip_prefix("$nrconf{override_rc}{qr(^")
        .and_then(|r| r.strip_suffix(")} = 0;"))
        .expect("an anchored qr()");
    let (prefix, class) = pattern.split_once('[').expect("a character class");
    let class = class.strip_suffix(']').expect("a closed class");
    (prefix.to_owned(), class.chars().collect())
}

fn needrestart_skips(text: &str, unit: &str) -> bool {
    let (prefix, class) = needrestart_pattern(text);
    unit.strip_prefix(prefix.as_str())
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| class.contains(&c))
}

#[test]
fn the_package_holds_the_needrestart_drop_in_as_a_0644_config_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let plan = plan(&Inputs {
        binary: &root.join("units/kbf-installer.service"),
        kbf: &root.join("units/kbf-installer.service"),
        units: &root.join("units"),
        trust: &root.join("testdata/trust"),
        sysusers: &root.join("sysusers.d/kbf-installer.conf"),
        tmpfiles: &root.join("tmpfiles.d/kbf-installer.conf"),
        needrestart: &root.join("needrestart/kbf.conf"),
        platform: Platform::LinuxX86_64,
        version: "0.0.0+ci",
    })
    .expect("the real files are a plan");
    let e = plan
        .iter()
        .find(|e| e.dst == "/etc/needrestart/conf.d/kbf.conf")
        .expect("the drop-in is packaged");
    assert_eq!((e.mode, e.sha256.is_some()), (0o644, true));
    let yaml = config(Package::Installer, &plan, Platform::LinuxX86_64, "0.0.0+ci");
    let at = yaml
        .find("dst: \"/etc/needrestart/conf.d/kbf.conf\"")
        .expect("in the nfpm configuration");
    let block = yaml[at..].split("  - dst:").next().expect("block");
    assert!(block.contains("config|noreplace"), "{block}");
    assert!(block.contains("mode: 0644"), "{block}");
}

#[test]
fn the_needrestart_override_matches_every_packaged_unit_and_a_templated_instance() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let text = std::fs::read_to_string(root.join("needrestart/kbf.conf")).expect("drop-in");
    let mut names: Vec<String> = std::fs::read_dir(root.join("units"))
        .expect("units")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.is_empty());
    names.push("kbf-frontend@a.service".to_owned());
    for name in names {
        assert!(needrestart_skips(&text, &name), "{name} is not skipped");
    }
}

#[test]
fn the_needrestart_override_leaves_other_units_to_needrestart() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let text = std::fs::read_to_string(root.join("needrestart/kbf.conf")).expect("drop-in");
    for name in [
        "ssh.service",
        "kbfx.service",
        "postgresql.service",
        "my-kbf-a.service",
    ] {
        assert!(!needrestart_skips(&text, name), "{name} is skipped");
    }
    assert!(
        text.lines()
            .any(|l| l == "$nrconf{override_rc}{qr(^kbf[-.@])} = 0;"),
        "the override sets the restart choice to 0 (never)"
    );
}

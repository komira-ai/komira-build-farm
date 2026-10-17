//! The `linux` kit: the example settings render units that pass every rule, each
//! planted defect in a template turns the rule meant for it red, and each bad
//! setting is refused before anything is rendered.

use super::check::{Report, check_texts};
use super::config::{LinuxConfig, LinuxError};
use super::render::{Templates, Units, render};
use super::{Refusal, check_units, render_units};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");
const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn img() -> String {
    format!("gcr.io/distroless/base-debian12@{D1}")
}

/// `config.example` with each `KEY=value` line replaced (the key must be there).
fn example_with(settings: &[(&str, &str)]) -> String {
    let mut text = EXAMPLE.to_owned();
    for (key, value) in settings {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap_or_else(|| panic!("config.example has no {key}= line"))
            .to_owned();
        text = text.replace(&line, &format!("{key}={value}"));
    }
    text
}

fn config(settings: &[(&str, &str)]) -> LinuxConfig {
    LinuxConfig::parse(&example_with(settings)).expect("valid settings")
}

fn rendered(c: &LinuxConfig, t: &Templates<'_>) -> Units {
    render(c, Some(1500), t).expect("renders")
}

fn checked(c: &LinuxConfig, u: &Units, installable: bool) -> Report {
    check_texts(c, u.files(), installable).expect("units parse")
}

fn lines(r: &Report) -> Vec<String> {
    r.failures.iter().map(ToString::to_string).collect()
}

/// Which template a planted defect edits.
#[derive(Clone, Copy, Debug)]
enum Which {
    Slice,
    Minio,
    Server,
    Daemon,
}

/// The built-in templates with `from` replaced by `to` in one of them; `from` must occur
/// exactly once, so a row whose edit no longer applies fails instead of passing on
/// unchanged templates.
fn planted(which: Which, from: &str, to: &str) -> [String; 4] {
    let t = Templates::BUILT_IN;
    let mut texts = [t.slice, t.minio, t.server, t.daemon].map(str::to_owned);
    let i = which as usize;
    assert_eq!(
        texts[i].matches(from).count(),
        1,
        "{which:?}: {from:?} is not in the template once: the defect is not planted"
    );
    texts[i] = texts[i].replace(from, to);
    texts
}

fn templates(texts: &[String; 4]) -> Templates<'_> {
    Templates {
        slice: &texts[0],
        minio: &texts[1],
        server: &texts[2],
        daemon: &texts[3],
    }
}

#[test]
fn the_example_settings_render_units_that_keep_every_priority() {
    // Catches: a template or rule that disagrees with the other on the example host
    // (every planted row below would then be red for the wrong reason), and a memory
    // split computed some other way than RAM - MinIO - kbf-server - OS (256 - 32 - 16
    // - 8 = 200) or a slice minimum not their sum plus the daemon's (49152 + 256).
    let c = config(&[]);
    let u = rendered(&c, &Templates::BUILT_IN);
    let r = checked(&c, &u, true);
    assert_eq!(lines(&r), Vec::<String>::new());
    assert_eq!(
        r.summary,
        "UNITS OK: MinIO -950 < kbf-server -925 < kbf-daemon -900 < containers 0; \
         swap off for MinIO and kbf-server; builds get 200 GiB"
    );
    assert!(u.slice.contains("\nMemoryMin=49408M\n"), "{}", u.slice);
    assert!(u.minio.contains("\nMemoryMin=32G\n"), "{}", u.minio);
    let daemon = &u.daemon;
    for want in [
        "\nUser=kbf\n",
        "\nEnvironment=XDG_RUNTIME_DIR=/run/user/1500\n",
        "\nExecStart=/usr/local/bin/kbf-daemon \\\n",
        "    --server=https://storage-node.example.internal:8981 \\\n",
        "    --data-dir=/var/lib/kbf \\\n    --actions-memory-max-gib=200 \\\n",
        "    --supervisor-memory-min-mib=256\n",
    ] {
        assert!(daemon.contains(want), "no {want:?} in\n{daemon}");
    }
    let server = &u.server;
    for want in [
        "\nUser=kbf-server\n",
        "\nAfter=network-online.target minio.service\n",
        "    --cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000&conditional-put=true \\\n    --meta-dir=/var/lib/kbf-server/raft \\\n    --listen=192.0.2.10:8980 \\\n",
        "    --reapi-tls-cert=/etc/kbf/reapi.pem \\\n",
        "    --api-listen=127.0.0.1:8982 \\\n",
    ] {
        assert!(server.contains(want), "no {want:?} in\n{server}");
    }
    assert_eq!(
        u.files().map(|(name, _)| name),
        [
            "kbf.slice",
            "minio.service.d/kbf.conf",
            "kbf-server.service",
            "kbf-daemon.service"
        ]
    );
}

/// One planted defect: the edit, the settings it is checked with, and the line the
/// check must print (a `FAIL` line, or the `UNITS OK` summary for a row that must pass).
/// A row edits a template, or with `rendered` the rendered unit (a template may not
/// hold an `@` that is no placeholder, so an image reference is planted there).
struct Row {
    name: &'static str,
    which: Which,
    rendered: bool,
    from: String,
    to: String,
    settings: Vec<(&'static str, String)>,
    want: String,
}

fn row(name: &'static str, which: Which, from: &str, to: &str, want: &str) -> Row {
    Row {
        name,
        which,
        rendered: false,
        from: from.to_owned(),
        to: to.to_owned(),
        settings: Vec::new(),
        want: want.to_owned(),
    }
}

fn rows() -> Vec<Row> {
    use Which::{Daemon, Minio, Server, Slice};
    let swap = "MemorySwapMax=0\n";
    let with_image = |mut r: Row| {
        r.settings = vec![("KBF_IMAGES", img())];
        r.rendered = true;
        r
    };
    let pinned = format!("    --image=docker://{} \\\n", img());
    vec![
        // swap: a MinIO or server page in swap slows every build; a cap on the daemon
        // or the slice stops the leases (in the daemon's cgroup) from swapping.
        row(
            "minio-swap-allowed",
            Minio,
            swap,
            "",
            "FAIL swap: MinIO's drop-in must set MemorySwapMax=0, has ''",
        ),
        row(
            "server-swap-allowed",
            Server,
            swap,
            "",
            "FAIL swap: kbf-server must set MemorySwapMax=0, has ''",
        ),
        row(
            "daemon-swap-off",
            Daemon,
            "CPUWeight=200\n",
            "CPUWeight=200\nMemorySwapMax=0\n",
            "FAIL swap: kbf-daemon sets MemorySwapMax=",
        ),
        row(
            "slice-swap-off",
            Slice,
            "MemoryMin=@SLICE_MEMORY_MIN@\n",
            "MemoryMin=@SLICE_MEMORY_MIN@\nMemorySwapMax=0\n",
            "FAIL swap: kbf.slice sets MemorySwapMax=",
        ),
        // oom: a host-wide OOM kills the highest score first.
        row(
            "server-oom-above-daemon",
            Server,
            "OOMScoreAdjust=-925\n",
            "OOMScoreAdjust=-850\n",
            "FAIL oom: kbf-server's OOMScoreAdjust=-850 must be below kbf-daemon's -900",
        ),
        row(
            "minio-oom-above-server",
            Minio,
            "OOMScoreAdjust=-950\n",
            "OOMScoreAdjust=-900\n",
            "FAIL oom: MinIO's OOMScoreAdjust=-900 must be below kbf-server's -925",
        ),
        row(
            "minio-oom-below-range",
            Minio,
            "OOMScoreAdjust=-950\n",
            "OOMScoreAdjust=-1001\n",
            "FAIL oom: MinIO's OOMScoreAdjust=-1001 is below -1000",
        ),
        // The floor itself is allowed (`<` tightened to `<=` would refuse it).
        row(
            "minio-oom-at-floor",
            Minio,
            "OOMScoreAdjust=-950\n",
            "OOMScoreAdjust=-1000\n",
            "UNITS OK: MinIO -1000 < kbf-server -925",
        ),
        // Ties: two services at one score are killed in no defined order.
        row(
            "minio-oom-tied-server",
            Minio,
            "OOMScoreAdjust=-950\n",
            "OOMScoreAdjust=-925\n",
            "FAIL oom: MinIO's OOMScoreAdjust=-925 must be below kbf-server's -925",
        ),
        row(
            "server-oom-tied-daemon",
            Server,
            "OOMScoreAdjust=-925\n",
            "OOMScoreAdjust=-900\n",
            "FAIL oom: kbf-server's OOMScoreAdjust=-900 must be below kbf-daemon's -900",
        ),
        row(
            "daemon-oom-above-containers",
            Daemon,
            "OOMScoreAdjust=-900\n",
            "OOMScoreAdjust=100\n",
            "FAIL oom: kbf-daemon's OOMScoreAdjust=100 must be below the build containers' 0",
        ),
        // The tie: a daemon at 0 is as likely a victim as its builds (`<` loosened to `<=`).
        row(
            "daemon-oom-tied-containers",
            Daemon,
            "OOMScoreAdjust=-900\n",
            "OOMScoreAdjust=0\n",
            "FAIL oom: kbf-daemon's OOMScoreAdjust=0 must be below the build containers' 0",
        ),
        // systemd uses a key's last assignment: a check reading the first misses this...
        row(
            "server-oom-last-wins",
            Server,
            "OOMScoreAdjust=-925\n",
            "OOMScoreAdjust=-925\nOOMScoreAdjust=-850\n",
            "FAIL oom: kbf-server's OOMScoreAdjust=-850 must be below kbf-daemon's -900",
        ),
        // ... and one failing on any assignment refuses this, which systemd overrides.
        row(
            "server-oom-overridden",
            Server,
            "OOMScoreAdjust=-925\n",
            "OOMScoreAdjust=-850\nOOMScoreAdjust=-925\n",
            "UNITS OK: MinIO -950 < kbf-server -925",
        ),
        row(
            "server-oom-missing",
            Server,
            "OOMScoreAdjust=-925\n",
            "",
            "FAIL oom: every unit needs an integer OOMScoreAdjust= (MinIO '-950', kbf-server 'bad', kbf-daemon '-900')",
        ),
        // cpu
        row(
            "server-cpu-above-minio",
            Server,
            "CPUWeight=300\n",
            "CPUWeight=500\n",
            "FAIL cpu: MinIO's CPUWeight=400 must be above kbf-server's 500",
        ),
        row(
            "server-cpu-tied-minio",
            Server,
            "CPUWeight=300\n",
            "CPUWeight=400\n",
            "FAIL cpu: MinIO's CPUWeight=400 must be above kbf-server's 400",
        ),
        row(
            "daemon-cpu-above-server",
            Daemon,
            "CPUWeight=200\n",
            "CPUWeight=300\n",
            "FAIL cpu: kbf-server's CPUWeight=300 must be above kbf-daemon's 300",
        ),
        row(
            "daemon-cpu-missing",
            Daemon,
            "CPUWeight=200\n",
            "CPUWeight=high\n",
            "FAIL cpu: every unit needs an integer CPUWeight= (MinIO '400', kbf-server '300', kbf-daemon 'bad')",
        ),
        // min: the kernel caps a unit's protection at its slice's.
        row(
            "slice-min-not-sum",
            Slice,
            "MemoryMin=@SLICE_MEMORY_MIN@\n",
            "MemoryMin=48G\n",
            "FAIL min: kbf.slice's MemoryMin= is 49152 MiB, not the sum of its units' 49408 MiB",
        ),
        row(
            "slice-min-in-bytes",
            Slice,
            "MemoryMin=@SLICE_MEMORY_MIN@\n",
            "MemoryMin=51808043008\n",
            "UNITS OK",
        ),
        row(
            "slice-min-in-kib",
            Slice,
            "MemoryMin=@SLICE_MEMORY_MIN@\n",
            "MemoryMin=50593792K\n",
            "UNITS OK",
        ),
        row(
            "minio-min-in-tib",
            Minio,
            "MemoryMin=@MINIO_MEMORY_MIN@\n",
            "MemoryMin=1T\n",
            "FAIL min: MinIO's MemoryMin= is 1048576 MiB, not its reservation of 32 GiB",
        ),
        row(
            "server-min-missing",
            Server,
            "MemoryMin=@SERVER_MEMORY_MIN@\n",
            "",
            "FAIL min: kbf-server's MemoryMin= is bad MiB, not its reservation of 16 GiB",
        ),
        row(
            "daemon-min-not-configured",
            Daemon,
            "MemoryMin=@DAEMON_MEMORY_MIN@\n",
            "MemoryMin=512M\n",
            "FAIL min: kbf-daemon's MemoryMin= is 512 MiB, not 256",
        ),
        // slice
        row(
            "daemon-not-in-slice",
            Daemon,
            "Slice=kbf.slice\n",
            "",
            "FAIL slice: kbf-daemon.service is not in kbf.slice",
        ),
        row(
            "minio-not-in-slice",
            Minio,
            "Slice=kbf.slice\n",
            "Slice=system.slice\n",
            "FAIL slice: minio.service.d/kbf.conf is not in kbf.slice",
        ),
        row(
            "daemon-not-delegated",
            Daemon,
            "Delegate=yes\n",
            "",
            "FAIL slice: kbf-daemon needs Delegate=yes",
        ),
        // caps: the cap on builds is actions/memory.max; CPU is not capped.
        row(
            "daemon-memory-max",
            Daemon,
            "CPUWeight=200\n",
            "CPUWeight=200\nMemoryMax=64G\n",
            "FAIL caps: kbf-daemon.service sets MemoryMax=",
        ),
        row(
            "slice-cpu-quota",
            Slice,
            "[Slice]\n",
            "[Slice]\nCPUQuota=50%\n",
            "FAIL caps: kbf.slice sets CPUQuota=",
        ),
        // actions
        row(
            "actions-headroom-only",
            Daemon,
            "@ACTIONS_MEMORY_MAX_GIB@",
            "248",
            "FAIL actions: --actions-memory-max-gib=248, want 200 (RAM 256 - MinIO 32 - kbf-server 16 - OS 8)",
        ),
        row(
            "supervisor-min-mismatch",
            Daemon,
            "--supervisor-memory-min-mib=@KBF_DAEMON_MEMORY_MIN_MIB@",
            "--supervisor-memory-min-mib=512",
            "FAIL actions: --supervisor-memory-min-mib=512, but the daemon's MemoryMin= is 256 MiB",
        ),
        // server
        row(
            "server-key-in-env",
            Server,
            "EnvironmentFile=@KBF_ETC@/s3.env",
            "Environment=AWS_SECRET_ACCESS_KEY=x",
            "FAIL server: kbf-server has no EnvironmentFile= for the S3 key pair",
        ),
        row(
            "server-key-in-env-named",
            Server,
            "EnvironmentFile=@KBF_ETC@/s3.env",
            "Environment=AWS_SECRET_ACCESS_KEY=x",
            "FAIL server: kbf-server sets an AWS_ value in Environment=",
        ),
        row(
            "server-key-on-argv",
            Server,
            "    --meta-dir=@KBF_META_DIR@ \\\n",
            "    --meta-dir=@KBF_META_DIR@ \\\n    --x=AWS_SECRET_ACCESS_KEY \\\n",
            "FAIL server: kbf-server's command line names an AWS_ variable",
        ),
        row(
            "server-plain-reapi",
            Server,
            "    --reapi-tls-key=@KBF_ETC@/reapi.key \\\n",
            "",
            "FAIL server: kbf-server needs --reapi-tls-cert and --reapi-tls-key",
        ),
        row(
            "server-plain-reapi-cert",
            Server,
            "    --reapi-tls-cert=@KBF_ETC@/reapi.pem \\\n",
            "",
            "FAIL server: kbf-server needs --reapi-tls-cert and --reapi-tls-key",
        ),
        row(
            "server-store-file",
            Server,
            "--cold-store=s3://",
            "--cold-store=file://",
            "FAIL server: kbf-server needs --cold-store=s3://... (MinIO)",
        ),
        row(
            "server-no-endpoint",
            Server,
            "?endpoint=@MINIO_S3_ENDPOINT@&",
            "?",
            "FAIL server: --cold-store has no endpoint= query",
        ),
        row(
            "server-meta-dir-dropped",
            Server,
            "    --meta-dir=@KBF_META_DIR@ \\\n",
            "",
            "FAIL server: kbf-server has no --meta-dir",
        ),
        // runtime: Podman named as the runtime, the runtime flags left out, and the name
        // in a comment of the unit.
        row(
            "daemon-runtime-podman",
            Daemon,
            "--container-runtime=oci",
            "--container-runtime=podman",
            "FAIL runtime: kbf-daemon.service passes --container-runtime=podman",
        ),
        row(
            "daemon-runtime-dropped",
            Daemon,
            "    --container-runtime=oci \\\n",
            "",
            "FAIL runtime: kbf-daemon.service does not pass --container-runtime=oci",
        ),
        row(
            "daemon-oci-runtime-dropped",
            Daemon,
            "    --oci-runtime=/usr/bin/crun \\\n",
            "",
            "FAIL runtime: kbf-daemon.service does not pass --oci-runtime",
        ),
        row(
            "daemon-names-podman",
            Daemon,
            "OOMScoreAdjust=-900\n",
            "OOMScoreAdjust=-900\n# rootless Podman\n",
            "FAIL runtime: kbf-daemon.service line ",
        ),
        // image: kbf-daemon exits 2 at start on a bare --image (the kit once rendered
        // bare form), and must run the images the settings pin.
        with_image(row(
            "daemon-image-no-scheme",
            Daemon,
            &pinned,
            &format!("    --image={} \\\n", img()),
            &format!(
                "FAIL image: kbf-daemon would exit 2 at start: container-image \"{}\" does not start with docker://",
                img()
            ),
        )),
        with_image(row(
            "daemon-image-unpinned",
            Daemon,
            &pinned,
            &format!("    --image=docker://gcr.io/x@{D1} \\\n{pinned}"),
            &format!(
                "FAIL image: kbf-daemon's --image values are 'docker://gcr.io/x@{D1} docker://{}', want 'docker://{}'",
                img(),
                img()
            ),
        )),
    ]
}

#[test]
fn each_planted_defect_turns_its_rule_red() {
    // Catches: a rule that no longer fires on the defect it is for (each row names
    // it), a rule that reads the first assignment where systemd reads the last, and a
    // row whose edit no longer applies to the templates (`planted` refuses it).
    let rows = rows();
    let mut wrong = Vec::new();
    for r in &rows {
        let settings: Vec<(&str, &str)> =
            r.settings.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let c = config(&settings);
        let u = if r.rendered {
            let mut u = rendered(&c, &Templates::BUILT_IN);
            assert_eq!(
                u.daemon.matches(r.from.as_str()).count(),
                1,
                "{}: not planted",
                r.name
            );
            u.daemon = u.daemon.replace(r.from.as_str(), &r.to);
            u
        } else {
            let texts = planted(r.which, &r.from, &r.to);
            render(&c, Some(1500), &templates(&texts)).expect("renders")
        };
        let report = checked(&c, &u, true);
        let got = lines(&report);
        let ok = if r.want.starts_with("UNITS OK") {
            got.is_empty() && report.summary.starts_with(&r.want)
        } else {
            got.iter().any(|l| l.starts_with(&r.want))
                && report.summary.starts_with("UNITS BROKEN: ")
        };
        if !ok {
            wrong.push(format!(
                "{}: want {:?}, got {got:?} / {}",
                r.name, r.want, report.summary
            ));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
    assert_eq!(rows.len(), 46, "rows");
}

#[test]
fn a_leftover_or_unknown_placeholder_is_refused() {
    // Catches: a template that gained a placeholder its kit sets no value for being
    // rendered with the placeholder left in, an `@` typo passed through, and a unit
    // that still holds `@KEY@` or `<kbf uid>` (rendered by an older kit, or by hand)
    // passing the check.
    let c = config(&[]);
    let refused = |which, from, to: &str| {
        let texts = planted(which, from, to);
        render(&c, Some(1500), &templates(&texts))
            .err()
            .map(|e| e.to_string())
    };
    assert_eq!(
        refused(Which::Daemon, "Restart=on-failure", "Restart=@KBF_RESTART@").as_deref(),
        Some("kbf-daemon.service:36: placeholder @KBF_RESTART@ has no value")
    );
    assert_eq!(
        refused(
            Which::Server,
            "    --reapi-tls-cert=@KBF_ETC@/",
            "    --reapi-tls-cert=@KBF_ETC/"
        )
        .as_deref(),
        Some("kbf-server.service:26: an @ that starts no @KEY@ placeholder")
    );
    assert_eq!(
        refused(
            Which::Daemon,
            "    --driver=container",
            "    --driver=container @DAEMON_IMAGE_ARGS@"
        )
        .as_deref(),
        Some(
            "kbf-daemon.service:23: @DAEMON_IMAGE_ARGS@ stands for whole lines and must be alone on its line"
        )
    );

    let mut u = rendered(&c, &Templates::BUILT_IN);
    u.daemon = u.daemon.replace("/run/user/1500", "/run/user/@KBF_UID@");
    let r = checked(&c, &u, false);
    assert_eq!(
        lines(&r),
        ["FAIL render: kbf-daemon.service keeps the placeholder @KBF_UID@"]
    );
    assert_eq!(r.summary, "UNITS BROKEN: 1 failed check(s)");

    let no_uid = render(&c, None, &Templates::BUILT_IN).expect("renders");
    assert!(no_uid.daemon.contains("/run/user/<kbf uid>\n"));
    assert_eq!(lines(&checked(&c, &no_uid, false)), Vec::<String>::new());
    assert_eq!(
        lines(&checked(&c, &no_uid, true)),
        ["FAIL render: kbf-daemon.service keeps a <...> placeholder: <kbf uid>"]
    );
}

#[test]
fn settings_change_the_units_as_documented() {
    // Catches: KBF_DATA_DIR and KBF_META_DIR not reaching the flags the units pass
    // (`--data-dir`, `--meta-dir`), the removed KBF_META_LOG still accepted (a settings
    // file written for the old units would run with a log it never asked for); an IPv6
    // listen address written unbracketed (`2001:db8::10:8980` is no socket address);
    // images rendered in the bare form or not at all; a defaulted key not taking
    // its default.
    let moved = config(&[("KBF_DATA_DIR", "/srv/kbf"), ("KBF_META_DIR", "/srv/meta")]);
    let u = rendered(&moved, &Templates::BUILT_IN);
    assert!(
        u.daemon.contains("    --data-dir=/srv/kbf \\\n"),
        "{}",
        u.daemon
    );
    assert!(
        u.server.contains("    --meta-dir=/srv/meta \\\n"),
        "{}",
        u.server
    );
    assert_eq!(lines(&checked(&moved, &u, true)), Vec::<String>::new());
    let old = format!("{EXAMPLE}KBF_META_LOG=raft\n");
    assert!(refusal(&old).contains("KBF_META_LOG"), "{}", refusal(&old));

    let v6 = config(&[("KBF_LISTEN_ADDRESS", "2001:db8::10")]);
    let u = rendered(&v6, &Templates::BUILT_IN);
    assert!(
        u.server.contains("    --listen=[2001:db8::10]:8980 \\\n"),
        "{}",
        u.server
    );
    assert!(
        u.server
            .contains("    --worker-listen=[2001:db8::10]:8981 \\\n")
    );
    assert_eq!(lines(&checked(&v6, &u, true)), Vec::<String>::new());

    let two = format!("{} registry.example:5000/t/tool@{D1}", img());
    let images = config(&[("KBF_IMAGES", &two)]);
    let u = rendered(&images, &Templates::BUILT_IN);
    let want = format!(
        "    --data-dir=/var/lib/kbf \\\n    --image=docker://{} \\\n    --image=docker://registry.example:5000/t/tool@{D1} \\\n    --actions-memory-max-gib=200 \\\n",
        img()
    );
    assert!(u.daemon.contains(&want), "{}", u.daemon);
    assert_eq!(lines(&checked(&images, &u, true)), Vec::<String>::new());

    let defaults = config(&[
        ("KBF_REAPI_PORT", ""),
        ("KBF_ETC", "\"\""),
        ("MINIO_UNIT", "minio-kbf.service"),
    ]);
    assert_eq!(defaults.reapi_port, 8980);
    assert_eq!(defaults.etc.as_str(), "/etc/kbf");
    let u = rendered(&defaults, &Templates::BUILT_IN);
    assert_eq!(u.files()[1].0, "minio-kbf.service.d/kbf.conf");
    assert!(
        u.minio
            .contains("/etc/systemd/system/minio-kbf.service.d/kbf.conf.")
    );
}

#[test]
fn a_meta_dir_under_the_data_dir_fails_the_check() {
    // Catches: the server's log on the partition builds fill (a full data directory
    // would stop the server), and a prefix match that takes `/var/lib/kbf2/raft` as
    // inside `/var/lib/kbf`.
    let inside = config(&[("KBF_META_DIR", "/var/lib/kbf/raft")]);
    let r = checked(&inside, &rendered(&inside, &Templates::BUILT_IN), true);
    assert_eq!(
        lines(&r),
        [
            "FAIL server: --meta-dir=/var/lib/kbf/raft is under the daemon's data directory /var/lib/kbf"
        ]
    );
    let beside = config(&[("KBF_META_DIR", "/var/lib/kbf2/raft")]);
    let r = checked(&beside, &rendered(&beside, &Templates::BUILT_IN), true);
    assert_eq!(lines(&r), Vec::<String>::new());
}

fn refusal(text: &str) -> String {
    LinuxConfig::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn bad_settings_are_refused_before_anything_renders() {
    // Catches, one row each: a host with nothing left for builds (or exactly 0), too
    // little left to the OS, a listener on every interface, a host value left unset,
    // a typo'd or repeated key, a character systemd would expand or a unit cannot
    // carry, an image by tag or in the daemon's docker:// form, a number or port that
    // is not one, two listeners on one port (the first pair, the last pair, the first
    // and the last), a relative path, and a MinIO unit that is not a service.
    let tag = "gcr.io/distroless/base-debian12:latest";
    let tag_digest = format!("gcr.io/distroless/base-debian12:nonroot@{D1}");
    let docker = format!("docker://{}", img());
    let cases: Vec<(String, &str)> = vec![
        (
            example_with(&[("KBF_RAM_GIB", "56")]),
            "KBF_RAM_GIB=56 leaves 0 GiB for builds after MinIO 32, kbf-server 16 and the OS 8",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "8")]),
            "KBF_RAM_GIB=8 leaves -48 GiB for builds",
        ),
        (
            example_with(&[("KBF_OS_HEADROOM_GIB", "3")]),
            "KBF_OS_HEADROOM_GIB=3: keep at least 4 GiB for the OS",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "0.0.0.0")]),
            "KBF_LISTEN_ADDRESS=0.0.0.0: bind the host's pinned address, not every interface",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "::")]),
            "KBF_LISTEN_ADDRESS=::: bind the host's pinned address",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "[::]")]),
            "KBF_LISTEN_ADDRESS holds a character the units cannot carry: [::]",
        ),
        (
            example_with(&[("KBF_HOST_NAME", "")]),
            "KBF_HOST_NAME is not set (config file)",
        ),
        (
            example_with(&[("KBF_NODE_ID", "")]),
            "KBF_NODE_ID is not set (config file)",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "")]),
            "KBF_RAM_GIB is not set (config file)",
        ),
        (
            format!("{EXAMPLE}KBF_RAM=1\n"),
            "line 108: unknown key KBF_RAM",
        ),
        (
            format!("{EXAMPLE}KBF_RAM_GIB=1\n"),
            "line 108: KBF_RAM_GIB is already set on line 24",
        ),
        (
            example_with(&[("KBF_NODE_ID", "a;b")]),
            "KBF_NODE_ID holds a character the units cannot carry: a;b",
        ),
        (
            example_with(&[("KBF_ETC", "/etc/%h")]),
            "KBF_ETC holds a character the units cannot carry: /etc/%h",
        ),
        (
            example_with(&[("KBF_USER", "$USER")]),
            "KBF_USER holds a character the units cannot carry: $USER",
        ),
        (example_with(&[("KBF_IMAGES", tag)]), "names no digest"),
        (
            example_with(&[("KBF_IMAGES", &tag_digest)]),
            "names a tag; name an image by digest only",
        ),
        (
            example_with(&[("KBF_IMAGES", &docker)]),
            "write the bare <repo>@sha256:<digest>",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "+256")]),
            "KBF_RAM_GIB must be a whole number, not '+256'",
        ),
        (
            example_with(&[("MINIO_RESERVATION_GIB", "32G")]),
            "MINIO_RESERVATION_GIB must be a whole number, not '32G'",
        ),
        (
            example_with(&[("KBF_REAPI_PORT", "0")]),
            "KBF_REAPI_PORT must be a port number from 1 to 65535, not '0'",
        ),
        (
            example_with(&[("KBF_API_PORT", "65536")]),
            "KBF_API_PORT must be a port number from 1 to 65535, not '65536'",
        ),
        (
            example_with(&[("KBF_WORKER_PORT", "+81")]),
            "KBF_WORKER_PORT must be a port number from 1 to 65535, not '+81'",
        ),
        (
            example_with(&[("KBF_ETC", "etc/kbf")]),
            "KBF_ETC: \"etc/kbf\" is not absolute",
        ),
        (
            example_with(&[("KBF_META_DIR", "/var/lib/../raft")]),
            "KBF_META_DIR: \"/var/lib/../raft\" has an empty",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "0")]),
            "KBF_STORAGE_PORT must be a port number from 1 to 65535, not '0'",
        ),
        (
            example_with(&[("KBF_HOT_DIR", "hot")]),
            "KBF_HOT_DIR: \"hot\" is not absolute",
        ),
        (
            example_with(&[("KBF_REAPI_PORT", "8981")]),
            "KBF_REAPI_PORT and KBF_WORKER_PORT are both 8981: each listener of a host needs its own port",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "8982")]),
            "KBF_API_PORT and KBF_STORAGE_PORT are both 8982",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "8980")]),
            "KBF_REAPI_PORT and KBF_STORAGE_PORT are both 8980",
        ),
        (
            example_with(&[("MINIO_UNIT", "minio")]),
            "MINIO_UNIT=minio is not a .service unit name",
        ),
        (
            example_with(&[("MINIO_UNIT", ".service")]),
            "MINIO_UNIT=.service is not a .service unit name",
        ),
    ];
    let mut wrong = Vec::new();
    for (text, want) in &cases {
        let got = refusal(text);
        if !got.contains(want) {
            wrong.push(format!("want {want:?}, got {got:?}"));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
    // The least that renders: one GiB for builds.
    assert_eq!(config(&[("KBF_RAM_GIB", "57")]).actions_gib, 1);
    assert_eq!(config(&[("KBF_OS_HEADROOM_GIB", "4")]).actions_gib, 204);
}

#[test]
fn the_verbs_write_into_the_directory_and_read_it_back() {
    // Catches: render-units writing somewhere other than --out (or not making the
    // drop-in directory), check-units reading other files than render-units wrote,
    // and a missing file or garbled unit checked as if it were empty.
    let dir = kbf_host::testkit::scratch("linux_verbs");
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("write config");
    let out = dir.join("units");
    let line = render_units(&cfg, &out, Some(1500)).expect("renders");
    assert_eq!(
        line,
        format!(
            "rendered kbf.slice minio.service.d/kbf.conf kbf-server.service kbf-daemon.service into {} (builds get 200 GiB)",
            out.display()
        )
    );
    let mut names: Vec<_> = walk(&out);
    names.sort();
    assert_eq!(
        names,
        [
            "kbf-daemon.service",
            "kbf-server.service",
            "kbf.slice",
            "minio.service.d/kbf.conf"
        ]
    );
    let r = check_units(&cfg, &out, true).expect("checks");
    assert_eq!(
        (r.failures.len(), r.summary.starts_with("UNITS OK")),
        (0, true)
    );

    std::fs::write(out.join("kbf-server.service"), "[Service]\nMemoryMin 1G\n").expect("garble");
    let e = check_units(&cfg, &out, true).err().map(|e| e.to_string());
    assert_eq!(
        e.as_deref(),
        Some("kbf-server.service: line 2: not [Section], a comment or Key=value")
    );
    std::fs::remove_file(out.join("kbf.slice")).expect("remove");
    let e = check_units(&cfg, &out, true).err().map(|e| e.to_string());
    assert!(
        e.as_deref()
            .is_some_and(|e| e.starts_with("cannot read ") && e.contains("kbf.slice")),
        "{e:?}"
    );

    let missing = check_units(&dir.join("nope"), &out, true).err();
    assert!(matches!(
        missing,
        Some(Refusal::Config(LinuxError::Config(_)))
    ));
    let blocked = dir.join("file");
    std::fs::write(&blocked, "").expect("write");
    let e = render_units(&cfg, &blocked, None).err();
    assert!(matches!(e, Some(Refusal::Write { .. })), "{e:?}");
    // A directory where a unit file goes: the drop-in directory is made, the write fails.
    std::fs::create_dir_all(dir.join("taken/kbf.slice")).expect("dir in the way");
    let e = render_units(&cfg, &dir.join("taken"), None).err();
    assert!(matches!(e, Some(Refusal::Write { .. })), "{e:?}");
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            out.extend(walk(&path).into_iter().map(|n| format!("{name}/{n}")));
        } else {
            out.push(name);
        }
    }
    out
}

#[test]
fn an_unset_key_takes_the_default_config_example_shows() {
    // Catches: a default that differs from what config.example documents (a host
    // configured by leaving keys out would get other units than the example says), and
    // an unset key read as empty rather than defaulted.
    let host = "KBF_HOST_NAME=storage-node.example.internal\nKBF_LISTEN_ADDRESS=192.0.2.10\n\
                KBF_NODE_ID=storage-node-1\nKBF_RAM_GIB=256\n";
    let minimal = LinuxConfig::parse(host).expect("the four host values are enough");
    // MINIO_S3_ENDPOINT alone has no default in the settings: the converged units take
    // the host's own MinIO, and `--mode storage` refuses (modes_tests).
    assert_eq!(minimal.minio_s3_endpoint, None);
    let example = config(&[]);
    assert_eq!(
        example.minio_s3_endpoint.as_deref(),
        Some(super::config::LOOPBACK_MINIO)
    );
    assert_eq!(
        LinuxConfig {
            minio_s3_endpoint: example.minio_s3_endpoint.clone(),
            // The example names another frontend and a storage node to call; left out,
            // the frontend is this host and there is no storage URL.
            frontend_name: example.frontend_name.clone(),
            storage_url: example.storage_url.clone(),
            ..minimal.clone()
        },
        example
    );
    assert_eq!(
        rendered(&minimal, &Templates::BUILT_IN),
        rendered(&example, &Templates::BUILT_IN)
    );
}

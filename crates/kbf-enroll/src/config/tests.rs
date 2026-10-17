//! Every row of the design's table 4.1, accepted and refused. Each refusal names the key
//! and the line; a file with several faults reports all of them together.

use std::os::unix::fs::{PermissionsExt, symlink};

use kbf_types::{Role, RoleSet};

use super::*;

fn host() -> Host {
    Host {
        hostname: "Node-02.farm.example.com".to_owned(),
        cpus: 8,
    }
}

fn ok(text: &str) -> Config {
    Config::parse(text, &host()).unwrap_or_else(|e| panic!("refused: {e}"))
}

fn problems(text: &str) -> Vec<Problem> {
    Config::parse(text, &host()).expect_err("must be refused").0
}

/// The one problem of `text`, as `key@line: message`.
fn one(text: &str) -> String {
    let all = problems(text);
    assert_eq!(all.len(), 1, "{all:?}");
    let p = &all[0];
    format!("{}@{}: {}", p.key, p.line.map_or(0, |l| l), p.message)
}

const MINIMAL: &str = "[frontend]\nurl = \"https://build.example.com:8990\"\n";

#[test]
fn the_two_required_values_make_a_config_with_every_default() {
    let c = ok(MINIMAL);
    assert_eq!(c.frontend.to_url(), "https://build.example.com:8990");
    assert_eq!(c.token_file, PathBuf::from(DEFAULT_TOKEN_FILE));
    assert_eq!(
        c.node_id.as_str(),
        "node-02",
        "the short host name, lower-cased"
    );
    assert_eq!(c.advertise, None);
    assert_eq!(c.zone, "");
    assert_eq!(c.state_dir, PathBuf::from(DEFAULT_STATE_DIR));
    assert_eq!(c.hot_dir, Path::new(DEFAULT_STATE_DIR).join("hot"));
    assert_eq!(c.meta_dir, Path::new(DEFAULT_STATE_DIR).join("meta"));
    assert_eq!(c.data_dir, Path::new(DEFAULT_STATE_DIR).join("data"));
    assert_eq!(c.reserve_cpus, None);
    assert_eq!(c.trust, Trust::Pin, "pin unless public is written down");
}

#[test]
fn every_field_written_is_read() {
    let c = ok(r#"
[frontend]
url = "https://Build.Example.com"
[enroll]
token_file = "/srv/kbf/token"
[node]
id = "n7"
advertise = "n7.farm.example.com:8983"
zone = "rack-a.1_x"
[dirs]
state = "/srv/kbf"
[storage]
hot_dir = "/srv/hot"
meta_dir = "/srv/meta"
[daemon]
data_dir = "/srv/data"
reserve_cpus = 7
[tls]
trust = "public"
"#);
    assert_eq!(c.frontend.to_url(), "https://build.example.com:443");
    assert_eq!(c.token_file, PathBuf::from("/srv/kbf/token"));
    assert_eq!(c.node_id.as_str(), "n7");
    let adv = c.advertise.expect("advertise");
    assert_eq!(
        (adv.host.to_string(), adv.port),
        ("n7.farm.example.com".to_owned(), 8983)
    );
    assert_eq!(c.zone, "rack-a.1_x");
    assert_eq!(c.state_dir, PathBuf::from("/srv/kbf"));
    assert_eq!(c.hot_dir, PathBuf::from("/srv/hot"));
    assert_eq!(c.meta_dir, PathBuf::from("/srv/meta"));
    assert_eq!(c.data_dir, PathBuf::from("/srv/data"));
    assert_eq!(c.reserve_cpus, Some(7));
    assert_eq!(c.trust, Trust::Public);
}

#[test]
fn state_dir_moves_the_defaults_of_the_directories_under_it() {
    let c = ok(&format!("{MINIMAL}[dirs]\nstate = \"/srv/kbf\"\n"));
    assert_eq!(c.hot_dir, PathBuf::from("/srv/kbf/hot"));
    assert_eq!(c.data_dir, PathBuf::from("/srv/kbf/data"));
}

#[test]
fn an_ip_frontend_is_read_with_pin_trust() {
    let c = ok("[frontend]\nurl = \"https://192.0.2.3:8990\"\n");
    assert_eq!(c.frontend.host, HostName::Ip("192.0.2.3".parse().unwrap()));
    let c = ok("[frontend]\nurl = \"https://[fd00::1]:9\"\n");
    assert_eq!(c.frontend.to_url(), "https://[fd00::1]:9");
    let c = ok("[frontend]\nurl = \"https://[fd00::1]\"\n");
    assert_eq!(c.frontend.port, 443);
}

#[test]
fn frontend_url_refusals_name_the_key_and_the_line() {
    let url = |u: &str| one(&format!("# comment\n[frontend]\nurl = \"{u}\"\n"));
    assert_eq!(
        url("http://a.example.com"),
        "frontend.url@3: the scheme must be exactly https"
    );
    assert_eq!(
        url("HTTPS://a.example.com"),
        "frontend.url@3: the scheme must be exactly https"
    );
    assert_eq!(
        url("a.example.com"),
        "frontend.url@3: the scheme must be exactly https"
    );
    assert_eq!(
        url("https://u:p@a.example.com"),
        "frontend.url@3: the URL must not carry user information"
    );
    for tail in ["/", "/x", "?q=1", "#f"] {
        assert_eq!(
            url(&format!("https://a.example.com{tail}")),
            "frontend.url@3: the URL must not carry a path, query or fragment"
        );
    }
    for port in ["0", "65536", "x", "", "+80", "-1"] {
        assert_eq!(
            url(&format!("https://a.example.com:{port}")),
            "frontend.url@3: the port must be a number from 1 to 65535",
            "port {port:?}"
        );
    }
    assert!(url("https://a_b.example.com").contains("is not a DNS name"));
    assert!(url("https://*.example.com").contains("is not a DNS name"));
    assert!(url("https://-a.example.com").contains("is not a DNS name"));
    assert!(url("https://a-.example.com").contains("is not a DNS name"));
    assert!(url("https://a..example.com").contains("is not a DNS name"));
    assert!(url("https://").contains("is not a DNS name"));
    assert!(url(&format!("https://{}.example.com", "a".repeat(64))).contains("is not a DNS name"));
    let long = format!("https://{}", vec!["a".repeat(60); 5].join("."));
    assert_eq!(
        url(&long),
        "frontend.url@3: the host is longer than 253 bytes"
    );
    assert!(url("https://[::1").contains("is not a DNS name"));
    assert!(url("https://[::1]x").contains("has text after the address"));
    assert!(url("https://[nope]:80").contains("is not an IPv6 address"));
}

#[test]
fn frontend_url_is_required() {
    let all = problems("[node]\nid = \"a\"\n");
    assert_eq!(
        all,
        vec![Problem {
            key: "frontend.url".to_owned(),
            line: None,
            message: "is required".to_owned()
        }]
    );
    assert_eq!(all[0].to_string(), "frontend.url: is required");
    assert_eq!(problems("[frontend]\n")[0].message, "is required");
    assert_eq!(problems("")[0].message, "is required");
}

#[test]
fn an_ip_frontend_with_public_trust_is_refused_at_the_url() {
    let text = "[frontend]\nurl = \"https://192.0.2.3\"\n[tls]\ntrust = \"public\"\n";
    assert_eq!(
        one(text),
        "frontend.url@2: an IP address host needs tls.trust = \"pin\""
    );
    // A DNS name with public trust is the point of the fallback.
    ok("[frontend]\nurl = \"https://a.example.com\"\n[tls]\ntrust = \"public\"\n");
}

#[test]
fn trust_is_pin_or_public() {
    assert_eq!(
        one(&format!("{MINIMAL}[tls]\ntrust = \"first-use\"\n")),
        "tls.trust@4: must be \"pin\" or \"public\""
    );
    assert_eq!(
        ok(&format!("{MINIMAL}[tls]\ntrust = \"pin\"\n")).trust,
        Trust::Pin
    );
}

#[test]
fn node_id_follows_the_pattern_and_the_default_is_refused_when_it_fails() {
    for id in ["", "A", "a_b", "-a", "a-", &"a".repeat(64)] {
        let got = one(&format!("{MINIMAL}[node]\nid = \"{id}\"\n"));
        assert!(got.starts_with("node.id@4: "), "{id:?}: {got}");
    }
    assert_eq!(
        ok(&format!("{MINIMAL}[node]\nid = \"a\"\n"))
            .node_id
            .as_str(),
        "a"
    );
    let bad_host = Host {
        hostname: "build_box.lan".to_owned(),
        cpus: 4,
    };
    let err = Config::parse(MINIMAL, &bad_host).expect_err("refused");
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].key, "node.id");
    assert_eq!(err.0[0].line, None);
    assert!(
        err.0[0].message.ends_with("; set node.id"),
        "{}",
        err.0[0].message
    );
    let set = format!("{MINIMAL}[node]\nid = \"build-box\"\n");
    assert_eq!(
        Config::parse(&set, &bad_host).unwrap().node_id.as_str(),
        "build-box"
    );
}

#[test]
fn advertise_is_host_port_and_not_loopback_wildcard_or_a_frontend_name() {
    let adv = |a: &str| one(&format!("{MINIMAL}[node]\nadvertise = \"{a}\"\n"));
    assert_eq!(
        adv("node-02.farm"),
        "node.advertise@4: the address must be host:port"
    );
    assert!(adv("node:0").contains("port must be"));
    assert!(adv("*.farm:80").contains("is not a DNS name"));
    for (a, kind) in [
        ("localhost:80", "loopback"),
        ("127.0.0.1:80", "loopback"),
        ("[::1]:80", "loopback"),
        ("0.0.0.0:80", "unspecified"),
        ("[::]:80", "unspecified"),
        (&format!("{}.0.0.1:80", 224), "multicast"),
        (&format!("169.254.{}.1:80", 1), "link-local"),
        ("[fe80::1]:80", "link-local"),
    ] {
        assert_eq!(
            adv(a),
            format!("node.advertise@4: the advertised host must not be {kind}"),
            "{a}"
        );
    }
    assert_eq!(
        adv("Build.Example.com:80"),
        "node.advertise@4: the advertised host must not be a name of the frontend"
    );
    let c = ok(&format!(
        "{MINIMAL}[node]\nadvertise = \"198.51.100.5:8983\"\n"
    ));
    assert_eq!(c.advertise.unwrap().host.to_string(), "198.51.100.5");
    let c = ok(&format!(
        "{MINIMAL}[node]\nadvertise = \"[fd00::5]:8983\"\n"
    ));
    assert_eq!(c.advertise.unwrap().host.to_string(), "[fd00::5]");
}

#[test]
fn zone_is_a_short_label() {
    assert!(one(&format!("{MINIMAL}[node]\nzone = \"Rack A\"\n")).starts_with("node.zone@4: "));
    let long = "a".repeat(64);
    assert!(one(&format!("{MINIMAL}[node]\nzone = \"{long}\"\n")).starts_with("node.zone@4: "));
    assert_eq!(
        ok(&format!("{MINIMAL}[node]\nzone = \"{}\"\n", "a".repeat(63)))
            .zone
            .len(),
        63
    );
}

#[test]
fn paths_are_absolute_and_directories_have_no_dotdot() {
    assert_eq!(
        one(&format!("{MINIMAL}[enroll]\ntoken_file = \"token\"\n")),
        "enroll.token_file@4: the path must be absolute"
    );
    for (table, key) in [
        ("dirs", "state"),
        ("storage", "hot_dir"),
        ("storage", "meta_dir"),
        ("daemon", "data_dir"),
    ] {
        let got = one(&format!("{MINIMAL}[{table}]\n{key} = \"/a/../b\"\n"));
        assert_eq!(
            got,
            format!("{table}.{key}@4: the path must not contain ..")
        );
        let got = one(&format!("{MINIMAL}[{table}]\n{key} = \"rel\"\n"));
        assert_eq!(got, format!("{table}.{key}@4: the path must be absolute"));
    }
    // A token file may be anywhere absolute, `..` included: it is checked as a file.
    ok(&format!("{MINIMAL}[enroll]\ntoken_file = \"/a/../t\"\n"));
}

#[test]
fn reserve_cpus_is_zero_to_cpus_minus_one() {
    let cpus = |n: i64| one(&format!("{MINIMAL}[daemon]\nreserve_cpus = {n}\n"));
    assert_eq!(
        cpus(8),
        "daemon.reserve_cpus@4: must be from 0 to 7 on a host with 8 CPUs"
    );
    assert_eq!(
        cpus(-1),
        "daemon.reserve_cpus@4: must be from 0 to 7 on a host with 8 CPUs"
    );
    assert_eq!(
        ok(&format!("{MINIMAL}[daemon]\nreserve_cpus = 0\n")).reserve_cpus,
        Some(0)
    );
    let one_cpu = Host {
        hostname: "n".to_owned(),
        cpus: 1,
    };
    let text = format!("{MINIMAL}[daemon]\nreserve_cpus = 1\n");
    assert!(Config::parse(&text, &one_cpu).is_err());
}

#[test]
fn a_typo_a_wrong_type_and_bad_toml_are_not_silently_ignored() {
    // Unknown key, in a known table and as a table.
    let got = one(&format!("{MINIMAL}[node]\nidd = \"a\"\n"));
    assert!(got.starts_with("idd@4: unknown field `idd`"), "{got}");
    let got = one(&format!("{MINIMAL}[nodes]\nid = \"a\"\n"));
    assert!(
        got.starts_with("@3: unknown field `nodes`") || got.contains("unknown field"),
        "{got}"
    );
    // Wrong type: the key is named from its line.
    let got = one(&format!("{MINIMAL}[daemon]\nreserve_cpus = \"two\"\n"));
    assert!(got.starts_with("reserve_cpus@4: "), "{got}");
    // Not TOML at all, and a repeated key (TOML itself refuses it).
    assert!(one("[frontend\nurl = 1").starts_with("@"));
    assert!(one(&format!("{MINIMAL}url = \"https://b.example.com\"\n")).contains("duplicate"));
}

#[test]
fn a_problem_prints_its_line_and_key_when_it_has_them() {
    let syntax = problems("[frontend\nurl = 1");
    assert_eq!(syntax[0].key, "", "a line with no key");
    assert!(
        syntax[0].to_string().starts_with("line 1: "),
        "{}",
        syntax[0]
    );
    let keyed = Problem {
        key: "node.id".to_owned(),
        line: Some(4),
        message: "bad".to_owned(),
    };
    assert_eq!(keyed.to_string(), "line 4: node.id: bad");
    let bare = Problem {
        key: String::new(),
        line: None,
        message: "bad".to_owned(),
    };
    assert_eq!(bare.to_string(), "bad");
}

#[test]
fn all_the_problems_are_reported_together() {
    let text = "[frontend]\nurl = \"http://x\"\n[node]\nid = \"A\"\nzone = \"Z\"\n[daemon]\nreserve_cpus = 99\n";
    let all = problems(text);
    let keys: Vec<_> = all.iter().map(|p| (p.key.as_str(), p.line)).collect();
    assert_eq!(
        keys,
        vec![
            ("frontend.url", Some(2)),
            ("node.id", Some(4)),
            ("node.zone", Some(5)),
            ("daemon.reserve_cpus", Some(7)),
        ]
    );
    let shown = Problems(all).to_string();
    assert!(shown.starts_with("line 2: frontend.url: "), "{shown}");
    assert_eq!(shown.matches("; ").count(), 3);
}

#[test]
fn warnings_name_the_tables_for_roles_the_frontend_did_not_grant() {
    let c = ok(&format!(
        "{MINIMAL}[node]\nadvertise = \"n.example.net:80\"\n[storage]\n[daemon]\n"
    ));
    let both = RoleSet::new([Role::Storage, Role::Daemon]).unwrap();
    assert!(c.role_warnings(both).is_empty());
    let daemon = RoleSet::new([Role::Daemon]).unwrap();
    let w = c.role_warnings(daemon);
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w[0].starts_with("[storage]") && w[1].starts_with("node.advertise"));
    let storage = RoleSet::new([Role::Storage]).unwrap();
    assert_eq!(
        c.role_warnings(storage),
        vec!["[daemon] is set but the frontend granted no daemon role"]
    );
    assert!(ok(MINIMAL).role_warnings(daemon).is_empty());
}

// ---- the file-system checks -------------------------------------------------------

fn config_for(state: &Path) -> Config {
    ok(&format!(
        "{MINIMAL}[dirs]\nstate = \"{}\"\n",
        state.display()
    ))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-enroll-config-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Resolve a symlinked temp dir (macOS) so the walk below sees real directories.
    std::fs::canonicalize(&dir).unwrap()
}

fn mkdir(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_missing_directory_is_not_a_problem_yet() {
    let root = scratch("missing");
    assert!(
        config_for(&root.join("nope"))
            .check_paths(rustix::process::geteuid().as_raw())
            .is_empty()
    );
}

#[test]
fn directories_must_be_owned_by_the_service_user_and_mode_0700() {
    let root = scratch("modes");
    let euid = rustix::process::geteuid().as_raw();
    let state = root.join("state");
    mkdir(&state, 0o700);
    assert!(config_for(&state).check_paths(euid).is_empty());

    mkdir(&state, 0o750);
    let got = config_for(&state).check_paths(euid);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].key, "dirs.state");
    assert_eq!(got[0].message, "has mode 0750, not 0700");

    mkdir(&state, 0o700);
    let got = config_for(&state).check_paths(euid + 1);
    assert_eq!(
        got[0].message,
        format!("is owned by uid {euid}, not {}", euid + 1)
    );

    let file = root.join("file");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(
        config_for(&file).check_paths(euid)[0].message,
        "is not a directory"
    );
}

#[test]
fn a_symlink_in_the_path_is_refused() {
    let root = scratch("links");
    let euid = rustix::process::geteuid().as_raw();
    let real = root.join("real");
    mkdir(&real.join("state"), 0o700);
    symlink(&real, root.join("alias")).unwrap();
    // A link above the directory and a link as the directory itself.
    let above = config_for(&root.join("alias/state")).check_paths(euid);
    let state: Vec<_> = above.iter().filter(|p| p.key == "dirs.state").collect();
    assert_eq!(state.len(), 1);
    assert!(
        state[0].message.ends_with("alias is a symlink"),
        "{}",
        state[0].message
    );
    symlink(real.join("state"), root.join("state-link")).unwrap();
    let last = config_for(&root.join("state-link")).check_paths(euid);
    assert!(
        last[0].message.ends_with("state-link is a symlink"),
        "{}",
        last[0].message
    );
    assert_eq!(last[0].key, "dirs.state");
}

#[cfg(target_os = "linux")]
#[test]
fn a_root_owned_system_alias_is_not_a_symlink_the_service_user_could_plant() {
    // /proc/self is a symlink root owns, like macOS's /var: allowed above the directory.
    let euid = rustix::process::geteuid().as_raw();
    let message =
        dir_problem(Path::new("/proc/self/fd"), euid).expect("a mode problem, not a link");
    assert!(
        message.contains("not 0700") || message.contains("owned by"),
        "{message}"
    );
    // As the directory itself it is refused, whoever owns it.
    let message = dir_problem(Path::new("/proc/self"), euid).expect("refused");
    assert_eq!(message, "/proc/self is a symlink");
}

#[test]
fn hot_and_data_dirs_must_share_a_file_system() {
    let root = scratch("fs");
    let euid = rustix::process::geteuid().as_raw();
    let (hot, data) = (root.join("hot"), root.join("data"));
    mkdir(&hot, 0o700);
    mkdir(&data, 0o700);
    assert!(
        config_for(&root)
            .check_paths(euid)
            .iter()
            .all(|p| p.key != "storage.hot_dir")
    );
    // /dev/shm is a different file system from the scratch dir on Linux; elsewhere the
    // check is skipped, since there is no second file system to name.
    let shm = Path::new("/dev/shm");
    if shm.is_dir()
        && std::fs::metadata(shm).unwrap().dev() != std::fs::metadata(&root).unwrap().dev()
    {
        let other = shm.join(format!("kbf-enroll-config-{}", std::process::id()));
        mkdir(&other, 0o700);
        let text = format!(
            "{MINIMAL}[storage]\nhot_dir = \"{}\"\n[daemon]\ndata_dir = \"{}\"\n",
            hot.display(),
            other.display()
        );
        let got = ok(&text).check_paths(euid);
        let _ = std::fs::remove_dir(&other);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            got[0].message,
            "must be on the same file system as daemon.data_dir"
        );
    }
}

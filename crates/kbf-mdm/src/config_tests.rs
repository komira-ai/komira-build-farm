//! Startup: every file is checked before the gate serves, and the gate serves the API
//! over mutual TLS once it has reconciled with the MDM.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::*;
use crate::fake_nanohub::{FakeHub, start};
use crate::tlskit::{Pki, pki, request};

const API_KEY: &str = "nanohub-test-key";

fn put(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Writes every file a gate needs under a fresh directory; the flags that name them.
fn setup(name: &str, hub: &FakeHub, pki: &Pki) -> (PathBuf, Cli) {
    let dir = crate::testkit::scratch(name);
    let inventory =
        r#"{"macs": [{"serial": "MAC0", "enrollment": "UDID-0", "pool": "mac-arm64"}]}"#;
    put(&dir.join("inventory.json"), inventory, 0o644);
    let signers = crate::testkit::SkKey::new(1).allowed_line("alice");
    put(&dir.join("allowed_signers"), &signers, 0o644);
    put(&dir.join("allowlist"), "", 0o644);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    let root = crate::sets::fixture::public(&crate::sets::fixture::key(crate::sets::fixture::ROOT));
    put(&dir.join("root.pub"), &root, 0o644);
    put(&dir.join("grant.key"), &STANDARD.encode([5u8; 32]), 0o600);
    put(&dir.join("api.key"), API_KEY, 0o600);
    pki.gate.write(&dir, "gate");
    let cli = Cli {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls_cert: dir.join("gate.pem"),
        tls_key: dir.join("gate.key"),
        server_key_sha256: hex::encode(pki.server.pin()),
        nanohub_url: hub.url.clone(),
        nanohub_api_key_file: dir.join("api.key"),
        inventory: dir.join("inventory.json"),
        allowed_signers: dir.join("allowed_signers"),
        profile_allowlist: dir.join("allowlist"),
        profile_dir: dir.join("profiles"),
        root_key: dir.join("root.pub"),
        grant_key: dir.join("grant.key"),
        state_dir: dir.join("state"),
        mac_floor: 0,
        daily_erase_cap: 2,
        max_lease_minutes: 480,
        require_user_verified: false,
        tick_seconds: 1,
        trusted_uid: trusted::owner_of(&dir).unwrap(),
    };
    (dir, cli)
}

#[tokio::test]
async fn the_gate_reconciles_then_serves_mtls_until_shutdown() {
    let hub = start(API_KEY).await;
    let pki = pki();
    let (dir, cli) = setup("config-run", &hub, &pki);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(run(
        cli,
        Box::pin(async { stop_rx.await.unwrap() }),
        Some(ready_tx),
    ));
    let addr = ready_rx.await.unwrap();
    let (status, body) = request(
        addr,
        &pki.ca,
        Some(&pki.server),
        "GET",
        "/v1/macs/MAC0",
        b"",
    )
    .await
    .unwrap();
    assert_eq!(status, 200, "{body}");
    let requests = hub.requests();
    assert_eq!(
        requests[0], "PUT /api/v1/ddm/declarations",
        "the subscription comes first"
    );
    assert!(requests.contains(&"GET /api/v1/ddm/declarations".to_owned()));
    assert!(requests.contains(&"GET /api/v1/ddm/status-values/UDID-0".to_owned()));
    assert!(
        request(addr, &pki.ca, Some(&pki.stranger), "GET", "/v1/macs", b"")
            .await
            .is_err()
    );
    // A tick that cannot save its state is logged, and the gate keeps serving.
    let state = dir.join("state").join("state.json");
    while !state.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir_all(state.join("blocked")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (status, _) = request(addr, &pki.ca, Some(&pki.server), "GET", "/v1/macs", b"")
        .await
        .unwrap();
    assert_eq!(status, 200);
    stop_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn every_file_is_checked_before_the_gate_starts() {
    // Catches: a gate that starts with a file another user can write, or a secret
    // others can read.
    let hub = start(API_KEY).await;
    let pki = pki();
    let (dir, cli) = setup("config-files", &hub, &pki);
    assert!(gate(&cli).is_ok());
    let check = |cli: &Cli, expected: &str| {
        let e = gate(cli).err().unwrap();
        assert!(e.contains(expected), "{e} lacks {expected}");
    };
    check(
        &Cli {
            trusted_uid: cli.trusted_uid + 1,
            ..cli.clone()
        },
        "owned by uid",
    );
    put(&dir.join("bad.json"), "{}", 0o644);
    check(
        &Cli {
            inventory: dir.join("bad.json"),
            ..cli.clone()
        },
        "bad.json: missing field",
    );
    check(
        &Cli {
            root_key: dir.join("bad.json"),
            ..cli.clone()
        },
        "expected a base64 ed25519 public key",
    );
    put(&dir.join("open.key"), &STANDARD.encode([5u8; 32]), 0o640);
    check(
        &Cli {
            grant_key: dir.join("open.key"),
            ..cli.clone()
        },
        "readable by its owner only",
    );
    put(&dir.join("short.key"), "AAAA", 0o600);
    check(
        &Cli {
            grant_key: dir.join("short.key"),
            ..cli.clone()
        },
        "expected a base64 32-byte seed",
    );
    check(
        &Cli {
            grant_key: dir.join("missing"),
            ..cli.clone()
        },
        "missing",
    );
    std::fs::create_dir_all(dir.join("key_dir")).unwrap();
    std::fs::set_permissions(dir.join("key_dir"), std::fs::Permissions::from_mode(0o700)).unwrap();
    check(
        &Cli {
            nanohub_api_key_file: dir.join("key_dir"),
            ..cli.clone()
        },
        "key_dir",
    );
    put(&dir.join("bad_signers"), "x cert-authority y", 0o644);
    check(
        &Cli {
            allowed_signers: dir.join("bad_signers"),
            ..cli.clone()
        },
        "cert-authority",
    );
    check(
        &Cli {
            profile_allowlist: dir.join("missing"),
            ..cli.clone()
        },
        "missing",
    );
    check(
        &Cli {
            state_dir: dir.join("inventory.json").join("x"),
            ..cli.clone()
        },
        "inventory.json",
    );
    std::fs::create_dir_all(dir.join("state2").join("audit.log")).unwrap();
    check(
        &Cli {
            state_dir: dir.join("state2"),
            ..cli.clone()
        },
        "audit log",
    );
    std::fs::create_dir_all(dir.join("state4")).unwrap();
    put(&dir.join("state4").join("state.json"), "{", 0o644);
    check(
        &Cli {
            state_dir: dir.join("state4"),
            ..cli.clone()
        },
        "state file",
    );
}

#[tokio::test]
async fn startup_fails_cleanly_when_the_mdm_tls_or_listener_does() {
    let hub = start(API_KEY).await;
    let pki = pki();
    let (dir, cli) = setup("config-start", &hub, &pki);
    let fail = |cli: Cli| async move {
        run(cli, Box::pin(std::future::pending()), None)
            .await
            .unwrap_err()
            .to_string()
    };
    let e = fail(Cli {
        nanohub_url: "http://127.0.0.1:9".into(),
        ..cli.clone()
    })
    .await;
    assert!(e.starts_with("startup reconciliation: "), "{e}");
    let e = fail(Cli {
        tls_cert: dir.join("missing.pem"),
        ..cli.clone()
    })
    .await;
    assert!(e.contains("missing.pem"), "{e}");
    let e = fail(Cli {
        server_key_sha256: "abc".into(),
        ..cli.clone()
    })
    .await;
    assert_eq!(e, "--server-key-sha256: expected 64 hex digits");
    pki.stranger.write(&dir, "stranger");
    let e = fail(Cli {
        tls_key: dir.join("stranger.key"),
        ..cli.clone()
    })
    .await;
    assert!(e.starts_with("TLS: "), "{e}");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let e = fail(Cli {
        listen: taken.local_addr().unwrap(),
        ..cli.clone()
    })
    .await;
    assert!(e.starts_with("bind "), "{e}");
}

fn args(cli: &Cli) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec!["kbf-mdm-gate".into()];
    let mut flag = |name: &str, value: &dyn AsRef<std::ffi::OsStr>| {
        out.push(format!("--{name}").into());
        out.push(value.as_ref().into());
    };
    flag("listen", &cli.listen.to_string());
    flag("tls-cert", &cli.tls_cert);
    flag("tls-key", &cli.tls_key);
    flag("server-key-sha256", &cli.server_key_sha256);
    flag("nanohub-url", &cli.nanohub_url);
    flag("nanohub-api-key-file", &cli.nanohub_api_key_file);
    flag("inventory", &cli.inventory);
    flag("allowed-signers", &cli.allowed_signers);
    flag("profile-allowlist", &cli.profile_allowlist);
    flag("profile-dir", &cli.profile_dir);
    flag("root-key", &cli.root_key);
    flag("grant-key", &cli.grant_key);
    flag("state-dir", &cli.state_dir);
    flag("mac-floor", &cli.mac_floor.to_string());
    flag("max-lease-minutes", &cli.max_lease_minutes.to_string());
    flag("trusted-uid", &cli.trusted_uid.to_string());
    out.push("--require-user-verified".into());
    out
}

#[test]
fn main_exits_2_on_bad_flags_or_a_failed_start_and_0_on_sigterm() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let hub = runtime.block_on(start(API_KEY));
    let pki = pki();
    let (_dir, mut cli) = setup("config-main", &hub, &pki);
    assert_eq!(
        main(vec!["kbf-mdm-gate".into(), "--nonsense".into()]),
        ExitCode::from(2)
    );
    let broken = Cli {
        inventory: PathBuf::from("/nonexistent"),
        ..cli.clone()
    };
    assert_eq!(main(args(&broken)), ExitCode::from(2));
    // A clean start, stopped by SIGTERM once it listens.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    cli.listen = port;
    let killer = std::thread::spawn(move || {
        while std::net::TcpStream::connect(port).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: kill(2) on this process with SIGTERM, whose handler `main` installed
        // before it bound the port just connected to.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
    });
    assert_eq!(main(args(&cli)), ExitCode::SUCCESS);
    killer.join().unwrap();
}

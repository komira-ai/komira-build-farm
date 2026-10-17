//! Envoy's own validation of the rendered bootstrap: the pinned Envoy release binary
//! run as `envoy --mode validate`, which parses the whole config against Envoy's
//! protos and builds every listener, cluster and TLS context without binding a port
//! or resolving a name. `envoy check` reads only the fields it models; this is what
//! refuses a field Envoy does not know.
//!
//! Ignored in the plain test run: the binary is not in the repository. CI's
//! test-x86 job fetches the release `templates/envoy/config.example`'s image runs
//! (`cargo xtask fetch`, pinned by sha256) and runs these with `--ignored`, naming
//! the binary in `KBF_TEST_ENVOY`. Run without it, each test fails rather than
//! passes.
//!
//! The config names the CA at the pod's mount (`render::CA_FILE`), which does not
//! exist here; the copy validated points it at a CA certificate made for the test.
//! Nothing else is changed.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_deploy::envoy::box_render::render_box;
use kbf_deploy::envoy::render::{self, CA_FILE};
use kbf_deploy::envoy::{BoxConfig, EnvoyConfig};

/// The pinned Envoy binary CI fetched.
fn envoy_binary() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_ENVOY")
        .expect("KBF_TEST_ENVOY names the pinned Envoy binary (CI's test-x86 job fetches it)");
    PathBuf::from(path)
}

/// The example's render, its CA path pointed at a fresh test CA under `dir`.
fn example_envoy_json(dir: &Path) -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let c = EnvoyConfig::load(&manifest.join("templates/envoy/config.example")).expect("config");
    let text = render::render(&c).envoy;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().expect("key");
    let ca = params.self_signed(&key).expect("CA");
    let ca_path = dir.join("ca.pem");
    std::fs::write(&ca_path, ca.pem()).expect("write the CA");
    assert!(
        text.matches(CA_FILE).count() >= 2,
        "the CA path {CA_FILE} is not in the config"
    );
    text.replace(CA_FILE, ca_path.to_str().expect("utf-8"))
}

/// `envoy --mode validate` on `text`: its exit status and output.
fn validate(dir: &Path, name: &str, text: &str) -> (Option<i32>, String) {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write the config");
    let out = Command::new(envoy_binary())
        .args([
            "--mode",
            "validate",
            "--disable-hot-restart",
            "--log-level",
            "warn",
        ])
        .arg("--config-path")
        .arg(&path)
        .output()
        .expect("run envoy");
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    (out.status.code(), text.into_owned())
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a field Envoy does not know or a value of the wrong type in the render (a
/// typo `envoy check` reads past, such as `stream_idle_timout`, which Envoy would
/// refuse at pod start, so the front never comes up).
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_accepts_the_example_render() {
    let dir = scratch("envoy-validate-example");
    let text = example_envoy_json(&dir);
    let (code, out) = validate(&dir, "envoy.json", &text);
    assert_eq!(code, Some(0), "Envoy refuses the render:\n{out}");
}

/// The control. Catches a validation that accepts anything (a binary that is not
/// Envoy, or a mode that does not parse the config), without which the test above
/// proves nothing: the same render with one field misspelt must be refused.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_refuses_an_unknown_field() {
    let dir = scratch("envoy-validate-control");
    let text = example_envoy_json(&dir);
    let field = "\"stream_idle_timeout\"";
    assert_eq!(text.matches(field).count(), 1, "the field to misspell");
    let control = text.replace(field, "\"stream_idle_timout\"");
    let (code, out) = validate(&dir, "control.json", &control);
    assert_ne!(code, Some(0), "Envoy accepted stream_idle_timout:\n{out}");
    assert!(
        out.contains("stream_idle_timout"),
        "refused for another reason:\n{out}"
    );
}

/// The box example's render with its three TLS files (the certificate, its key and the
/// CA) replaced by ones made for the test under `dir`.
fn box_envoy_json(dir: &Path) -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let c = BoxConfig::load(&manifest.join("templates/envoy/box.example")).expect("config");
    let text = render_box(&c).envoy;
    let key = rcgen::KeyPair::generate().expect("key");
    let params = rcgen::CertificateParams::new(vec![c.host.clone()]).expect("params");
    let cert = params.self_signed(&key).expect("certificate");
    let files = [
        (&c.cert_file, "reapi.pem", cert.pem()),
        (&c.key_file, "reapi.key", key.serialize_pem()),
        (&c.ca_file, "ca.pem", cert.pem()),
    ];
    let mut text = text;
    for (from, name, pem) in files {
        let to = dir.join(name);
        std::fs::write(&to, pem).expect("write the file");
        assert!(text.contains(from.as_str()), "{from} is not in the config");
        text = text.replace(from.as_str(), to.to_str().expect("utf-8"));
    }
    text
}

/// Catches a box render Envoy refuses: a field it does not know, a value of the wrong
/// type, a cluster or listener it cannot build (a retry policy, a breaker, a health
/// check of an endpoint's own port, a tcp_proxy idle timeout, or the REAPI listener's
/// TLS context), which `envoy check` reads past.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_accepts_the_box_render() {
    let dir = scratch("envoy-validate-box");
    let text = box_envoy_json(&dir);
    let (code, out) = validate(&dir, "envoy.json", &text);
    assert_eq!(code, Some(0), "Envoy refuses the box render:\n{out}");
}

/// The control for the test above: the same render with one box-only field misspelt
/// must be refused, so a validation that reads nothing of the box's own settings cannot
/// pass it.
#[test]
#[ignore = "needs the pinned Envoy binary: CI's test-x86 job passes it in KBF_TEST_ENVOY"]
fn envoy_refuses_an_unknown_box_field() {
    let dir = scratch("envoy-validate-box-control");
    let text = box_envoy_json(&dir);
    let field = "\"close_connections_on_host_health_failure\"";
    assert_eq!(text.matches(field).count(), 2, "the field to misspell");
    let control = text.replace(field, "\"close_connections_on_host_health_failur\"");
    let (code, out) = validate(&dir, "control.json", &control);
    assert_ne!(code, Some(0), "Envoy accepted the misspelt field:\n{out}");
    assert!(
        out.contains("close_connections_on_host_health_failur"),
        "refused for another reason:\n{out}"
    );
}

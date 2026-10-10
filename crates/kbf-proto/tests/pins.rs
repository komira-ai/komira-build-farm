//! The vendored upstream files match `proto/third_party/PINS`.
//!
//! Catches:
//! - a vendored file edited in place (even one comment) without its pin bumped, so the
//!   tree no longer matches the upstream commit PINS names;
//! - a vendored file added without a PINS line, or a PINS line whose file is gone;
//! - one directory that mixes files from two upstream commits or repositories;
//! - a PINS file that lists nothing or loses a pinned `.proto` or LICENSE, which would
//!   make every check above pass vacuously.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

struct Pin {
    repo: String,
    commit: String,
    sha256: String,
}

fn third_party() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("proto/third_party")
}

/// Reads PINS into a map keyed by `<directory>/<path>`.
fn read_pins() -> BTreeMap<String, Pin> {
    let text = fs::read_to_string(third_party().join("PINS")).expect("read PINS");
    let mut pins = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [dir, repo, commit, sha256, path] = fields[..] else {
            panic!("PINS line {}: want 5 fields, got {}", n + 1, fields.len());
        };
        assert!(
            commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
            "PINS line {}: commit {commit:?} is not a full SHA-1",
            n + 1
        );
        assert!(
            sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "PINS line {}: {sha256:?} is not a sha256",
            n + 1
        );
        let key = format!("{dir}/{path}");
        let pin = Pin {
            repo: repo.to_owned(),
            commit: commit.to_owned(),
            sha256: sha256.to_ascii_lowercase(),
        };
        assert!(
            pins.insert(key.clone(), pin).is_none(),
            "PINS lists {key} twice"
        );
    }
    pins
}

/// Every regular file under `dir`, as a path relative to `base` with `/` separators.
fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read vendored directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            walk(base, &path, out);
        } else {
            let rel = path.strip_prefix(base).expect("under base");
            let parts: Vec<_> = rel
                .iter()
                .map(|c| c.to_str().expect("UTF-8 path"))
                .collect();
            out.push(parts.join("/"));
        }
    }
}

fn vendored_files() -> Vec<String> {
    let base = third_party();
    let mut files = Vec::new();
    walk(&base, &base, &mut files);
    files.retain(|f| f != "PINS");
    files.sort();
    files
}

#[test]
fn every_vendored_file_matches_its_pin() {
    let pins = read_pins();
    let files = vendored_files();

    let mut errors = Vec::new();
    for file in &files {
        let Some(pin) = pins.get(file) else {
            errors.push(format!("{file}: vendored but not listed in PINS"));
            continue;
        };
        let bytes = fs::read(third_party().join(file)).expect("read vendored file");
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != pin.sha256 {
            errors.push(format!(
                "{file}: sha256 {actual} but PINS says {} ({} at {})",
                pin.sha256, pin.repo, pin.commit
            ));
        }
    }
    for key in pins.keys() {
        if !files.contains(key) {
            errors.push(format!("{key}: listed in PINS but not vendored"));
        }
    }
    assert!(
        errors.is_empty(),
        "PINS mismatch:\n  {}",
        errors.join("\n  ")
    );
}

#[test]
fn each_directory_comes_from_one_upstream_commit() {
    let mut origin: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    let pins = read_pins();
    for (key, pin) in &pins {
        let dir = key.split('/').next().expect("directory");
        let seen = origin.entry(dir).or_insert((&pin.repo, &pin.commit));
        assert_eq!(
            *seen,
            (pin.repo.as_str(), pin.commit.as_str()),
            "{key}: {dir} mixes upstream repositories or commits"
        );
    }
}

#[test]
fn pins_cover_the_files_the_build_reads() {
    let pins = read_pins();
    for key in [
        "remote-apis/LICENSE",
        "remote-apis/build/bazel/remote/execution/v2/remote_execution.proto",
        "remote-apis/build/bazel/semver/semver.proto",
        "googleapis/LICENSE",
        "googleapis/google/api/annotations.proto",
        "googleapis/google/api/client.proto",
        "googleapis/google/api/http.proto",
        "googleapis/google/bytestream/bytestream.proto",
        "googleapis/google/longrunning/operations.proto",
        "googleapis/google/rpc/status.proto",
        "grpc-proto/LICENSE",
        "grpc-proto/grpc/health/v1/health.proto",
    ] {
        assert!(pins.contains_key(key), "PINS has no line for {key}");
    }
}

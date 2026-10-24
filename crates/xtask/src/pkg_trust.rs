//! `pkg-trust --gh <program> --trusted-root <file> --source-ref refs/heads/<branch> --out
//! <dir>`: the trust directory of a `kbf-installer` package (docs/design/kbf-installer.md,
//! section 3; [`crate::pkg_linux::TRUST_FILES`]), made from the pinned `gh`, the Sigstore
//! trusted root `gh` fetched, and the one per-build setting:
//!
//! - `gh`, mode 0755: a copy of `--gh`, refused when empty;
//! - `trusted_root.jsonl`, mode 0644: a copy of `--trusted-root`, refused unless it is
//!   JSON lines (every line that is not blank a JSON object) with at least one;
//! - `installer.conf`, mode 0644: exactly `SOURCE_REFS=<ref>` and a newline. The ref is
//!   the branch the package is attested on (`$GITHUB_REF` in the workflow), so a host
//!   that installs the package believes releases attested for that branch and no other:
//!   a package built on `main` must never trust a feature branch (and one built on a
//!   feature branch trusts that branch, not `main`). The generator writes no other key,
//!   so it cannot drift from the keys the installer's settings parser accepts
//!   (`crates/kbf-integration-test` parses its output with the installer's own parser).
//!
//! A ref the installer would refuse ([`ref_ok`], the rule of its `SourceRef`) is refused
//! here, before a package is built that no host could load.

use std::path::Path;

use crate::Error;
use crate::package::read;
use crate::pkg_linux::TRUST_FILES;

/// Whether `source_ref` is `refs/heads/` and a branch name of letters, digits and `. _ -
/// /` that is not empty, does not start or end with `/`, and holds no `//` or `..`: the
/// rule of the installer's `SourceRef`, which it passes to `gh` as an argument.
pub fn ref_ok(source_ref: &str) -> bool {
    let branch = source_ref.strip_prefix("refs/heads/").unwrap_or("");
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    !branch.is_empty()
        && branch.chars().all(plain)
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.contains("//")
        && !branch.contains("..")
}

/// The text of `installer.conf` for the ref `source_ref`.
pub fn conf(source_ref: &str) -> String {
    format!("SOURCE_REFS={source_ref}\n")
}

/// The `SOURCE_REFS` value of an `installer.conf` that [`conf`] wrote, or `None` when
/// `text` is not exactly that.
pub fn conf_ref(text: &str) -> Option<&str> {
    let value = text.strip_prefix("SOURCE_REFS=")?.strip_suffix('\n')?;
    ref_ok(value).then_some(value)
}

/// What is wrong with `bytes` as the trusted root: it is JSON lines, at least one.
pub fn root_problems(bytes: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return vec!["is not UTF-8".to_owned()];
    };
    let mut objects = 0;
    let mut problems = Vec::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) if v.is_object() => objects += 1,
            Ok(_) => problems.push(format!("line {}: not a JSON object", i + 1)),
            Err(e) => problems.push(format!("line {}: not JSON: {e}", i + 1)),
        }
    }
    if objects == 0 && problems.is_empty() {
        problems.push("holds no JSON object".to_owned());
    }
    problems
}

/// The `pkg-trust` verb: writes the three trust files into `out` and returns the lines to
/// print. The directory is always built fresh: an existing `out` (a leftover of an earlier
/// run, or a restored build cache) is emptied first, so nothing stale survives; a path
/// that exists but is not a plain directory (a file, a symlink) is refused.
///
/// # Errors
/// An input is refused (the ref, an empty `gh`, a trusted root that is not JSON lines)
/// or a file cannot be read or written.
pub fn write(
    gh: &Path,
    trusted_root: &Path,
    source_ref: &str,
    out: &Path,
) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    if !ref_ok(source_ref) {
        problems.push(format!(
            "--source-ref {source_ref:?} is not a branch the installer accepts: refs/heads/ and a name of letters, digits and . _ - /"
        ));
    }
    let gh_bytes = read(gh)?;
    if gh_bytes.is_empty() {
        problems.push(format!("{} is empty", gh.display()));
    }
    let root = read(trusted_root)?;
    problems.extend(
        root_problems(&root)
            .into_iter()
            .map(|p| format!("{}: {p}", trusted_root.display())),
    );
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    match std::fs::symlink_metadata(out) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(out)
            .map_err(Error::spawn(format!("empty {}", out.display())))?,
        Ok(_) => {
            return Err(Error::Refused(vec![format!(
                "{} exists and is not a directory pkg-trust may replace",
                out.display()
            )]));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Refused(vec![format!("{}: {e}", out.display())]));
        }
    }
    std::fs::create_dir_all(out).map_err(Error::spawn(format!("create {}", out.display())))?;
    let conf = conf(source_ref);
    let mut printed = Vec::new();
    for (name, mode) in TRUST_FILES {
        let bytes: &[u8] = match name {
            "gh" => &gh_bytes,
            "trusted_root.jsonl" => &root,
            _ => conf.as_bytes(),
        };
        let path = out.join(name);
        crate::package::write_new(&path, bytes)?;
        set_mode(&path, mode)?;
        printed.push(format!("wrote {} ({mode:04o})", path.display()));
    }
    printed.push(format!("installer.conf: SOURCE_REFS={source_ref}"));
    Ok(printed)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(Error::spawn(format!("set the mode of {}", path.display())))
}

#[cfg(test)]
mod tests;

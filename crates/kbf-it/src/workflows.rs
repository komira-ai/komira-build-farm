//! Lint for GitHub Actions workflow files.
//!
//! CI for this repository runs on GitHub-hosted runners only and never runs pull
//! request code with repository write access. A workflow file is refused if:
//! - it names the `self-hosted` runner label anywhere outside a comment;
//! - a `runs-on:` value is not one literal hosted image label on the same line
//!   (`ubuntu-*`, `windows-*`, `macos-*`). A custom label, runner group, list or
//!   expression could select a non-hosted runner without saying `self-hosted`;
//! - it uses the `pull_request_target` trigger anywhere outside a comment;
//! - a `uses:` step is not pinned to a full 40-character commit SHA followed by a
//!   version comment (local `./` actions are exempt);
//! - it lacks a top-level `permissions: {}` line, so jobs start with no token rights.
//!
//! The lint reads lines, not YAML: it is strict on purpose, so a form it cannot read
//! one line at a time is refused rather than guessed.

/// Scans one workflow text and returns one message per problem, each prefixed with
/// its 1-based line number (0 for a whole-file problem).
pub fn scan(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut top_permissions = false;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let (code, comment) = split_comment(raw);
        if raw.trim_end() == "permissions: {}" {
            top_permissions = true;
        }
        if code.contains("self-hosted") {
            out.push(format!("line {n}: names the self-hosted runner label"));
        }
        if code.contains("pull_request_target") {
            out.push(format!("line {n}: uses the pull_request_target trigger"));
        }
        let key = code.trim_start().trim_start_matches("- ");
        if let Some(v) = key.strip_prefix("runs-on:") {
            let v = v.trim();
            if !is_hosted_label(v) {
                out.push(format!(
                    "line {n}: runs-on must be one literal hosted label, got `{v}`"
                ));
            }
        }
        if let Some(v) = key.strip_prefix("uses:") {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            if !v.starts_with("./") && !is_sha_pinned(v) {
                out.push(format!(
                    "line {n}: `{v}` is not pinned to a full commit SHA"
                ));
            } else if !v.starts_with("./") && comment.is_none_or(|c| c.trim().is_empty()) {
                out.push(format!("line {n}: `{v}` needs a version comment"));
            }
        }
    }
    if !top_permissions {
        out.push("line 0: no top-level `permissions: {}`".to_owned());
    }
    out
}

/// Splits a line into code and the text after a YAML comment marker (`#` at the
/// start of the line or after whitespace).
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let b = line.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            return (&line[..i], Some(&line[i + 1..]));
        }
    }
    (line, None)
}

fn is_hosted_label(v: &str) -> bool {
    let ok_chars = v
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.');
    ok_chars
        && ["ubuntu-", "windows-", "macos-"]
            .iter()
            .any(|p| v.len() > p.len() && v.starts_with(p))
}

fn is_sha_pinned(v: &str) -> bool {
    match v.rsplit_once('@') {
        Some((action, sha)) => {
            !action.is_empty()
                && sha.len() == 40
                && sha
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn workflow(body: &str) -> String {
        format!("name: t\non: push\npermissions: {{}}\njobs:\n  a:\n{body}")
    }

    #[test]
    fn a_clean_workflow_passes() {
        // Catches: a lint that refuses the forms ci.yml relies on.
        let w = workflow(&format!(
            "    runs-on: ubuntu-24.04-arm\n    steps:\n      - uses: a/b@{SHA} # v1.2.3\n      - uses: ./local\n"
        ));
        assert_eq!(scan(&w), Vec::<String>::new());
    }

    #[test]
    fn self_hosted_is_refused_in_any_form() {
        // Catches: a lint that only reads a bare `runs-on: self-hosted`.
        for body in [
            "    runs-on: self-hosted\n",
            "    runs-on: [self-hosted, linux]\n",
            "    strategy:\n      matrix:\n        os: [self-hosted]\n",
        ] {
            let found = scan(&workflow(body));
            assert!(found.iter().any(|m| m.contains("self-hosted")), "{body}");
        }
    }

    #[test]
    fn non_literal_runs_on_is_refused() {
        // Catches: a custom label, runner group, list or expression slipping through.
        for v in [
            "big-box",
            "",
            "${{ matrix.os }}",
            "[ubuntu-latest]",
            "ubuntu-",
        ] {
            let found = scan(&workflow(&format!("    runs-on: {v}\n")));
            assert_eq!(found.len(), 1, "runs-on: {v} -> {found:?}");
        }
    }

    #[test]
    fn pull_request_target_is_refused() {
        // Catches: a lint that misses the trigger in list or mapping form.
        for on in ["on: [pull_request_target]", "on:\n  pull_request_target:"] {
            let w = format!("{on}\npermissions: {{}}\n");
            assert_eq!(scan(&w).len(), 1, "{on}");
        }
    }

    #[test]
    fn comments_are_not_code() {
        // Catches: a lint that refuses a comment, or reads `#` inside a value as one.
        let w = workflow(
            "    # never pull_request_target, never self-hosted\n    runs-on: ubuntu-latest\n",
        );
        assert_eq!(scan(&w), Vec::<String>::new());
    }

    #[test]
    fn unpinned_actions_are_refused() {
        // Catches: a tag, branch, short SHA or uppercase SHA accepted as a pin.
        let short = &SHA[..12];
        let upper = SHA.to_uppercase();
        for v in [
            "a/b@v4",
            "a/b@main",
            "a/b",
            &format!("a/b@{short}"),
            &format!("a/b@{upper}"),
        ] {
            let found = scan(&workflow(&format!("      - uses: {v} # v4\n")));
            assert_eq!(found.len(), 1, "{v}");
            assert!(found[0].contains("not pinned"), "{v}");
        }
    }

    #[test]
    fn pins_need_a_version_comment() {
        // Catches: a bare SHA, which hides which release it is.
        let found = scan(&workflow(&format!("      - uses: a/b@{SHA}\n")));
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("version comment"));
    }

    #[test]
    fn top_level_permissions_are_required() {
        // Catches: a workflow whose jobs inherit the default token rights.
        let found = scan("on: push\njobs:\n  a:\n    permissions: {}\n");
        assert_eq!(
            found,
            vec!["line 0: no top-level `permissions: {}`".to_owned()]
        );
    }
}

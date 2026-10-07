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
//! The lint reads lines, not YAML. So that the checks above cannot be dodged by a
//! YAML spelling they do not read, it also refuses:
//! - the word `runs-on` or `uses` anywhere except as the plain block key
//!   `runs-on:`/`uses:` at the start of a line (catches `runs-on :`, `"runs-on":` and
//!   either key inside a flow mapping);
//! - a line that continues the value of a `runs-on:` or `uses:` key (a plain scalar
//!   folded over several lines);
//! - a flow collection (`{` or `[`, other than an empty `{}`/`[]` and `${{ }}`
//!   expressions) on the `jobs:` line or under it;
//! - a line whose first item is quoted, tagged, anchored, an alias, an explicit key
//!   or a flow collection (`"`, `'`, `!`, `&`, `*`, `?`, `{`, `[`);
//! - a backslash on a line with a double quote (an escape could spell any word);
//! - a YAML tag (`!` starting a token outside `${{ }}`).
//!
//! These apply to `run:` scripts too, since the lint cannot tell a script line from a
//! key. A script that needs such a form belongs in a file the workflow runs.

/// Scans one workflow text and returns one message per problem, each prefixed with
/// its 1-based line number (0 for a whole-file problem).
pub fn scan(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut top_permissions = false;
    let mut in_jobs = false;
    // Column of the last `runs-on:`/`uses:` key; the next line must not be deeper.
    let mut scalar_key_col: Option<usize> = None;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let (code, comment) = split_comment(raw);
        if raw.trim_end() == "permissions: {}" {
            top_permissions = true;
        }
        if code.trim().is_empty() {
            continue;
        }
        let indent = code.len() - code.trim_start().len();
        let item = first_item(code);
        let col = code.len() - item.len();
        if indent == 0 {
            in_jobs = is_key(item, "jobs");
        }
        if scalar_key_col.take().is_some_and(|k| indent > k) {
            out.push(format!(
                "line {n}: continues the value of the key on an earlier line"
            ));
        }
        if code.contains("self-hosted") {
            out.push(format!("line {n}: names the self-hosted runner label"));
        }
        if code.contains("pull_request_target") {
            out.push(format!("line {n}: uses the pull_request_target trigger"));
        }
        out.extend(unreadable_forms(n, code, item, in_jobs));
        for key in ["runs-on", "uses"] {
            let canonical = plain_value(item, key).is_some();
            if word_offsets(code, key).any(|at| !(canonical && at == col)) {
                out.push(format!(
                    "line {n}: `{key}` appears outside the plain `{key}:` key form"
                ));
            }
        }
        if let Some(v) = plain_value(item, "runs-on") {
            scalar_key_col = Some(col);
            let v = v.trim();
            if !is_hosted_label(v) {
                out.push(format!(
                    "line {n}: runs-on must be one literal hosted label, got `{v}`"
                ));
            }
        }
        if let Some(v) = plain_value(item, "uses") {
            scalar_key_col = Some(col);
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

/// Messages for YAML forms that could spell a checked key or value without the
/// literal text the line checks look for.
fn unreadable_forms(n: usize, code: &str, item: &str, in_jobs: bool) -> Vec<String> {
    let mut out = Vec::new();
    if item.starts_with(['"', '\'', '!', '&', '*', '?', '{', '[']) {
        out.push(format!(
            "line {n}: a quoted, tagged, anchored, alias, explicit or flow item is not read"
        ));
    }
    if code.contains('"') && code.contains('\\') {
        out.push(format!(
            "line {n}: a backslash escape in double quotes is not read"
        ));
    }
    let bare = strip_expressions(code).replace("{}", "").replace("[]", "");
    if in_jobs && bare.contains(['{', '[']) {
        out.push(format!(
            "line {n}: a flow collection under `jobs:` is not read"
        ));
    }
    let b = bare.as_bytes();
    let tag = (0..b.len())
        .any(|i| b[i] == b'!' && (i == 0 || matches!(b[i - 1], b' ' | b'\t' | b',' | b'[' | b'{')));
    if tag {
        out.push(format!("line {n}: a YAML tag is not read"));
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

/// The line after its indentation and any block sequence markers (`- `).
fn first_item(code: &str) -> &str {
    let mut s = code.trim_start();
    while let Some(rest) = s.strip_prefix('-') {
        if !rest.starts_with([' ', '\t']) {
            break;
        }
        s = rest.trim_start();
    }
    s
}

/// True if `item` is the key `key` with any spacing before the colon.
fn is_key(item: &str, key: &str) -> bool {
    item.strip_prefix(key)
        .is_some_and(|r| r.trim_start().starts_with(':'))
}

/// The value after the plain block key `key:`, if `item` starts with it.
fn plain_value<'a>(item: &'a str, key: &str) -> Option<&'a str> {
    let v = item.strip_prefix(key)?.strip_prefix(':')?;
    (v.is_empty() || v.starts_with([' ', '\t'])).then_some(v)
}

/// Byte offsets where `word` occurs bounded by characters that cannot extend a key.
fn word_offsets<'a>(code: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
    let b = code.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    code.match_indices(word).map(|(i, _)| i).filter(move |&i| {
        (i == 0 || !ident(b[i - 1])) && b.get(i + word.len()).is_none_or(|&c| !ident(c))
    })
}

/// The line with every `${{ ... }}` expression removed. An unterminated `${{` is kept.
fn strip_expressions(code: &str) -> String {
    let mut out = String::new();
    let mut rest = code;
    while let Some(s) = rest.find("${{") {
        out.push_str(&rest[..s]);
        match rest[s..].find("}}") {
            Some(e) => rest = &rest[s + e + 2..],
            None => {
                out.push_str(&rest[s..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
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
            "    runs-on: ubuntu-24.04-arm\n    steps:\n      - uses: a/b@{SHA} # v1.2.3\n        with:\n          save-if: ${{{{ github.ref == 'refs/heads/main' }}}}\n      - uses: ./local\n      - run: echo causes  # words containing uses are not keys\n"
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
        // The last two catch an `is_hosted_label` that checks only the prefix.
        for v in [
            "big-box",
            "",
            "${{ matrix.os }}",
            "[ubuntu-latest]",
            "ubuntu-",
            "ubuntu-latest, big-box",
            "ubuntu-${{ inputs.x }}",
        ] {
            let found = scan(&workflow(&format!("    runs-on: {v}\n")));
            assert!(
                found
                    .iter()
                    .any(|m| m.contains("runs-on must be one literal")),
                "runs-on: {v} -> {found:?}"
            );
        }
    }

    /// Asserts that `body`, under a job, yields at least one message containing `want`.
    fn refused(body: &str, want: &str) {
        let found = scan(&workflow(body));
        assert!(
            found.iter().any(|m| m.contains(want)),
            "{body} -> {found:?}"
        );
    }

    #[test]
    fn respelled_runs_on_and_uses_keys_are_refused() {
        // Catches: a key the line checks would not read, yet YAML reads as runs-on/uses.
        refused("    runs-on : big-box\n", "outside the plain `runs-on:`");
        refused("    \"runs-on\": big-box\n", "outside the plain `runs-on:`");
        refused("    'runs-on': big-box\n", "outside the plain `runs-on:`");
        refused(
            "    steps:\n      - \"uses\": a/b@v1\n",
            "outside the plain `uses:`",
        );
        refused(
            "    steps:\n      - uses : a/b@v1\n",
            "outside the plain `uses:`",
        );
    }

    #[test]
    fn flow_style_jobs_are_refused() {
        // Catches: a non-hosted runner and an unpinned action hidden in a flow mapping.
        let w =
            "on: push\npermissions: {}\njobs: {a: {runs-on: big-box, steps: [{uses: a/b@v1}]}}\n";
        let found = scan(w);
        for want in [
            "flow collection under `jobs:`",
            "outside the plain `runs-on:`",
            "outside the plain `uses:`",
        ] {
            assert!(found.iter().any(|m| m.contains(want)), "{want}: {found:?}");
        }
        // A flow sequence on a line under `jobs:`, as well as on the `jobs:` line.
        refused("    needs: [b]\n", "flow collection under `jobs:`");
        // Catches: a flow check that also refuses expressions and empty collections.
        let ok = workflow("    if: ${{ github.event_name == 'push' }}\n    permissions: {}\n");
        assert_eq!(scan(&ok), Vec::<String>::new());
    }

    #[test]
    fn spellings_that_hide_words_are_refused() {
        // Catches: an escape, tag, anchor, explicit key or continuation line spelling a
        // value the substring checks never see.
        refused("    \"runs\\u002don\": big-box\n", "backslash escape");
        refused("    !!str runs-on: big-box\n", "quoted, tagged");
        refused("    name: !!binary cnVucy1vbg==\n", "YAML tag");
        refused("    ? runs-on\n    : big-box\n", "quoted, tagged");
        refused("    &r runs-on: big-box\n", "quoted, tagged");
        refused(
            "    runs-on: ubuntu-latest\n      big-box\n",
            "continues the value",
        );
        refused(
            &format!("    steps:\n      - uses: a/b@{SHA} # v1\n          x\n"),
            "continues the value",
        );
        let w = "on: [\"pull_request\\u005ftarget\"]\npermissions: {}\n";
        assert!(scan(w).iter().any(|m| m.contains("backslash escape")));
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

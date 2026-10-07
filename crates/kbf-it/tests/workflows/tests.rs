//! Tests of the workflow lint. Each says which defect it catches; the regression tests
//! name the YAML spelling that got past the earlier line-based scanner.

use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// The files a local `uses: ./...` in these tests may name.
fn repo() -> Repo {
    Repo {
        action_dirs: vec!["local".to_owned(), "x".to_owned(), ".github/a".to_owned()],
        workflows: vec![".github/workflows/c.yml".to_owned()],
    }
}

/// Lints a workflow against [`repo`].
fn scan(text: &str) -> Vec<String> {
    super::scan(text, &repo())
}

fn workflow(body: &str) -> String {
    format!("name: t\non: push\npermissions: {{}}\njobs:\n  a:\n{body}")
}

/// Asserts that `text` yields at least one message containing `want`.
fn refused_text(text: &str, want: &str) {
    let found = scan(text);
    assert!(
        found.iter().any(|m| m.contains(want)),
        "want `{want}`:\n{text}\n-> {found:?}"
    );
}

/// Asserts that `body`, under a job, yields at least one message containing `want`.
fn refused(body: &str, want: &str) {
    refused_text(&workflow(body), want);
}

fn clean(text: &str) {
    assert_eq!(scan(text), Vec::<String>::new(), "{text}");
}

#[test]
fn a_clean_workflow_passes() {
    // Catches: a lint that refuses the forms ci.yml relies on, or reads `#` inside a
    // value, a word inside a block scalar or a comment as code.
    clean(&workflow(&format!(
        "    runs-on: ubuntu-24.04-arm\n    steps:\n      - uses: a/b@{SHA} # v1.2.3\n        with:\n          save-if: ${{{{ github.ref == 'refs/heads/main' }}}}\n      - uses: ./local\n      - name: 'a # not a comment'\n        run: |\n          echo causes  # runs-on: big-box is script text\n    # never pull_request_target, never self-hosted\n"
    )));
    clean(
        "on:\n  pull_request:\n  push:\n    branches: [main]\npermissions: {}\njobs:\n  a:\n    runs-on: [ubuntu-latest]\n  b:\n    runs-on: {labels: [macos-15]}\n  c:\n    uses: ./.github/workflows/c.yml\n",
    );
}

#[test]
fn pull_request_target_is_refused_in_every_trigger_form() {
    // Catches: a lint that reads only some shapes of `on`.
    for on in [
        "on: pull_request_target",
        "on: [push, pull_request_target]",
        "on:\n  - pull_request_target",
        "on:\n  pull_request_target:",
        "on: {pull_request_target: {}}",
        "\"on\": [pull_request_target]",
        "On: [Pull_Request_Target]",
        "? on\n: [pull_request_target]",
    ] {
        refused_text(
            &format!("{on}\npermissions: {{}}\njobs: {{}}\n"),
            "triggers on pull_request_target",
        );
    }
}

#[test]
fn regression_hash_inside_a_quoted_flow_value_hid_the_trigger() {
    // The bypass the verifier found: the line scanner took `#` in 'a #' for a comment
    // and never read the rest of the line.
    let w = "on: {workflow_dispatch: {inputs: {x: {description: 'a #'}}}, pull_request_target: {}}\npermissions: {}\njobs: {}\n";
    refused_text(w, "triggers on pull_request_target");
    let w = "on: ['a #', pull_request_target]\npermissions: {}\njobs: {}\n";
    refused_text(w, "triggers on pull_request_target");
}

#[test]
fn regression_a_repeated_permissions_key_overrode_the_empty_one() {
    // The line scanner saw `permissions: {}` and never read the later key, which a
    // last-wins YAML reader would use.
    let w = "on: push\npermissions: {}\npermissions: write-all\njobs: {}\n";
    refused_text(w, "the key `permissions` is repeated");
}

#[test]
fn regression_escaped_words_are_decoded() {
    // An escape spelled a key or trigger the substring checks never saw.
    refused(
        "    \"runs\\u002don\": big-box\n",
        "runs-on must name hosted",
    );
    refused_text(
        "on: [\"pull_request\\u005ftarget\"]\npermissions: {}\njobs: {}\n",
        "triggers on pull_request_target",
    );
}

#[test]
fn regression_respelled_keys_are_read() {
    // A key spelled with a space, quotes, explicit `?`, other case or folded over
    // lines, or a step key in a flow mapping.
    for body in [
        "    runs-on : big-box\n",
        "    \"runs-on\": big-box\n",
        "    'runs-on': big-box\n",
        "    ? runs-on\n    : big-box\n",
        "    Runs-On: big-box\n",
        "    runs-on: ubuntu-latest\n      big-box\n",
    ] {
        refused(body, "runs-on must name hosted");
    }
    for body in [
        "    runs-on: ubuntu-latest\n    steps:\n      - \"uses\": a/b@v1\n",
        "    runs-on: ubuntu-latest\n    steps:\n      - uses : a/b@v1\n",
        "    runs-on: ubuntu-latest\n    steps: [{uses: a/b@v1}]\n",
    ] {
        refused(body, "not pinned");
    }
}

#[test]
fn regression_flow_style_jobs_are_read() {
    // A non-hosted runner and an unpinned action inside flow collections.
    let w = "on: push\npermissions: {}\njobs: {a: {runs-on: big-box, steps: [{uses: a/b@v1}]}}\n";
    refused_text(w, "runs-on must name hosted");
    refused_text(w, "not pinned");
}

#[test]
fn regression_anchors_and_aliases_are_resolved() {
    // An alias carried a value from somewhere the checks did not look.
    let w = "name: &r big-box\non: push\npermissions: {}\njobs:\n  a:\n    runs-on: *r\n";
    refused_text(w, "got `big-box`");
    let w = "name: t\non: [*p]\npermissions: {}\njobs: {}\nenv: {X: &p pull_request_target}\n";
    // The alias comes before its anchor here, so the parser itself refuses it.
    assert!(!scan(w).is_empty());
    let w = "env: {X: &p pull_request_target}\non: [*p]\npermissions: {}\njobs: {}\n";
    refused_text(w, "triggers on pull_request_target");
}

#[test]
fn self_hosted_is_refused_in_any_form() {
    // Catches: a lint that only reads a bare `runs-on: self-hosted`; the matrix case
    // is caught by the whole-document word check alone.
    for body in [
        "    runs-on: self-hosted\n",
        "    runs-on: [self-hosted, linux]\n",
        "    runs-on: {labels: Self-Hosted}\n",
        "    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        os: [self-hosted]\n",
    ] {
        refused(body, "names self-hosted");
    }
}

#[test]
fn runs_on_that_cannot_be_evaluated_is_refused() {
    // Catches: an expression or matrix, a runner group, a custom label or an empty
    // value accepted as hosted; the last two catch an `is_hosted_label` that checks
    // only the prefix.
    for (v, want) in [
        ("${{ matrix.os }}", "from an expression"),
        ("[ubuntu-latest, '${{ inputs.x }}']", "from an expression"),
        ("ubuntu-${{ inputs.x }}", "from an expression"),
        ("{group: big-runners}", "runner group"),
        ("{group: g, labels: ubuntu-latest}", "runner group"),
        ("{}", "names no labels"),
        ("[]", "names no labels"),
        ("big-box", "must name hosted"),
        ("[ubuntu-latest, big-box]", "must name hosted"),
        ("''", "must name hosted"),
        ("[[ubuntu-latest]]", "must name hosted"),
        ("ubuntu-", "must name hosted"),
        ("ubuntu-latest, big-box", "must name hosted"),
    ] {
        refused(&format!("    runs-on: {v}\n"), want);
    }
    refused("    steps: []\n", "without `runs-on`");
}

#[test]
fn a_remote_reusable_workflow_is_refused() {
    // Catches: a job whose runners live in another repository's file.
    refused(
        &format!("    uses: o/r/.github/workflows/w.yml@{SHA} # v1\n"),
        "reusable workflow outside",
    );
}

#[test]
fn forms_the_lint_does_not_evaluate_are_refused() {
    // Catches: a tag, merge key, repeated key, second document or alias bomb read some
    // way GitHub might not read it.
    refused("    !!str runs-on: ubuntu-latest\n", "YAML tag");
    refused("    runs-on: !custom ubuntu-latest\n", "YAML tag");
    refused("    runs-on: !!seq [ubuntu-latest]\n", "YAML tag");
    refused("    runs-on: !m {labels: ubuntu-latest}\n", "YAML tag");
    refused(
        "    runs-on: ubuntu-latest\n    RUNS-ON: big-box\n",
        "is repeated",
    );
    refused_text(
        "on: push\npermissions: {}\nPermissions: {contents: write}\njobs: {}\n",
        "is repeated",
    );
    refused_text(
        "x: &b {runs-on: big-box}\non: push\npermissions: {}\njobs:\n  a:\n    <<: *b\n    runs-on: ubuntu-latest\n",
        "merge key",
    );
    refused_text(
        "on: push\npermissions: {}\njobs: {}\n---\non: pull_request\n",
        "second YAML document",
    );
    refused(
        "    ? [a]\n    : b\n    runs-on: ubuntu-latest\n",
        "not a scalar",
    );
    refused_text(
        "on: push\npermissions: {}\njobs: {}\nrun_on: x\n",
        "unknown top-level key",
    );
    refused_text("on: [push\n", "not valid YAML");
    refused_text("", "no YAML document");
    refused_text("on: {}\npermissions: {}\njobs: {}\n", "names no trigger");
    refused_text("on: [[push]]\npermissions: {}\njobs: {}\n", "not a name");
}

#[test]
fn an_alias_bomb_is_refused() {
    // Catches: a loader that expands aliases without a budget.
    let mut w = String::from(
        "on: push\npermissions: {}\njobs: {}\nenv:\n  l0: &l0 [x, x, x, x, x, x, x, x, x, x]\n",
    );
    for i in 1..7 {
        let refs = vec![format!("*l{}", i - 1); 10].join(", ");
        w.push_str(&format!("  l{i}: &l{i} [{refs}]\n"));
    }
    refused_text(&w, "expands to more than");
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
        let found = scan(&workflow(&format!(
            "    runs-on: ubuntu-latest\n    steps:\n      - uses: {v} # v4\n"
        )));
        assert_eq!(found.len(), 1, "{v}: {found:?}");
        assert!(found[0].contains("not pinned"), "{v}");
    }
}

#[test]
fn pins_need_a_version_comment() {
    // Catches: a bare SHA, which hides which release it is, including a comment that
    // only looks like one because it sits on another line or inside quotes.
    for step in [
        format!("      - uses: a/b@{SHA}\n"),
        format!("      - uses: a/b@{SHA} #\n"),
        format!("      - uses: a/b@{SHA}\n        # v1\n"),
        format!("      - name: 'é # v1'\n        uses: a/b@{SHA}\n"),
    ] {
        let found = scan(&workflow(&format!(
            "    runs-on: ubuntu-latest\n    steps:\n{step}"
        )));
        assert_eq!(found.len(), 1, "{step}: {found:?}");
        assert!(found[0].contains("version comment"), "{step}");
    }
    // Catches: character offsets used as byte offsets after a multi-byte character.
    clean(&workflow(&format!(
        "    runs-on: ubuntu-latest\n    steps:\n      - {{name: é, uses: ./x}}\n      - uses: a/b@{SHA} # v1\n"
    )));
    // A quoted value's span runs past its comment, so only a plain value is read.
    refused(
        &format!("    runs-on: ubuntu-latest\n    steps:\n      - uses: 'a/b@{SHA}' # v1\n"),
        "must be a plain scalar",
    );
}

#[test]
fn top_level_permissions_are_required_and_empty() {
    // Catches: a workflow whose jobs inherit the default token rights, or start with some.
    let found = scan("on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    permissions: {}\n");
    assert_eq!(
        found,
        vec!["line 0: no top-level `permissions: {}`".to_owned()]
    );
    refused_text(
        "on: push\npermissions: {contents: write}\njobs: {}\n",
        "must be `{}`",
    );
    refused_text(
        "on: push\npermissions: write-all\njobs: {}\n",
        "must be `{}`",
    );
}

#[test]
fn regression_a_custom_label_with_a_hosted_prefix_was_accepted() {
    // The bypass the verifier found: `is_hosted_label` matched `ubuntu-`, `windows-`
    // and `macos-` by prefix, so a custom label no hosted runner carries passed, and
    // GitHub could only give the job to a self-hosted runner. `macos-13` is a retired
    // image, so it is a custom label now too.
    for v in [
        "ubuntu-gpu-farm",
        "[ubuntu-latest, ubuntu-farm]",
        "{labels: [macos-my-mac-mini]}",
        "windows-big-box",
        "macos-13",
    ] {
        refused(&format!("    runs-on: {v}\n"), "must name hosted");
    }
}

#[test]
fn more_than_one_label_is_refused() {
    // Catches: a list of hosted labels accepted; no hosted runner carries two labels,
    // so only a self-hosted runner could match it.
    for v in [
        "[ubuntu-latest, ubuntu-24.04]",
        "{labels: [macos-15, macos-latest]}",
    ] {
        refused(&format!("    runs-on: {v}\n"), "more than one label");
    }
}

#[test]
fn every_hosted_label_passes() {
    // Catches: a table entry the check does not accept, in each runs-on form.
    for label in HOSTED_LABELS {
        for v in [
            (*label).to_owned(),
            format!("[{label}]"),
            format!("{{labels: {label}}}"),
        ] {
            clean(&workflow(&format!("    runs-on: {v}\n")));
        }
    }
}

#[test]
fn workflow_run_is_refused_in_every_trigger_form() {
    // Catches: a lint that lets through a trigger that runs with the base
    // repository's secrets, like pull_request_target.
    for on in [
        "on: workflow_run",
        "on: [push, workflow_run]",
        "on:\n  workflow_run:\n    workflows: [ci]",
        "on: {Workflow_Run: {workflows: [ci]}}",
    ] {
        refused_text(
            &format!("{on}\npermissions: {{}}\njobs: {{}}\n"),
            "triggers on workflow_run",
        );
    }
}

#[test]
fn job_permissions_may_not_widen_the_token() {
    // Catches: a job that gives itself write scopes back after the top-level `{}`.
    for p in [
        "write-all",
        "read-all",
        "{contents: write}",
        "{contents: read, pull-requests: write}",
        "{id-token: write}",
        "''",
    ] {
        refused(
            &format!("    runs-on: ubuntu-latest\n    permissions: {p}\n"),
            "a job's `permissions`",
        );
    }
    for p in ["{}", "{contents: read}", "{contents: read, actions: none}"] {
        clean(&workflow(&format!(
            "    runs-on: ubuntu-latest\n    permissions: {p}\n"
        )));
    }
}

#[test]
fn pull_request_target_is_refused_outside_on() {
    // Catches: a lint that looks for pull_request_target only among the triggers,
    // so a job gated on it (as if the workflow ran for that event) or a value naming
    // it passed.
    for body in [
        "    if: github.event_name == 'pull_request_target'\n    runs-on: ubuntu-latest\n",
        "    runs-on: ubuntu-latest\n    env:\n      EVENT: Pull_Request_Target\n",
    ] {
        refused(body, "names pull_request_target");
    }
}

#[test]
fn a_trigger_name_with_blanks_is_refused() {
    // Catches: a trigger compared without trimming, so a quoted name with a blank
    // GitHub might trim was not read as the refused trigger.
    for on in ["on: [\"workflow_run \"]", "on: {' workflow_run': {}}"] {
        refused_text(
            &format!("{on}\npermissions: {{}}\njobs: {{}}\n"),
            "triggers on workflow_run",
        );
    }
}

/// A job whose steps are `steps` (each a full `- ...` line).
fn job_steps(steps: &str) -> String {
    workflow(&format!("    runs-on: ubuntu-latest\n    steps:\n{steps}"))
}

#[test]
fn a_local_action_must_be_a_linted_action_directory() {
    // Catches: `./` exempt from every check, so a step ran a directory holding no
    // linted action file (or one outside the repository).
    for dir in ["local", "local/", ".github/a", "x/"] {
        clean(&job_steps(&format!("      - uses: ./{dir}\n")));
    }
    for dir in [
        "missing",
        "local/sub",
        "../local",
        "./local",
        ".//local",
        "x/../local",
        "",
    ] {
        let w = job_steps(&format!("      - uses: ./{dir}\n"));
        refused_text(&w, "holding a linted action file");
    }
    // The root counts only when it holds an action file.
    let root = Repo {
        action_dirs: vec![String::new()],
        ..Repo::default()
    };
    assert_eq!(
        super::scan(&job_steps("      - uses: ./\n"), &root),
        Vec::<String>::new()
    );
}

#[test]
fn a_local_reusable_workflow_must_be_a_linted_workflow() {
    // Catches: a job-level `./` call to a file the workflow lint never read.
    refused(
        "    uses: ./.github/workflows/other.yml\n",
        "not a linted workflow file",
    );
}

#[test]
fn a_local_action_after_a_foreign_checkout_is_refused() {
    // Catches: `./x` run from a workspace that a step filled with another repository,
    // whichever order the steps come in and however the key is spelled.
    let checkout = format!("      - uses: actions/checkout@{SHA} # v7\n        with:\n");
    for steps in [
        format!("{checkout}          repository: o/r\n      - uses: ./local\n"),
        format!("      - uses: ./local\n{checkout}          Repository: o/r\n"),
        format!(
            "{checkout}          repository: ${{{{ github.repository }}}}\n      - uses: ./x\n"
        ),
    ] {
        refused_text(&job_steps(&steps), "checks out another repository");
    }
    clean(&job_steps(&format!(
        "{checkout}          persist-credentials: false\n      - uses: ./local\n"
    )));
}

/// Lints an action file against [`repo`].
fn action(text: &str) -> Vec<String> {
    scan_action(text, &repo())
}

fn action_refused(text: &str, want: &str) {
    let found = action(text);
    assert!(
        found.iter().any(|m| m.contains(want)),
        "want `{want}`:\n{text}\n-> {found:?}"
    );
}

#[test]
fn action_files_follow_the_step_rules() {
    // Catches: a composite action whose steps escape the pin, local-path and word
    // rules that hold in a workflow.
    let ok = format!(
        "name: a\ndescription: d\nruns:\n  using: composite\n  steps:\n    - uses: a/b@{SHA} # v1\n    - uses: ./local\n    - run: echo\n      shell: bash\n"
    );
    assert_eq!(action(&ok), Vec::<String>::new());
    for (steps, want) in [
        ("    - uses: a/b@v1\n".to_owned(), "not pinned"),
        (format!("    - uses: a/b@{SHA}\n"), "version comment"),
        ("    - uses: ./missing\n".to_owned(), "linted action file"),
        (
            format!(
                "    - uses: actions/checkout@{SHA} # v7\n      with: {{repository: o/r}}\n    - uses: ./x\n"
            ),
            "checks out another repository",
        ),
        (
            "    - run: echo pull_request_target\n".to_owned(),
            "names pull_request_target",
        ),
    ] {
        action_refused(
            &format!("name: a\nruns:\n  using: composite\n  steps:\n{steps}"),
            want,
        );
    }
}

#[test]
fn action_files_refuse_what_they_cannot_evaluate() {
    // Catches: an action read as clean when its runtime pulls an unpinned image, its
    // shape is unknown, or it has no steps to check.
    let digest = "0".repeat(64);
    for runs in [
        "{using: node24, main: index.js}".to_owned(),
        "{using: docker, image: Dockerfile}".to_owned(),
        format!("{{using: docker, image: 'docker://alpine@sha256:{digest}'}}"),
    ] {
        assert_eq!(
            action(&format!("name: a\nruns: {runs}\n")),
            Vec::<String>::new(),
            "{runs}"
        );
    }
    for (text, want) in [
        (
            "runs: {using: docker, image: 'docker://alpine:3'}\n",
            "neither a local",
        ),
        (
            "runs: {using: docker, image: 'docker://alpine@sha256:ab'}\n",
            "neither a local",
        ),
        (
            "runs: {using: docker, image: '${{ inputs.i }}'}\n",
            "neither a local",
        ),
        ("runs: {using: docker}\n", "without `image`"),
        ("runs: {using: composite}\n", "without `steps`"),
        ("runs: {using: wasm}\n", "must be composite"),
        ("runs: [x]\n", "must be a mapping"),
        ("name: a\n", "no `runs:`"),
        ("runs: {using: node24}\non: push\n", "unknown top-level key"),
        ("[a]\n", "an action must be a mapping"),
    ] {
        action_refused(text, want);
    }
}

#[test]
fn unreadable_bytes_are_refused_and_a_bom_is_dropped() {
    // Catches: a file skipped (and so never linted) because it holds a NUL or is not
    // UTF-8, and a BOM read as part of the first key.
    let w = workflow("    runs-on: ubuntu-latest\n");
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend(w.as_bytes());
    let text = decode(&bom).expect("a UTF-8 BOM is dropped");
    assert_eq!(scan(text), Vec::<String>::new());
    let mut nul = w.clone().into_bytes();
    nul.push(0);
    assert!(decode(&nul).unwrap_err().contains("NUL byte"));
    assert!(
        decode(b"on: \xFF\n")
            .unwrap_err()
            .contains("not valid UTF-8")
    );
    // UTF-16 holds NUL bytes for every ASCII character.
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(w.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert!(decode(&utf16).is_err());
}

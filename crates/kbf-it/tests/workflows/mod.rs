//! Lint for GitHub Actions workflow files.
//!
//! CI for this repository runs on GitHub-hosted runners only and never runs pull
//! request code with repository write access. The lint parses each workflow with a
//! YAML parser (`saphyr-parser`, through [`yaml`]) and evaluates the document. It
//! refuses a workflow if:
//! - `on` names the `pull_request_target` or `workflow_run` trigger (both run with
//!   the base repository's secrets and token), in any form (a string, a list or a
//!   mapping, block or flow);
//! - a job's `runs-on` is not exactly one GitHub-hosted runner label from
//!   [`HOSTED_LABELS`], written out literally: a string, a one-entry list, or a
//!   mapping holding only `labels:` with such a value. A runner group, an expression
//!   (`${{ }}`, so a matrix too), a custom label (even one that starts like a hosted
//!   label, such as `ubuntu-gpu`) or a second label could select a non-hosted runner,
//!   so each is refused because the lint cannot tell where it lands;
//! - a job's `permissions` grants anything but `read` or `none`;
//! - a job calls a reusable workflow outside this repository (its runners are not in
//!   this file);
//! - a step's `uses:` is not pinned to a full 40-character commit SHA followed on the
//!   same line by a version comment, written as a plain (unquoted) scalar so the
//!   comment's place is exact (local `./` actions are exempt);
//! - it lacks a top-level `permissions: {}`, so jobs start with no token rights;
//! - any key or value names `self-hosted` or `pull_request_target`, whatever key it
//!   sits under (a second line of defence behind the structural checks).
//!
//! Keys are matched ignoring ASCII case. So that no spelling is read one way here and
//! another way by GitHub, it also refuses what it does not evaluate: a YAML tag, a
//! second document, a merge key (`<<`), a key that is not a scalar, a key repeated in
//! one mapping (ignoring case), an unknown top-level key, a trigger list or `on`
//! value of an unexpected shape, and a file that does not parse.
//!
//! What this lint cannot cover: GitHub routes a job to a self-hosted runner whose
//! labels match before a hosted one, so a self-hosted runner registered with the label
//! `ubuntu-latest` would take a job this lint accepts. The control for that is the
//! repository (or organization) setting that disallows self-hosted runners; this lint
//! keeps the workflow files from asking for one. [`HOSTED_LABELS`] is a copy of
//! GitHub's published table: when GitHub retires an image its label becomes a custom
//! label, so drop it here when it is retired.

mod yaml;

use yaml::{Node, Value};

/// The keys GitHub reads at the top of a workflow file.
const TOP_LEVEL_KEYS: &[&str] = &[
    "name",
    "run-name",
    "on",
    "permissions",
    "env",
    "defaults",
    "concurrency",
    "jobs",
];

/// The GitHub-hosted runner labels, exactly as GitHub's runner table spells them
/// (preview images left out). Anything else names a custom label.
const HOSTED_LABELS: &[&str] = &[
    "ubuntu-slim",
    "ubuntu-latest",
    "ubuntu-26.04",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "ubuntu-26.04-arm",
    "ubuntu-24.04-arm",
    "ubuntu-22.04-arm",
    "windows-latest",
    "windows-2025",
    "windows-2025-vs2026",
    "windows-2022",
    "windows-11-arm",
    "windows-11-vs2026-arm",
    "macos-latest",
    "macos-26",
    "macos-15",
    "macos-14",
    "macos-26-intel",
    "macos-15-intel",
];

/// Triggers that run with the base repository's secrets and token on behalf of code
/// or events from outside it.
const REFUSED_TRIGGERS: &[&str] = &["pull_request_target", "workflow_run"];

/// Parses one workflow text and returns one message per problem, each prefixed with
/// its 1-based line number (0 for a whole-file problem).
pub fn scan(text: &str) -> Vec<String> {
    let root = match yaml::load(text) {
        Ok(root) => root,
        Err(e) => return vec![e],
    };
    let mut lint = Lint {
        text,
        out: Vec::new(),
    };
    lint.every_node(&root);
    lint.workflow(&root);
    lint.out
}

struct Lint<'a> {
    text: &'a str,
    out: Vec<String>,
}

impl Lint<'_> {
    fn refuse(&mut self, node: &Node, what: impl std::fmt::Display) {
        self.out.push(format!("line {}: {what}", node.line()));
    }

    /// Checks that hold everywhere: key shapes, and the two words that are never allowed.
    fn every_node(&mut self, node: &Node) {
        match &node.value {
            Value::Scalar { text, .. } => {
                let lower = text.to_ascii_lowercase();
                for word in ["self-hosted", "pull_request_target"] {
                    if lower.contains(word) {
                        self.refuse(node, format_args!("a key or value names {word}"));
                    }
                }
            }
            Value::Seq(items) => items.iter().for_each(|n| self.every_node(n)),
            Value::Map(entries) => {
                let mut seen: Vec<String> = Vec::new();
                for (k, v) in entries {
                    match &k.value {
                        Value::Scalar { text, plain } => {
                            if *plain && text == "<<" {
                                self.refuse(k, "a merge key (`<<`) is not evaluated");
                            }
                            let folded = text.to_ascii_lowercase();
                            if seen.contains(&folded) {
                                self.refuse(k, format_args!("the key `{text}` is repeated"));
                            }
                            seen.push(folded);
                        }
                        _ => self.refuse(k, "a key that is not a scalar is not evaluated"),
                    }
                    self.every_node(k);
                    self.every_node(v);
                }
            }
        }
    }

    fn workflow(&mut self, root: &Node) {
        let Value::Map(top) = &root.value else {
            self.refuse(root, "a workflow must be a mapping");
            return;
        };
        for (k, _) in top {
            let known = k
                .as_str()
                .is_some_and(|key| TOP_LEVEL_KEYS.iter().any(|t| key.eq_ignore_ascii_case(t)));
            if !known {
                self.refuse(k, "an unknown top-level key is not evaluated");
            }
        }
        match get(top, "permissions") {
            Some(p) if p.is_empty_map() => {}
            Some(p) => self.refuse(p, "top-level `permissions` must be `{}`"),
            None => self
                .out
                .push("line 0: no top-level `permissions: {}`".to_owned()),
        }
        match get(top, "on") {
            Some(on) => self.triggers(on),
            None => self.out.push("line 0: no `on:` triggers".to_owned()),
        }
        match get(top, "jobs") {
            Some(Node {
                value: Value::Map(jobs),
                ..
            }) => {
                for (_, job) in jobs {
                    self.job(job);
                }
            }
            Some(other) => self.refuse(other, "`jobs` must be a mapping"),
            None => self.out.push("line 0: no `jobs:`".to_owned()),
        }
    }

    fn triggers(&mut self, on: &Node) {
        let names: Vec<&Node> = match &on.value {
            Value::Scalar { .. } => vec![on],
            Value::Seq(items) => items.iter().collect(),
            Value::Map(entries) => entries.iter().map(|(k, _)| k).collect(),
        };
        if names.is_empty() {
            self.refuse(on, "`on` names no trigger");
        }
        for name in names {
            match name.as_str() {
                Some(n) => {
                    let n = n.trim();
                    if let Some(t) = REFUSED_TRIGGERS.iter().find(|t| n.eq_ignore_ascii_case(t)) {
                        self.refuse(name, format_args!("triggers on {t}"));
                    }
                }
                None => self.refuse(name, "a trigger that is not a name is not evaluated"),
            }
        }
    }

    fn job(&mut self, job: &Node) {
        let Value::Map(job_map) = &job.value else {
            self.refuse(job, "a job must be a mapping");
            return;
        };
        if let Some(called) = get(job_map, "uses")
            && !called.as_str().is_some_and(|w| w.starts_with("./"))
        {
            self.refuse(
                called,
                "a reusable workflow outside this repository runs where this lint cannot see",
            );
        }
        if let Some(p) = get(job_map, "permissions") {
            self.job_permissions(p);
        }
        match get(job_map, "runs-on") {
            Some(r) => self.runs_on(r),
            None if get(job_map, "uses").is_some() => {}
            None => self.refuse(job, "a job without `runs-on` is not evaluated"),
        }
        match get(job_map, "steps") {
            Some(Node {
                value: Value::Seq(steps),
                ..
            }) => {
                for step in steps {
                    self.step(step);
                }
            }
            Some(other) => self.refuse(other, "`steps` must be a list"),
            None => {}
        }
    }

    fn runs_on(&mut self, r: &Node) {
        match &r.value {
            Value::Scalar { .. } => self.hosted_label(r),
            Value::Seq(labels) if !labels.is_empty() => {
                labels.iter().for_each(|l| self.hosted_label(l));
                if labels.len() > 1 {
                    self.refuse(
                        r,
                        "runs-on lists more than one label, which no hosted runner carries",
                    );
                }
            }
            Value::Map(entries) => {
                for (k, v) in entries {
                    if k.as_str().is_some_and(|k| k.eq_ignore_ascii_case("labels")) {
                        self.runs_on(v);
                    } else {
                        self.refuse(
                            k,
                            "runs-on names a runner group or key that may select a non-hosted runner",
                        );
                    }
                }
                if !entries
                    .iter()
                    .any(|(k, _)| k.as_str().is_some_and(|k| k.eq_ignore_ascii_case("labels")))
                {
                    self.refuse(r, "runs-on names no labels");
                }
            }
            Value::Seq(_) => self.refuse(r, "runs-on names no labels"),
        }
    }

    /// A job may narrow its token to `read` or `none` scopes; it may not widen it.
    fn job_permissions(&mut self, p: &Node) {
        let Value::Map(scopes) = &p.value else {
            self.refuse(
                p,
                "a job's `permissions` must be a mapping of `read` or `none` scopes",
            );
            return;
        };
        for (scope, level) in scopes {
            if !level.as_str().is_some_and(|l| l == "read" || l == "none") {
                self.refuse(
                    level,
                    format_args!(
                        "a job's `permissions` may grant only `read` or `none`, got `{}: {}`",
                        scope.as_str().unwrap_or("?"),
                        level.as_str().unwrap_or("?"),
                    ),
                );
            }
        }
    }

    fn hosted_label(&mut self, label: &Node) {
        match label.as_str() {
            Some(v) if v.contains("${{") => self.refuse(
                label,
                format_args!("runs-on from an expression cannot be evaluated, got `{v}`"),
            ),
            Some(v) if is_hosted_label(v) => {}
            Some(v) => self.refuse(
                label,
                format_args!("runs-on must name hosted runner labels only, got `{v}`"),
            ),
            None => self.refuse(label, "runs-on must name hosted runner labels only"),
        }
    }

    fn step(&mut self, step: &Node) {
        let Value::Map(step_map) = &step.value else {
            self.refuse(step, "a step must be a mapping");
            return;
        };
        let Some(uses) = get(step_map, "uses") else {
            return;
        };
        let Some(v) = uses.as_str() else {
            self.refuse(uses, "`uses` must be a string");
            return;
        };
        if v.starts_with("./") {
            return;
        }
        if !matches!(uses.value, Value::Scalar { plain: true, .. }) {
            self.refuse(
                uses,
                format_args!("`{v}` must be a plain scalar, so its version comment can be read"),
            );
        } else if !is_sha_pinned(v) {
            self.refuse(
                uses,
                format_args!("`{v}` is not pinned to a full commit SHA"),
            );
        } else if trailing_comment(self.text, uses).is_none_or(|c| c.trim().is_empty()) {
            self.refuse(uses, format_args!("`{v}` needs a version comment"));
        }
    }
}

/// The value under `key` (ignoring ASCII case) in a mapping's entries.
fn get<'a>(entries: &'a [(Node, Node)], key: &str) -> Option<&'a Node> {
    entries
        .iter()
        .find(|(k, _)| k.as_str().is_some_and(|k| k.eq_ignore_ascii_case(key)))
        .map(|(_, v)| v)
}

/// The text of a comment that follows the plain scalar `node` on the line where it
/// ends, with nothing but blanks between them. (The parser ends a quoted scalar's span
/// after any comment that follows it, so only a plain scalar's end is exact.)
fn trailing_comment<'a>(text: &'a str, node: &Node) -> Option<&'a str> {
    // Marker indices count characters, not bytes.
    let end = node.span.end.index();
    let at = text.char_indices().nth(end).map_or(text.len(), |(i, _)| i);
    let line = text[at..].split('\n').next().unwrap_or_default();
    line.trim_start_matches([' ', '\t']).strip_prefix('#')
}

fn is_hosted_label(v: &str) -> bool {
    HOSTED_LABELS.contains(&v)
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
mod tests;

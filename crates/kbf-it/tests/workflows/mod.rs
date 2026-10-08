//! Lint for GitHub Actions workflow files and the repository's own action files.
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
//! - a job's `permissions` grants anything but `read` or `none`, except the two scopes
//!   that sign build provenance ([`SIGNING_SCOPES`]: `id-token` and `attestations`),
//!   which may be `write` only in a job whose `if:` is exactly [`MAIN_PUSH_ONLY`]. So
//!   no pull request or merge-queue run holds a token that can sign an attestation,
//!   and every attestation this repository signs names `refs/heads/main`;
//! - a job calls a reusable workflow that is not one of this repository's linted
//!   workflow files (its runners are not in a file this lint read);
//! - a step's `uses:` is not pinned to a full 40-character commit SHA followed on the
//!   same line by a version comment, written as a plain (unquoted) scalar so the
//!   comment's place is exact;
//! - a step's `uses:` names an action outside the set the repository's Actions policy
//!   allows ([`ALLOWED_OWNERS`], [`ALLOWED_REPOSITORIES`]): GitHub refuses such a
//!   workflow with `startup_failure` before any job runs, so a pull request that adds
//!   one leaves no job to go red. Owner and repository are compared exactly (GitHub
//!   reads them ignoring case; a respelling is refused here, the safe side);
//! - a step's local `uses: ./<dir>` does not name a directory of this repository
//!   holding a linted action file ([`Repo::action_dirs`]), or sits in a steps list
//!   where some step passes `repository:` under `with:` (a checkout of another
//!   repository, whose `./<dir>` this lint never read);
//! - it lacks a top-level `permissions: {}`, so jobs start with no token rights;
//! - any key or value names `self-hosted` or `pull_request_target`, whatever key it
//!   sits under (a second line of defence behind the structural checks).
//!
//! Every tracked `action.yml` or `action.yaml` (any directory, any case) is linted by
//! [`scan_action`] with the same step, local-path and whole-document rules: its
//! `runs.using` must be `composite` (whose steps are checked as above), `node*`
//! (code in the action's own directory) or `docker` with a local Dockerfile or a
//! `docker://` image pinned by `@sha256:` digest.
//!
//! Keys are matched ignoring ASCII case. So that no spelling is read one way here and
//! another way by GitHub, it also refuses what it does not evaluate: a YAML tag, a
//! second document, a merge key (`<<`), a key that is not a scalar, a key repeated in
//! one mapping (ignoring case), an unknown top-level key, a trigger list or `on`
//! value of an unexpected shape, and a file that does not parse. [`decode`] refuses a
//! file holding a NUL byte or invalid UTF-8 rather than skip it.
//!
//! # What this lint cannot cover
//!
//! - **Self-hosted runners with hosted labels.** GitHub routes a job to a self-hosted
//!   runner whose labels match before a hosted one, so a self-hosted runner registered
//!   with the label `ubuntu-latest` would take a job this lint accepts.
//! - **Imposter commits.** GitHub serves a commit from any fork of an action's
//!   repository under the parent's name, so `owner/action@<sha>` may name a commit the
//!   owner never published. The lint checks the shape of a pin, not where the commit
//!   came from; the reviewer of a pin change checks that the SHA is on the action's
//!   own tagged release.
//! - **Code a step fetches at run time.** A `run:` script can clone or download
//!   anything into the workspace before a local `./` action runs.
//! - **Retired images.** [`HOSTED_LABELS`] is a copy of GitHub's published table:
//!   when GitHub retires an image its label becomes a custom label, so drop it here
//!   when it is retired.
//!
//! The real guarantee does not come from this file. It rests on two repository
//! settings: no self-hosted runner registered for the repository (or allowed to it by
//! the organization), and an approving review required for every change to workflow
//! and action files before it merges. This lint keeps those files from asking for
//! anything else, so that review has less to catch.

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

/// The keys GitHub reads at the top of an action file.
const ACTION_KEYS: &[&str] = &[
    "name",
    "author",
    "description",
    "inputs",
    "outputs",
    "runs",
    "branding",
];

/// The owners whose every action the repository's Actions policy allows.
const ALLOWED_OWNERS: &[&str] = &["actions"];

/// Further `owner/repository` actions the repository's Actions policy allows.
const ALLOWED_REPOSITORIES: &[&str] = &["tailscale/github-action"];

/// Token scopes a job may set to `write` when it runs only for a push to `main`: the
/// OIDC token and the attestations API, which together sign build provenance.
const SIGNING_SCOPES: &[&str] = &["id-token", "attestations"];

/// The one `if:` spelling under which a job may hold [`SIGNING_SCOPES`] for writing.
const MAIN_PUSH_ONLY: &str =
    "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// Triggers that run with the base repository's secrets and token on behalf of code
/// or events from outside it.
const REFUSED_TRIGGERS: &[&str] = &["pull_request_target", "workflow_run"];

/// Parses one workflow text and returns one message per problem, each prefixed with
/// its 1-based line number (0 for a whole-file problem).
pub fn scan(text: &str, repo: &Repo) -> Vec<String> {
    run(text, repo, |lint, root| lint.workflow(root))
}

/// Parses one action file (`action.yml`) and returns its problems as [`scan`] does.
pub fn scan_action(text: &str, repo: &Repo) -> Vec<String> {
    run(text, repo, |lint, root| lint.action(root))
}

/// What a local `uses: ./...` may name: the repository's linted files.
#[derive(Debug, Default)]
pub struct Repo {
    /// Directories, relative to the root and without a trailing `/` (`""` for the root
    /// itself), that hold a linted `action.yml` or `action.yaml`.
    pub action_dirs: Vec<String>,
    /// Paths, relative to the root, of the linted workflow files.
    pub workflows: Vec<String>,
}

/// The text of a workflow or action file: refuses a file holding a NUL byte or one
/// that is not valid UTF-8, and drops a leading UTF-8 byte order mark.
pub fn decode(bytes: &[u8]) -> Result<&str, String> {
    if bytes.contains(&0) {
        return Err("line 0: the file holds a NUL byte, so it is not read".to_owned());
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    std::str::from_utf8(bytes)
        .map_err(|e| format!("line 0: the file is not valid UTF-8 ({e}), so it is not read"))
}

fn run(text: &str, repo: &Repo, check: impl FnOnce(&mut Lint<'_>, &Node)) -> Vec<String> {
    let root = match yaml::load(text) {
        Ok(root) => root,
        Err(e) => return vec![e],
    };
    let mut lint = Lint {
        text,
        repo,
        out: Vec::new(),
    };
    lint.every_node(&root);
    check(&mut lint, &root);
    lint.out
}

struct Lint<'a> {
    text: &'a str,
    repo: &'a Repo,
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
        self.known_keys(top, TOP_LEVEL_KEYS);
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

    fn known_keys(&mut self, top: &[(Node, Node)], keys: &[&str]) {
        for (k, _) in top {
            let known = k
                .as_str()
                .is_some_and(|key| keys.iter().any(|t| key.eq_ignore_ascii_case(t)));
            if !known {
                self.refuse(k, "an unknown top-level key is not evaluated");
            }
        }
    }

    fn action(&mut self, root: &Node) {
        let Value::Map(top) = &root.value else {
            self.refuse(root, "an action must be a mapping");
            return;
        };
        self.known_keys(top, ACTION_KEYS);
        let Some(runs) = get(top, "runs") else {
            self.out.push("line 0: no `runs:`".to_owned());
            return;
        };
        let Value::Map(r) = &runs.value else {
            self.refuse(runs, "`runs` must be a mapping");
            return;
        };
        let using = get(r, "using")
            .and_then(Node::as_str)
            .map(str::to_ascii_lowercase);
        match using.as_deref() {
            Some("composite") => match get(r, "steps") {
                Some(steps) => self.steps(steps),
                None => self.refuse(runs, "a composite action without `steps`"),
            },
            Some("docker") => match get(r, "image").and_then(Node::as_str) {
                Some(image) if is_docker_image_pinned(image) => {}
                Some(image) => self.refuse(
                    runs,
                    format_args!(
                        "the image `{image}` is neither a local Dockerfile nor pinned by `@sha256:` digest"
                    ),
                ),
                None => self.refuse(runs, "a docker action without `image`"),
            },
            Some(u) if u.starts_with("node") => {}
            _ => self.refuse(runs, "`runs.using` must be composite, node or docker"),
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
        if let Some(called) = get(job_map, "uses") {
            match called.as_str().and_then(|w| w.strip_prefix("./")) {
                None => self.refuse(
                    called,
                    "a reusable workflow outside this repository runs where this lint cannot see",
                ),
                Some(path) if self.repo.workflows.iter().any(|w| w == path) => {}
                Some(path) => self.refuse(
                    called,
                    format_args!("`./{path}` is not a linted workflow file of this repository"),
                ),
            }
        }
        if let Some(p) = get(job_map, "permissions") {
            let main_push_only = get(job_map, "if").and_then(Node::as_str) == Some(MAIN_PUSH_ONLY);
            self.job_permissions(p, main_push_only);
        }
        match get(job_map, "runs-on") {
            Some(r) => self.runs_on(r),
            None if get(job_map, "uses").is_some() => {}
            None => self.refuse(job, "a job without `runs-on` is not evaluated"),
        }
        if let Some(steps) = get(job_map, "steps") {
            self.steps(steps);
        }
    }

    /// Checks each step. A local `./` action reads the workspace, so it is refused in a
    /// steps list where any step checks out another repository (`with: repository:`).
    fn steps(&mut self, steps: &Node) {
        let Value::Seq(list) = &steps.value else {
            self.refuse(steps, "`steps` must be a list");
            return;
        };
        let foreign = list.iter().find_map(checkout_repository);
        for step in list {
            self.step(step, foreign);
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

    /// A job may narrow its token to `read` or `none` scopes; it may not widen it, but
    /// for [`SIGNING_SCOPES`] set to `write` in a job that runs only for a push to
    /// `main` (`main_push_only`). Scope names and levels are compared exactly, so
    /// another spelling is refused (the safe side).
    fn job_permissions(&mut self, p: &Node, main_push_only: bool) {
        let Value::Map(scopes) = &p.value else {
            self.refuse(
                p,
                "a job's `permissions` must be a mapping of `read` or `none` scopes",
            );
            return;
        };
        for (scope, level) in scopes {
            let name = scope.as_str().unwrap_or("?");
            let granted = level.as_str().unwrap_or("?");
            if granted == "read" || granted == "none" {
                continue;
            }
            if granted != "write" || !SIGNING_SCOPES.contains(&name) {
                self.refuse(
                    level,
                    format_args!(
                        "a job's `permissions` may grant only `read` or `none`, got `{name}: {granted}`"
                    ),
                );
            } else if !main_push_only {
                self.refuse(
                    level,
                    format_args!(
                        "`{name}: write` is allowed only in a job whose `if:` is exactly `{MAIN_PUSH_ONLY}`"
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

    fn step(&mut self, step: &Node, foreign: Option<&Node>) {
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
        if let Some(rest) = v.strip_prefix("./") {
            if let Some(checkout) = foreign {
                self.refuse(
                    uses,
                    format_args!(
                        "`{v}` reads the workspace, and the step on line {} checks out another repository there",
                        checkout.line()
                    ),
                );
            } else if !local_dir(rest).is_some_and(|d| self.repo.action_dirs.iter().any(|a| a == d))
            {
                self.refuse(
                    uses,
                    format_args!(
                        "`{v}` is not a directory of this repository holding a linted action file"
                    ),
                );
            }
            return;
        }
        if !is_allowed_action(v) {
            self.refuse(
                uses,
                format_args!(
                    "`{v}` is not an action the repository's Actions policy allows (actions/* or tailscale/github-action)"
                ),
            );
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

/// The `repository:` key a step passes under `with:`, if any.
fn checkout_repository(step: &Node) -> Option<&Node> {
    let Value::Map(step_map) = &step.value else {
        return None;
    };
    match &get(step_map, "with")?.value {
        Value::Map(with) => with
            .iter()
            .find(|(k, _)| {
                k.as_str()
                    .is_some_and(|k| k.eq_ignore_ascii_case("repository"))
            })
            .map(|(k, _)| k),
        _ => None,
    }
}

/// The directory a local `uses: ./<rest>` names, without a trailing `/`; `None` for a
/// path with an empty, `.` or `..` component or a backslash.
fn local_dir(rest: &str) -> Option<&str> {
    let dir = rest.strip_suffix('/').unwrap_or(rest);
    let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains('\\');
    (dir.is_empty() || dir.split('/').all(plain)).then_some(dir)
}

/// A docker action's image is a local Dockerfile path or `docker://...@sha256:<hex>`.
fn is_docker_image_pinned(image: &str) -> bool {
    if image.contains("${{") {
        return false;
    }
    match image.strip_prefix("docker://") {
        Some(reference) => reference
            .rsplit_once("@sha256:")
            .is_some_and(|(name, hex)| !name.is_empty() && is_lower_hex(hex, 64)),
        None => !image.contains("://"),
    }
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn is_hosted_label(v: &str) -> bool {
    HOSTED_LABELS.contains(&v)
}

/// Whether the remote action `owner/repository[/path][@ref]` is one the repository's
/// Actions policy allows.
fn is_allowed_action(v: &str) -> bool {
    let name = v.split_once('@').map_or(v, |(name, _)| name);
    let mut parts = name.splitn(3, '/');
    let (Some(owner), Some(repository)) = (parts.next(), parts.next()) else {
        return false;
    };
    !repository.is_empty()
        && (ALLOWED_OWNERS.contains(&owner)
            || ALLOWED_REPOSITORIES
                .iter()
                .any(|r| r.split_once('/') == Some((owner, repository))))
}

fn is_sha_pinned(v: &str) -> bool {
    match v.rsplit_once('@') {
        Some((action, sha)) => !action.is_empty() && is_lower_hex(sha, 40),
        None => false,
    }
}

#[cfg(test)]
mod tests;

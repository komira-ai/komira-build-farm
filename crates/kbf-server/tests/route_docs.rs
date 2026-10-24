//! The drift guard: every route the operator router serves has a row in `docs/api.md` and in
//! `docs/design/operator-verbs.md`, method and path both.
//!
//! The routes are read from the `.route(` calls of this crate's sources, constants resolved
//! within their own file, so a route added to any of the merged routers goes red here until the
//! two documents name it. The custom-method route `/v1/nodes/{target}` is expanded into one key
//! per verb that `write_request` in `api.rs` accepts (`:cordon`, `:drain`, `:uncordon`), and the
//! `:verb` stays in the key on the document side, so each verb needs its own row. Proven by:
//! - `an_undocumented_route_is_reported` (a planted route, a planted method),
//! - `a_custom_verb_needs_its_own_row` (a document naming only `:cordon`),
//! - deleting the `GET /healthz` row of `operator-verbs.md`, or one of its three `/v1/nodes/`
//!   verb rows, which turns `every_route_is_in_the_verbs_table` red (see the pull request for
//!   the runs).
//!
//! **The scan is textual**, not a parse of the router. It sees only `get`, `post`, `put` and
//! `delete` handlers inside a `.route(` call; it does not see `patch`, `head`, `any`, `.nest`,
//! `.fallback` or `.merge` of a router built elsewhere, so a route added that way is invisible
//! to it. A `.route(` whose path is neither a literal nor a `const &str` of the same file
//! panics. The document side is textual too: any `METHOD /path` in the file counts, prose
//! included, so a sentence that mentions a route satisfies the guard whether or not a table row
//! exists. The route count is pinned exactly (`ROUTES`), so a new route also asks for that
//! number to move.
//!
//! What it does not check: the gate column of the verbs table (that lives in the handlers) and
//! the reverse direction (a documented row with no route, which `[V]` and `[N]` rows are by
//! design).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

type Route = (String, String);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("tests.rs")
        {
            out.push(path);
        }
    }
}

/// `{name}` becomes `{}`; a trailing `:verb` stays, so `/v1/nodes/{node}:cordon` and
/// `/v1/nodes/{node}:drain` are different keys.
fn normalise(path: &str) -> String {
    let mut out = String::new();
    let mut in_brace = false;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'*') => {
                in_brace = true;
                out.push_str("{*rest}");
            }
            '{' => {
                in_brace = true;
                out.push_str("{}");
            }
            '}' => in_brace = false,
            _ if in_brace => {}
            _ => out.push(c),
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every `const NAME: &str = "...";` of one file.
fn consts(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(at) = line.find("const ") else {
            continue;
        };
        let rest = &line[at + 6..];
        let Some((name, value)) = rest.split_once(": &str = \"") else {
            continue;
        };
        if let Some((value, _)) = value.split_once('"') {
            out.push((name.trim().to_string(), value.to_string()));
        }
    }
    out
}

/// The `(METHOD, normalised path)` of every `.route(` call in `src`.
fn code_routes(src: &str) -> Vec<Route> {
    let uncommented: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let consts = consts(&uncommented);
    let mut routes = Vec::new();
    let mut rest = uncommented.as_str();
    while let Some(at) = rest.find(".route(") {
        let args = &rest[at + 7..];
        rest = args;
        // The arguments up to the matching close paren, strings skipped.
        let mut depth = 1;
        let mut in_str = false;
        let mut end = args.len();
        for (i, c) in args.char_indices() {
            match c {
                '"' => in_str = !in_str,
                '(' if !in_str => depth += 1,
                ')' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let args = &args[..end];
        let (first, handlers) = args
            .split_once(',')
            .expect("a route has a path and handlers");
        let first = first.trim();
        let path = match first.strip_prefix('"') {
            Some(lit) => lit.trim_end_matches('"').to_string(),
            None => consts
                .iter()
                .find(|(name, _)| name == first)
                .unwrap_or_else(|| {
                    panic!("route path `{first}` is neither a literal nor a const of its file")
                })
                .1
                .clone(),
        };
        let path = normalise(&path);
        for method in ["get", "post", "put", "delete"] {
            let needle = format!("{method}(");
            let mut from = 0;
            while let Some(i) = handlers[from..].find(&needle) {
                let i = from + i;
                let before = handlers[..i].chars().next_back();
                if !before.is_some_and(is_ident) {
                    routes.push((method.to_uppercase(), path.clone()));
                }
                from = i + needle.len();
            }
        }
    }
    routes
}

/// The `(METHOD, normalised path)` pairs a document writes as `METHOD /path`.
fn doc_routes(text: &str) -> BTreeSet<Route> {
    let mut out = BTreeSet::new();
    for method in ["GET", "POST", "PUT", "DELETE"] {
        let needle = format!("{method} /");
        let mut from = 0;
        while let Some(i) = text[from..].find(&needle) {
            let i = from + i;
            from = i + needle.len();
            if text[..i].chars().next_back().is_some_and(is_ident) {
                continue;
            }
            let path: String = text[i + method.len() + 1..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !"`?,)\"'".contains(*c))
                .collect();
            out.insert((
                method.to_string(),
                normalise(path.trim_end_matches(['.', ':'])),
            ));
        }
    }
    out
}

/// The routes of `code` that `doc` does not name. A catch-all `{*rest}` is named by any row under
/// its prefix.
fn missing(code: &[Route], doc: &BTreeSet<Route>) -> Vec<Route> {
    code.iter()
        .filter(|(method, path)| match path.strip_suffix("/{*rest}") {
            Some(prefix) => !doc
                .iter()
                .any(|(m, p)| m == method && p.starts_with(&format!("{prefix}/"))),
            None => !doc.contains(&(method.clone(), path.clone())),
        })
        .cloned()
        .collect()
}

/// The path whose last segment is `{node}:{verb}`, served by one `.route(` and one handler.
const VERB_ROUTE: &str = "/v1/nodes/{}";

/// The verbs `write_request` matches: the string literals that start a `match` arm in that
/// function.
fn node_verbs(api_src: &str) -> Vec<String> {
    let start = api_src
        .find("pub fn write_request(")
        .expect("api.rs has write_request");
    let body = &api_src[start..];
    let body = &body[..body.find("\n}\n").expect("write_request ends")];
    let mut verbs: Vec<String> = body
        .lines()
        .map(str::trim_start)
        .filter_map(|l| l.strip_prefix('"'))
        .filter(|l| l.contains("=>"))
        .filter_map(|l| l.split_once('"').map(|(v, _)| v.to_string()))
        .collect();
    verbs.sort();
    verbs.dedup();
    verbs
}

/// Replaces the `POST /v1/nodes/{}` of `routes` by one `POST /v1/nodes/{}:verb` per verb.
fn expand_verbs(routes: Vec<Route>, verbs: &[String]) -> Vec<Route> {
    let post_verb = ("POST".to_string(), VERB_ROUTE.to_string());
    assert!(
        routes.contains(&post_verb),
        "the router has no `POST {VERB_ROUTE}` any more: update this guard"
    );
    routes
        .into_iter()
        .flat_map(|r| {
            if r == post_verb {
                verbs
                    .iter()
                    .map(|v| ("POST".to_string(), format!("{VERB_ROUTE}:{v}")))
                    .collect()
            } else {
                vec![r]
            }
        })
        .collect()
}

/// The one `POST` of `/v1/object-stores/connections/{target}` is the probe `{name}:verify`: the
/// router captures the whole segment and the handler splits the verb off, so the documents name
/// it with the `:verify`.
const VERIFY_ROUTE: &str = "/v1/object-stores/connections/{}";

/// Renames the `POST` of `VERIFY_ROUTE` to `POST VERIFY_ROUTE:verify`.
fn name_verify(routes: Vec<Route>) -> Vec<Route> {
    let post = ("POST".to_string(), VERIFY_ROUTE.to_string());
    assert!(
        routes.contains(&post),
        "the router has no `POST {VERIFY_ROUTE}` any more: update this guard"
    );
    routes
        .into_iter()
        .map(|r| {
            if r == post {
                ("POST".to_string(), format!("{VERIFY_ROUTE}:verify"))
            } else {
                r
            }
        })
        .collect()
}

/// The exact number of `(method, path)` routes, verbs expanded. A new route moves it.
const ROUTES: usize = 54;

fn server_routes() -> Vec<Route> {
    let mut files = Vec::new();
    rust_files(&root().join("crates/kbf-server/src"), &mut files);
    let mut routes: Vec<Route> = files
        .iter()
        .flat_map(|f| code_routes(&fs::read_to_string(f).unwrap()))
        .collect();
    routes.sort();
    routes.dedup();
    let verbs =
        node_verbs(&fs::read_to_string(root().join("crates/kbf-server/src/api.rs")).unwrap());
    assert_eq!(
        verbs,
        ["cordon", "drain", "uncordon"],
        "write_request accepts other verbs: the guard and the documents need a row each"
    );
    let mut routes = name_verify(expand_verbs(routes, &verbs));
    routes.sort();
    routes.dedup();
    routes
}

fn check(doc_path: &str) {
    let doc = doc_routes(&fs::read_to_string(root().join(doc_path)).unwrap());
    let routes = server_routes();
    assert_eq!(
        routes.len(),
        ROUTES,
        "the router has {} routes, the guard pins {ROUTES}: update `ROUTES` with the documents",
        routes.len()
    );
    let missing = missing(&routes, &doc);
    assert!(
        missing.is_empty(),
        "{doc_path} does not name these routes: {missing:?}"
    );
}

#[test]
fn every_route_is_in_the_api_doc() {
    check("docs/api.md");
}

fn title(heading: &str) -> String {
    heading.split(" (").next().unwrap_or(heading).to_string()
}

/// `##` headings (outside code fences) that appear more than once, an annotation after the
/// title (`(**planned**)`) not told apart: the planned stub and the real section of one
/// route are the same section.
fn duplicate_headings(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut dup = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with("## ") && !seen.insert(title(line)) {
            dup.push(line.to_string());
        }
    }
    dup
}

#[test]
fn the_api_doc_names_each_section_once() {
    let text = fs::read_to_string(root().join("docs/api.md")).unwrap();
    let dup = duplicate_headings(&text);
    assert!(
        dup.is_empty(),
        "docs/api.md repeats these sections: {dup:?}"
    );
}

#[test]
fn a_repeated_section_heading_is_reported() {
    let text = "## A\ntext\n```\n## A\n```\n## B\n## A (planned)\n## B\n";
    assert_eq!(
        duplicate_headings(text),
        vec!["## A (planned)".to_string(), "## B".to_string()]
    );
}

#[test]
fn every_route_is_in_the_verbs_table() {
    check("docs/design/operator-verbs.md");
}

#[test]
fn an_undocumented_route_is_reported() {
    let src = r#"
        const PLANTED: &str = "/v1/planted/{id}";
        fn router() -> Router {
            Router::new()
                .route("/v1/nodes", get(a))
                .route(PLANTED, get(b).post(c))
                .route("/v1/catch/{id}/{*rest}", get(d))
        }
    "#;
    let code = code_routes(src);
    assert_eq!(
        code,
        [
            ("GET".to_string(), "/v1/nodes".to_string()),
            ("GET".to_string(), "/v1/planted/{}".to_string()),
            ("POST".to_string(), "/v1/planted/{}".to_string()),
            ("GET".to_string(), "/v1/catch/{}/{*rest}".to_string()),
        ]
    );
    let doc =
        doc_routes("`GET /v1/nodes` and `GET /v1/planted/{name}` and `GET /v1/catch/{x}/logs`");
    // POST is the only one missing: the method is part of the key, and the catch-all is named by
    // a row under its prefix.
    assert_eq!(
        missing(&code, &doc),
        [("POST".to_string(), "/v1/planted/{}".to_string())]
    );
    assert_eq!(missing(&code, &doc_routes("`GET /v1/nodes`")).len(), 3);
}

#[test]
fn a_custom_verb_needs_its_own_row() {
    let verbs: Vec<String> = ["cordon", "drain"].map(String::from).into();
    let code = expand_verbs(
        code_routes(r#"Router::new().route("/v1/nodes/{target}", post(act))"#),
        &verbs,
    );
    assert_eq!(code.len(), 2);
    let doc = doc_routes("`POST /v1/nodes/{node}:cordon` and `:drain`");
    assert_eq!(
        missing(&code, &doc),
        [("POST".to_string(), "/v1/nodes/{}:drain".to_string())]
    );
}

#[test]
fn the_verbs_come_from_the_handler() {
    let src = "pub fn write_request() {\n    let a = match verb {\n        \"x\" => 1,\n        \"y\" if z => 2,\n        _ => 3,\n    };\n}\nfn other() { \"w\" => 0 }\n";
    assert_eq!(node_verbs(src), ["x", "y"]);
}

#[test]
fn the_doc_reader_keeps_verbs_and_strips_queries_and_punctuation() {
    let doc = doc_routes(
        "`POST /v1/nodes/{node}:cordon`, `GET /v1/shapes/{key}?instance=&cfg=` and GET /healthz.",
    );
    let want: BTreeSet<Route> = [
        ("POST", "/v1/nodes/{}:cordon"),
        ("GET", "/v1/shapes/{}"),
        ("GET", "/healthz"),
    ]
    .map(|(m, p)| (m.to_string(), p.to_string()))
    .into();
    assert_eq!(doc, want);
}

#[test]
fn the_probe_route_needs_its_verify_row() {
    let code = name_verify(code_routes(
        r#"Router::new().route("/v1/object-stores/connections/{target}", get(a).post(b))"#,
    ));
    assert_eq!(
        code,
        [
            (
                "GET".to_string(),
                "/v1/object-stores/connections/{}".to_string()
            ),
            (
                "POST".to_string(),
                "/v1/object-stores/connections/{}:verify".to_string()
            ),
        ]
    );
    let doc = doc_routes(
        "`GET /v1/object-stores/connections/{name}` and `POST /v1/object-stores/connections/{name}`",
    );
    assert_eq!(
        missing(&code, &doc),
        [(
            "POST".to_string(),
            "/v1/object-stores/connections/{}:verify".to_string()
        )]
    );
}

#[test]
fn the_ui_design_names_no_route_the_server_dropped() {
    let text = fs::read_to_string(root().join("docs/design/kbf-ui-v1.md")).unwrap();
    assert!(
        !text.contains("update/hosts"),
        "docs/design/kbf-ui-v1.md names `update/hosts`, a route the server no longer serves"
    );
}

#[test]
fn the_rollout_rules_cite_no_removed_desired_route_or_moved_file() {
    let text = fs::read_to_string(root().join("docs/design/frontend-proxy.md")).unwrap();
    for stale in ["update/age.rs", "returns 409 otherwise"] {
        assert!(
            !text.contains(stale),
            "docs/design/frontend-proxy.md still says `{stale}`: the desired-release route is gone and the age check lives in release/age.rs"
        );
    }
}

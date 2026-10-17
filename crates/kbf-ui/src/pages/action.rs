//! The Action page: one action's answer, with its stdout and stderr as text when the
//! blob route links them and they are small.

use super::{
    action_label, attempt_end, build_page, bytes, definitions, failed, invocation_url, micros,
    outcome, pill, problem, segment,
};
use crate::api::{Action, Attempt, Blob, Output};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, PREFIX, Source};

/// Action: the action's answer and the log blobs it links.
pub(crate) const ACTION: Page = Page {
    name: "Action",
    reads: &[
        "/v1/invocations/{id}/actions/{digest}",
        "/v1/blobs/{digest}",
    ],
};

/// The largest log shown on the page; a larger one is a link. The read API's tails
/// (`docs/design/invocations-api.md` 4.7) will replace this read when they land.
pub(crate) const LOG_SHOWN_MAX_BYTES: u64 = 64 * 1024;

/// A log as the page shows it.
enum Log {
    /// The answer has none.
    None,
    /// The blob route is off: no link.
    NoRoute,
    /// The store no longer holds it.
    Gone,
    /// Too large to show, or of a size the digest does not say: a link, when the
    /// API's `href` is one.
    Large(Option<Href>, Option<u64>),
    /// Its text, or why it could not be read.
    Read(Result<String, ReadError>),
}

/// The Action page of action `digest` of build `id`.
pub(crate) async fn action<S: Source>(source: &S, id: &str, digest: &str) -> Answer {
    let here = format!("{}/actions/{}", build_page(id), segment(digest));
    let mut r = Reader::new(source, &ACTION);
    let url = format!("{}/actions/{}", invocation_url(id), segment(digest));
    let a: Action = match r.json(&url).await {
        Ok(a) => a,
        Err(e) => return failed(&ACTION, &here, r.asked(), &e),
    };
    let stdout = log(&mut r, a.stdout.as_ref()).await;
    let stderr = log(&mut r, a.stderr.as_ref()).await;
    let label = action_label(
        a.row.target.as_ref(),
        a.row.mnemonic.as_ref(),
        &a.row.action,
    );
    let body = layout::page(ACTION.name, &here, r.asked(), |h| {
        let builds = Href::root_relative(PREFIX).expect("the prefix is root-relative");
        let build = Href::root_relative(&build_page(id)).expect("a build page is root-relative");
        h.el("p", &[attr("class", "crumbs")], |h| {
            h.link(&builds, "Builds").text(" / ").link(&build, id);
        });
        h.el("h1", &[], |h| {
            h.text(&label);
        });
        let (words, class) = outcome(a.row.outcome.as_ref());
        h.el("p", &[], |h| pill(h, &words, class));
        let mut fields = vec![
            ("Action", a.row.action.clone()),
            ("Instance", a.row.instance.clone()),
            ("How", a.row.how.clone()),
            (
                "Command",
                a.command.clone().unwrap_or_else(|| "none".to_owned()),
            ),
        ];
        if let Some(u) = &a.usage {
            fields.extend([
                ("Wall", micros(u.wall_micros)),
                ("CPU user", micros(u.cpu_user_micros)),
                ("CPU system", micros(u.cpu_system_micros)),
                ("Peak memory", bytes(u.peak_memory_bytes)),
            ]);
        }
        definitions(h, &fields);
        attempts(h, &a.row.attempts);
        outputs(h, &a.outputs, a.outputs_truncated);
        show_log(h, "stdout", &stdout);
        show_log(h, "stderr", &stderr);
    });
    Answer::html(200, body)
}

/// Reads `blob`'s text through its `href` when it is held, linked and small.
async fn log<S: Source>(r: &mut Reader<'_, S>, blob: Option<&Blob>) -> Log {
    let Some(blob) = blob else {
        return Log::None;
    };
    if blob.available == Some(false) {
        return Log::Gone;
    }
    let Some(href) = &blob.href else {
        return Log::NoRoute;
    };
    match blob.size_bytes() {
        Some(size) if size <= LOG_SHOWN_MAX_BYTES => Log::Read(r.text(href).await),
        size => Log::Large(Href::root_relative(href), size),
    }
}

fn show_log(h: &mut Html, name: &str, log: &Log) {
    h.el("section", &[attr("class", "log")], |h| {
        h.el("h2", &[], |h| {
            h.text(name);
        });
        match log {
            Log::None => note(h, "None recorded."),
            Log::NoRoute => note(h, "Not linked: the blob route is off on this server."),
            Log::Gone => note(h, "No longer held by the store."),
            Log::Large(href, size) => {
                h.el("p", &[], |h| {
                    let words = size.map_or_else(
                        || "Its size is not known, so it is not shown here. ".to_owned(),
                        |size| format!("{} is too large to show here. ", bytes(size)),
                    );
                    h.text(&words);
                    if let Some(href) = href {
                        h.link(href, "Open the whole log");
                    }
                });
            }
            Log::Read(Ok(text)) => {
                h.el("pre", &[], |h| {
                    h.text(text);
                });
            }
            Log::Read(Err(e)) => problem(h, e),
        }
    });
}

fn note(h: &mut Html, words: &str) {
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(words);
    });
}

fn attempts(h: &mut Html, attempts: &[Attempt]) {
    h.el("h2", &[], |h| {
        h.text("Attempts");
    });
    if attempts.is_empty() {
        note(
            h,
            "None: the action did not run here (a cache hit, or joined another).",
        );
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in ["Lease", "Worker", "Queued", "Started", "Ended", "End"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for a in attempts {
                let cells = [
                    a.lease.clone(),
                    a.worker.clone(),
                    a.queued.clone(),
                    a.started.clone().unwrap_or_default(),
                    a.ended.clone().unwrap_or_default(),
                    attempt_end(a),
                ];
                h.el("tr", &[], |h| {
                    for cell in &cells {
                        h.el("td", &[], |h| {
                            h.text(cell);
                        });
                    }
                });
            }
        });
    });
}

fn outputs(h: &mut Html, outputs: &[Output], truncated: bool) {
    h.el("h2", &[], |h| {
        h.text("Outputs");
    });
    if outputs.is_empty() {
        note(h, "None.");
        return;
    }
    h.el("ul", &[attr("class", "outputs")], |h| {
        for o in outputs {
            let size = o.blob.size_bytes().map_or_else(String::new, bytes);
            let kind = if o.directory { "directory" } else { "file" };
            let link = o
                .blob
                .href
                .as_deref()
                .filter(|_| o.blob.available != Some(false))
                .and_then(Href::root_relative);
            h.el("li", &[], |h| {
                h.el("code", &[], |h| match &link {
                    Some(href) => {
                        h.link(href, &o.path);
                    }
                    None => {
                        h.text(&o.path);
                    }
                });
                h.text(&format!(" {kind} {size}"));
            });
        }
    });
    if truncated {
        note(h, "More outputs than the record keeps: the list is cut.");
    }
}

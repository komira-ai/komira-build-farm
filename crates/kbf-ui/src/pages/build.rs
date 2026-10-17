//! The Build page: one build's summary and a page of its actions.

use super::{
    action_label, attempt_end, build_page, failed, invocation_url, outcome, pill, query_value,
    segment, summary,
};
use crate::api::{ActionsPage, Invocation, Row};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, PREFIX, Source};

/// Build: the summary and `/actions`.
pub(crate) const BUILD: Page = Page {
    name: "Build",
    reads: &["/v1/invocations/{id}", "/v1/invocations/{id}/actions"],
};

/// The Build page of build `id`; `?page_token=` is passed to `/actions`.
pub(crate) async fn build<S: Source>(source: &S, id: &str, query: &str) -> Answer {
    let here = build_page(id);
    let token = query_value(query, "page_token");
    let mut r = Reader::new(source, &BUILD);
    let docs = load(&mut r, id, token.as_deref()).await;
    let (inv, page) = match docs {
        Ok(docs) => docs,
        Err(e) => return failed(&BUILD, &here, r.asked(), &e),
    };
    let body = layout::page(BUILD.name, &here, r.asked(), |h| {
        crumbs(h);
        h.el("h1", &[], |h| {
            h.text("Build ").el("code", &[], |h| {
                h.text(&inv.id);
            });
        });
        summary(h, &inv);
        h.el("h2", &[], |h| {
            h.text("Actions");
        });
        actions(h, &inv.id, &page.actions);
        if let Some(token) = &page.next_page_token {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("page_token", token);
            let next = Href::root_relative(&format!("{here}?{}", query.finish()))
                .expect("a build page's path is root-relative");
            h.el("p", &[attr("class", "pager")], |h| {
                h.link(&next, "Next page");
            });
        }
    });
    Answer::html(200, body)
}

async fn load<S: Source>(
    r: &mut Reader<'_, S>,
    id: &str,
    token: Option<&str>,
) -> Result<(Invocation, ActionsPage), ReadError> {
    let base = invocation_url(id);
    let inv: Invocation = r.json(&base).await?;
    let mut url = format!("{base}/actions");
    if let Some(token) = token {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("page_token", token);
        url = format!("{url}?{}", query.finish());
    }
    let page: ActionsPage = r.json(&url).await?;
    Ok((inv, page))
}

fn crumbs(h: &mut Html) {
    let builds = Href::root_relative(PREFIX).expect("the prefix is root-relative");
    h.el("p", &[attr("class", "crumbs")], |h| {
        h.link(&builds, "Builds");
    });
}

/// The rows of one page of `/actions`, each linking to its Action page.
fn actions(h: &mut Html, id: &str, rows: &[Row]) {
    if rows.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No actions on this page.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Action",
                    "Outcome",
                    "How",
                    "Attempts",
                    "Last attempt",
                    "Instance",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for row in rows {
                let page = format!("{}/actions/{}", build_page(id), segment(&row.action));
                let href =
                    Href::root_relative(&page).expect("an action page's path is root-relative");
                let label = action_label(row.target.as_ref(), row.mnemonic.as_ref(), &row.action);
                let (words, class) = outcome(row.outcome.as_ref());
                let last = row.attempts.last().map_or_else(String::new, |a| {
                    format!("{} on {}", attempt_end(a), a.worker)
                });
                h.el("tr", &[], |h| {
                    h.el("td", &[], |h| {
                        h.link(&href, &label);
                    });
                    h.el("td", &[], |h| pill(h, &words, class));
                    for cell in [
                        &row.how,
                        &row.attempts.len().to_string(),
                        &last,
                        &row.instance,
                    ] {
                        h.el("td", &[], |h| {
                            h.text(cell);
                        });
                    }
                });
            }
        });
    });
}

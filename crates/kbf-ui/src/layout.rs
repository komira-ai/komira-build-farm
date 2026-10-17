//! The shell every page sits in: head, navigation, and the list of API URLs the page
//! asked for.

use crate::html::{Href, Html, attr, url};
use crate::read::Asked;
use crate::{PREFIX, STYLESHEET};

/// The navigation: each section's label and page. More sections come with their
/// routes (nodes, queue, tests).
const NAV: &[(&str, &str)] = &[("Builds", PREFIX)];

/// A URL the page asked for, as the page lists it.
struct Link<'a> {
    text: &'a str,
    sent: bool,
    /// The link to it: only for a URL that was sent and is root-relative. A refused
    /// URL may name anything.
    href: Option<Href>,
}

/// A whole page: `title`, at UI path `here`, which asked for `asked`, its content
/// written by `content`.
pub(crate) fn page(
    title: &str,
    here: &str,
    asked: &[Asked],
    content: impl FnOnce(&mut Html),
) -> String {
    let stylesheet =
        Href::root_relative(STYLESHEET).expect("the stylesheet's path is root-relative");
    let links: Vec<Link<'_>> = asked
        .iter()
        .map(|a| Link {
            text: &a.url,
            sent: a.sent,
            href: a.sent.then(|| Href::root_relative(&a.url)).flatten(),
        })
        .collect();
    let mut h = Html::document();
    h.el("html", &[attr("lang", "en")], |h| {
        h.el("head", &[], |h| {
            h.void("meta", &[attr("charset", "utf-8")]);
            h.void(
                "meta",
                &[
                    attr("name", "viewport"),
                    attr("content", "width=device-width, initial-scale=1"),
                ],
            );
            h.el("title", &[], |h| {
                h.text(title).text(" - kbf");
            });
            h.void(
                "link",
                &[attr("rel", "stylesheet"), url("href", &stylesheet)],
            );
            // For an agent reading the HTML: the JSON this page was made from.
            for href in links.iter().filter_map(|l| l.href.as_ref()) {
                h.void(
                    "link",
                    &[
                        attr("rel", "alternate"),
                        attr("type", "application/json"),
                        url("href", href),
                    ],
                );
            }
        });
        h.el("body", &[], |h| {
            nav(h, here);
            h.el("main", &[], content);
            reads(h, &links);
        });
    });
    h.finish()
}

fn nav(h: &mut Html, here: &str) {
    h.el("header", &[attr("class", "top")], |h| {
        h.el("nav", &[attr("aria-label", "sections")], |h| {
            h.el("span", &[attr("class", "brand")], |h| {
                h.text("kbf");
            });
            h.el("ul", &[], |h| {
                for (label, path) in NAV {
                    let href =
                        Href::root_relative(path).expect("a section's path is root-relative");
                    h.el("li", &[], |h| {
                        if *path == here {
                            h.el(
                                "a",
                                &[url("href", &href), attr("aria-current", "page")],
                                |h| {
                                    h.text(label);
                                },
                            );
                        } else {
                            h.link(&href, label);
                        }
                    });
                }
            });
        });
    });
}

/// The footer: every URL the page asked the API for, each a link to its JSON; a
/// refused one is text, marked as not read.
fn reads(h: &mut Html, links: &[Link<'_>]) {
    h.el("footer", &[attr("class", "reads")], |h| {
        h.el("h2", &[], |h| {
            h.text("Read from the API");
        });
        if links.is_empty() {
            h.el("p", &[], |h| {
                h.text("Nothing: this page read no route.");
            });
            return;
        }
        h.el("ul", &[], |h| {
            for link in links {
                h.el("li", &[], |h| {
                    h.el("code", &[], |h| match &link.href {
                        Some(href) => {
                            h.link(href, link.text);
                        }
                        None => {
                            h.text(link.text);
                        }
                    });
                    if !link.sent {
                        h.text(" (not read: not a route this page declares)");
                    }
                });
            }
        });
    });
}

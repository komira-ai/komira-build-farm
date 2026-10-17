//! The pages of `kbf-ui` over this server's real read API, through a listener: the
//! documents the `/v1` routes answer are the ones the pages parse
//! (`docs/design/ui-data.md` 6.1), and what a build printed reaches the page as text
//! (section 8). The pages are not mounted on the server yet; the source here asks
//! the operator API over TCP as a browser's request would reach it.

mod pages;

use std::sync::Mutex;

use kbf_invocation::Outcome;
use kbf_ui::{CONTENT_SECURITY_POLICY, Fetched, Source, SourceError};
use pages::{OPEN, Server, at, digest, finished, granted, listing, queued_target, started, text};

/// The operator API as the pages' source, remembering each URL and its status.
struct Api<'a> {
    server: &'a Server,
    answered: Mutex<Vec<(String, u16)>>,
}

impl Source for Api<'_> {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        let answer = self.server.get(url).await;
        self.answered
            .lock()
            .expect("not poisoned")
            .push((url.to_owned(), answer.code));
        Ok(Fetched {
            status: answer.code,
            body: answer.body,
            headers: Vec::new(),
        })
    }
}

/// Catches: a key the pages read that the routes do not answer, or answer in another
/// form (the page would be `502`, "a document this page cannot read"); a page URL the
/// routes do not serve; the stderr blob, read through the blob route's own link,
/// written as markup; the target label or failure message written as markup.
#[tokio::test]
async fn the_pages_render_from_the_real_routes() {
    let server = Server::with(OPEN);
    let action = digest(7);
    let out = server.store(b"an object file").await;
    let stderr = server.store(b"<script>alert(1)</script>\n").await;
    let result = listing(digest(8), 1, &[out], (None, Some(stderr)));
    server
        .feed([
            queued_target("b1", "//app:<b>lib</b>", at(0), action, 1),
            granted(1, 1, "n4", at(1)),
            started(1, 1, at(2)),
            finished(
                1,
                1,
                Outcome::Failed {
                    code: 2,
                    message: "<img src=x onerror=alert(1)>".to_owned(),
                },
                Some(result),
                at(3),
            ),
        ])
        .await;
    let api = Api {
        server: &server,
        answered: Mutex::new(Vec::new()),
    };
    let action_page = format!("/ui/builds/b1/actions/{}", text(&action));
    let mut bodies = Vec::new();
    for (path, query) in [("/ui", "id=b1"), ("/ui/builds/b1", ""), (&action_page, "")] {
        let answer = kbf_ui::respond(&api, path, query).await;
        let html = String::from_utf8(answer.body().to_vec()).expect("UTF-8");
        assert_eq!(answer.status(), 200, "{path}: {html}");
        assert!(
            answer
                .headers()
                .contains(&("content-security-policy", CONTENT_SECURITY_POLICY)),
            "{path}"
        );
        bodies.push(html);
    }
    let answered = api.answered.lock().expect("not poisoned").clone();
    assert_eq!(answered.len(), 5, "{answered:?}");
    for (url, code) in &answered {
        assert_eq!(*code, 200, "{url}");
    }
    assert!(answered[4].0.starts_with("/v1/blobs/"), "{answered:?}");

    let build = &bodies[1];
    assert!(build.contains("//app:&lt;b&gt;lib&lt;/b&gt;"), "{build}");
    assert!(
        build.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "{build}"
    );
    let page = &bodies[2];
    assert!(
        page.contains("<pre>&lt;script&gt;alert(1)&lt;/script&gt;\n</pre>"),
        "{page}"
    );
    for html in &bodies {
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
    }
    server.stop().await;
}

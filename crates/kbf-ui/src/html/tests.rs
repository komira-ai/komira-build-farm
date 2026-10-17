//! The builder's guarantees: text and attribute values escaped, only allowlisted names,
//! URLs only root-relative.

use super::{Href, Html, attr, escape, url};

/// Catches: any of the five characters that end text or a quoted attribute value left
/// as it is (mutant: one `match` arm of `escape` removed).
#[test]
fn escape_writes_the_five_markup_characters_as_references() {
    assert_eq!(
        escape(r#"<a href="x" title='y'>&</a>"#),
        "&lt;a href=&quot;x&quot; title=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
    );
    assert_eq!(escape("plain text, é"), "plain text, é");
}

/// Catches: text or an attribute value written raw, so a value can close its quote
/// and add an attribute (mutant: `open` writing the value unescaped).
#[test]
fn text_and_attribute_values_are_escaped() {
    let mut h = Html::default();
    h.el("p", &[attr("title", "\" onclick=\"x")], |h| {
        h.text("<b>bold</b>");
    });
    assert_eq!(
        h.finish(),
        "<p title=\"&quot; onclick=&quot;x\">&lt;b&gt;bold&lt;/b&gt;</p>"
    );
}

#[test]
fn a_document_starts_with_the_doctype_and_void_elements_have_no_close() {
    let mut h = Html::document();
    h.void("meta", &[attr("charset", "utf-8")]);
    let href = Href::root_relative("/ui?a=1&b=2").expect("root-relative");
    h.link(&href, "x");
    assert_eq!(
        h.finish(),
        "<!DOCTYPE html>\n<meta charset=\"utf-8\"><a href=\"/ui?a=1&amp;b=2\">x</a>"
    );
}

/// Catches: a URL that leaves this server, or that a browser rewrites into one that
/// does, accepted as a link (mutants: each clause of `root_relative` removed).
#[test]
fn only_root_relative_urls_are_links() {
    for bad in [
        "",
        "ui",
        "javascript:alert(1)",
        "https://evil.example/",
        "//evil.example/",
        "/\\evil.example/",
        "/a\\b",
        "/\t/evil.example",
        "/a b",
        "/a\nb",
        "/a\u{7f}",
    ] {
        assert_eq!(Href::root_relative(bad), None, "{bad:?}");
    }
    for good in ["/", "/ui", "/v1/blobs/x-1?instance=a%20b&inline=text"] {
        assert_eq!(
            Href::root_relative(good).map(|h| h.0),
            Some(good.to_owned())
        );
    }
}

/// Catches: a page able to write a `<script>` or `<style>` element (mutant: the tag
/// allowlist check removed).
#[test]
#[should_panic(expected = "<script> is not an allowed element")]
fn a_script_element_is_refused() {
    Html::default().el("script", &[], |_| {});
}

/// Catches: an event handler or `style=` attribute written (mutant: the attribute
/// allowlist check removed).
#[test]
#[should_panic(expected = "onclick= is not an allowed attribute")]
fn an_event_handler_attribute_is_refused() {
    Html::default().el("p", &[attr("onclick", "x")], |_| {});
}

/// Catches: a URL attribute set from plain text, which would skip `Href`'s check
/// (mutant: one allowlist for both kinds).
#[test]
#[should_panic(expected = "href= is not an allowed attribute")]
fn href_takes_an_href_not_text() {
    Html::default().el("a", &[attr("href", "/x")], |_| {});
}

/// Catches: a text attribute name accepted as a URL attribute.
#[test]
#[should_panic(expected = "title= is not an allowed attribute")]
fn a_url_attribute_name_must_be_a_url_attribute() {
    let href = Href::root_relative("/x").expect("root-relative");
    Html::default().el("a", &[url("title", &href)], |_| {});
}

#[test]
#[should_panic(expected = "<input> has no body")]
fn a_void_element_has_no_body() {
    Html::default().el("input", &[], |_| {});
}

#[test]
#[should_panic(expected = "<p> is not a void element")]
fn only_void_elements_are_void() {
    Html::default().void("p", &[]);
}

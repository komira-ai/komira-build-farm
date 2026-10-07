//! The S3 XML bodies kbf reads: `ListBucketResult` (ListObjectsV2) and `Error`.

use roxmltree::{Document, Node};

use crate::{ListPage, ListToken, ObjectInfo, ObjectKey, ObjectStoreError};

/// The code and message of an S3 `<Error>` body; empty when the body is not one (a HEAD
/// answer has no body).
pub(crate) fn error_code_message(body: &[u8]) -> (String, String) {
    let Ok(text) = std::str::from_utf8(body) else {
        return (String::new(), String::new());
    };
    let Ok(doc) = Document::parse(text) else {
        return (String::new(), text.chars().take(200).collect());
    };
    let root = doc.root_element();
    (
        child_text(root, "Code").unwrap_or_default().to_owned(),
        child_text(root, "Message").unwrap_or_default().to_owned(),
    )
}

/// A ListObjectsV2 answer as a [`ListPage`].
pub(crate) fn list_page(body: &[u8]) -> Result<ListPage, ObjectStoreError> {
    let bad = |what: String| ObjectStoreError::Protocol(format!("ListObjectsV2 answer: {what}"));
    let text = std::str::from_utf8(body).map_err(|e| bad(e.to_string()))?;
    let doc = Document::parse(text).map_err(|e| bad(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(bad(format!("root element is <{}>", root.tag_name().name())));
    }
    let mut objects = Vec::new();
    for c in root.children().filter(|n| is_named(*n, "Contents")) {
        let key = child_text(c, "Key").ok_or_else(|| bad("<Contents> without <Key>".into()))?;
        let key = ObjectKey::new(key).map_err(|e| bad(format!("listed key: {e}")))?;
        let size = child_text(c, "Size")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad(format!("{key}: missing or bad <Size>")))?;
        objects.push(ObjectInfo { key, size });
    }
    let truncated = match child_text(root, "IsTruncated") {
        Some("true") => true,
        Some("false") | None => false,
        Some(other) => return Err(bad(format!("<IsTruncated> is {other:?}"))),
    };
    let next = match (truncated, child_text(root, "NextContinuationToken")) {
        (true, Some(t)) if !t.is_empty() => Some(ListToken(t.to_owned())),
        (true, _) => return Err(bad("truncated without <NextContinuationToken>".into())),
        (false, _) => None,
    };
    Ok(ListPage { objects, next })
}

/// Whether `node` is an element named `name`, in any namespace.
fn is_named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|n| is_named(*n, name))
        .map(|n| n.text().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns="http://s3.amazonaws.com/doc/2006-03-01/""#;

    /// Catches: a parser that drops the continuation token (so listing stops after one
    /// page), loses a key or size, or depends on the S3 namespace being absent.
    #[test]
    fn truncated_page() {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult {NS}>\
             <Name>b</Name><Prefix>p/</Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys>\
             <IsTruncated>true</IsTruncated><NextContinuationToken>tok/+=</NextContinuationToken>\
             <Contents><Key>p/a</Key><Size>3</Size><ETag>\"x\"</ETag></Contents>\
             <Contents><Key>p/b</Key><Size>0</Size></Contents></ListBucketResult>"
        );
        let page = list_page(body.as_bytes()).unwrap();
        assert_eq!(page.next, Some(ListToken("tok/+=".into())));
        let got: Vec<_> = page
            .objects
            .iter()
            .map(|o| (o.key.as_str(), o.size))
            .collect();
        assert_eq!(got, [("p/a", 3), ("p/b", 0)]);
    }

    /// Catches: a last page read as truncated, an empty listing refused, and a
    /// truncated answer with no token accepted (which would end a listing early).
    #[test]
    fn last_empty_and_broken_pages() {
        let last = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(
            list_page(last.as_bytes()).unwrap(),
            ListPage {
                objects: vec![],
                next: None
            }
        );
        let broken = "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>";
        assert!(matches!(
            list_page(broken.as_bytes()),
            Err(ObjectStoreError::Protocol(_))
        ));
        assert!(list_page(b"<Error><Code>X</Code></Error>").is_err());
        assert!(list_page(b"not xml").is_err());
    }

    /// Catches: an error body read without its code, which callers match on.
    #[test]
    fn error_body() {
        let body = "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code>\
                    <Message>The specified key does not exist.</Message></Error>";
        assert_eq!(
            error_code_message(body.as_bytes()),
            (
                "NoSuchKey".into(),
                "The specified key does not exist.".into()
            )
        );
        assert_eq!(error_code_message(b""), (String::new(), String::new()));
    }
}

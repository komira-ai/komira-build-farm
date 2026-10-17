//! The S3 XML bodies kbf reads: `ListBucketResult` (ListObjectsV2), `Error`, and the
//! answers of a multipart upload; and the one it writes, `CompleteMultipartUpload`.

use roxmltree::{Document, Node};

use crate::types::Token;
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
        (true, Some(t)) if !t.is_empty() => Some(ListToken(Token::Continue(t.to_owned()))),
        (true, _) => return Err(bad("truncated without <NextContinuationToken>".into())),
        (false, _) => None,
    };
    Ok(ListPage { objects, next })
}

/// The `UploadId` of a CreateMultipartUpload answer.
pub(crate) fn upload_id(body: &[u8]) -> Result<String, ObjectStoreError> {
    let bad =
        |what: String| ObjectStoreError::Protocol(format!("CreateMultipartUpload answer: {what}"));
    let text = std::str::from_utf8(body).map_err(|e| bad(e.to_string()))?;
    let doc = Document::parse(text).map_err(|e| bad(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "InitiateMultipartUploadResult" {
        return Err(bad(format!("root element is <{}>", root.tag_name().name())));
    }
    match child_text(root, "UploadId") {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(bad("no <UploadId>".into())),
    }
}

/// The body of CompleteMultipartUpload: each part's number and ETag, in order.
pub(crate) fn complete_request(etags: &[String]) -> String {
    let mut out =
        String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (i, etag) in etags.iter().enumerate() {
        out.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            i + 1,
            escape(etag)
        ));
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

/// A CompleteMultipartUpload answer of 200: success only if it is a
/// `CompleteMultipartUploadResult`. S3 answers 200 and then an `<Error>` when the
/// completion fails after the answer started.
pub(crate) fn complete_answer(body: &[u8]) -> Result<(), ObjectStoreError> {
    let text = std::str::from_utf8(body).unwrap_or_default();
    let root = Document::parse(text)
        .ok()
        .map(|doc| doc.root_element().tag_name().name().to_owned());
    match root.as_deref() {
        Some("CompleteMultipartUploadResult") => Ok(()),
        Some("Error") => {
            let (code, message) = error_code_message(body);
            Err(ObjectStoreError::Service {
                status: 200,
                code,
                message,
            })
        }
        _ => Err(ObjectStoreError::Protocol(format!(
            "CompleteMultipartUpload answered 200 with {:?}",
            text.chars().take(200).collect::<String>()
        ))),
    }
}

/// `text` with the characters XML gives meaning to escaped.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
        assert_eq!(page.next, Some(ListToken(Token::Continue("tok/+=".into()))));
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

    /// Catches: an upload id lost or taken from the wrong element (every part would be
    /// sent to an upload that does not exist), a completion that names parts out of
    /// order or unescaped, and a 200 completion whose body is an `<Error>` taken for
    /// success (an object reported written that the store does not hold).
    #[test]
    fn multipart_bodies() {
        let created = format!(
            "<?xml version=\"1.0\"?><InitiateMultipartUploadResult {NS}><Bucket>b</Bucket>\
             <Key>k</Key><UploadId>abc.def-1</UploadId></InitiateMultipartUploadResult>"
        );
        assert_eq!(upload_id(created.as_bytes()).unwrap(), "abc.def-1");
        assert!(upload_id(b"<InitiateMultipartUploadResult/>").is_err());
        let empty =
            b"<InitiateMultipartUploadResult><UploadId></UploadId></InitiateMultipartUploadResult>";
        assert!(upload_id(empty).is_err(), "an empty upload id");
        assert!(upload_id(b"\xff\xfe").is_err(), "not UTF-8");
        assert!(upload_id(b"<unclosed").is_err(), "not XML");
        assert!(upload_id(b"<Error><UploadId>x</UploadId></Error>").is_err());
        let body = complete_request(&["\"a1\"".into(), "b&2".into()]);
        assert!(
            body.contains(
                "<Part><PartNumber>1</PartNumber><ETag>\"a1\"</ETag></Part>\
                 <Part><PartNumber>2</PartNumber><ETag>b&amp;2</ETag></Part>"
            ),
            "{body}"
        );
        let done = format!(
            "<CompleteMultipartUploadResult {NS}><ETag>\"x-2\"</ETag></CompleteMultipartUploadResult>"
        );
        assert!(complete_answer(done.as_bytes()).is_ok());
        let late = b"<Error><Code>InternalError</Code><Message>m</Message></Error>";
        let late = complete_answer(late).err().map(|e| e.to_string());
        assert_eq!(late.as_deref(), Some("store answered 200 InternalError: m"));
        assert!(matches!(
            complete_answer(b""),
            Err(ObjectStoreError::Protocol(_))
        ));
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

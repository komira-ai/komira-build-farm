//! AWS Signature Version 4 for S3, header form, and the UTC date formats S3 uses.
//!
//! Only what kbf sends is covered: a signed payload hash, headers given by the caller,
//! and query parameters. The tests check it against the worked examples in the S3
//! SigV4 documentation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The SHA-256 of an empty payload, in hex.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What is signed for one request.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    /// `host` or `host:port`, exactly as the `Host` header carries it.
    pub host: &'a str,
    /// The path, already URI-encoded (kbf keys need no encoding; see `ObjectKey`).
    pub path: &'a str,
    /// Query parameters, not yet encoded.
    pub query: &'a [(&'a str, String)],
    /// Headers to sign besides `host`, with lowercase names. They must all be sent.
    pub headers: &'a [(&'a str, String)],
    /// Hex SHA-256 of the body, also sent as `x-amz-content-sha256`.
    pub payload_sha256: &'a str,
}

/// The signing identity and scope.
pub(crate) struct Signer<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
}

impl Signer<'_> {
    /// The `Authorization` header value for `req` at `amz_date` (`YYYYMMDDTHHMMSSZ`).
    pub fn authorization(&self, req: &Request<'_>, amz_date: &str) -> String {
        let date = &amz_date[..8];
        let scope = format!("{date}/{}/s3/aws4_request", self.region);

        let mut headers: Vec<(String, String)> = req
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
            .chain([("host".to_owned(), req.host.to_owned())])
            .collect();
        headers.sort();
        let signed_headers = headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

        let canonical_request = format!(
            "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
            req.method,
            req.path,
            canonical_query(req.query),
            req.payload_sha256
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let k_date = hmac(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key_id
        )
    }
}

/// The query string, sorted and encoded as SigV4 requires. The same string is sent.
pub(crate) fn canonical_query(query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes everything but the RFC 3986 unreserved characters, as SigV4 asks.
pub(crate) fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// A UTC calendar time to the second.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Utc {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u64,
    pub minute: u64,
    pub second: u64,
}

impl Utc {
    /// The calendar time of `t`, rounded up to the next whole second when `ceil` (so a
    /// retention date is never earlier than asked) and down otherwise.
    pub fn of(t: SystemTime, ceil: bool) -> Utc {
        let since = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let secs = since.as_secs() + u64::from(ceil && since.subsec_nanos() > 0);
        let (days, rem) = (secs / 86_400, secs % 86_400);
        let (year, month, day) = civil_from_days(days as i64);
        Utc {
            year,
            month,
            day,
            hour: rem / 3600,
            minute: rem % 3600 / 60,
            second: rem % 60,
        }
    }

    /// `YYYYMMDDTHHMMSSZ`, the `x-amz-date` form.
    pub fn amz(&self) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `YYYY-MM-DDTHH:MM:SSZ`, the ISO 8601 form Object Lock dates use.
    pub fn iso8601(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Year, month and day of a count of days since 1970-01-01 (proleptic Gregorian),
/// after Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example credentials from the S3 SigV4 documentation.
    const SIGNER: Signer<'static> = Signer {
        access_key_id: "AKIAIOSFODNN7EXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        region: "us-east-1",
    };
    const DATE: &str = "20130524T000000Z";

    fn signature(auth: &str) -> &str {
        auth.rsplit_once("Signature=").unwrap().1
    }

    /// Catches: any slip in the canonical request (header order, trimming, the empty
    /// query line, the payload hash), the string to sign or the key derivation; the
    /// expected signature is the documented one for "GET Object" with a Range header.
    #[test]
    fn documented_get_object_example() {
        let headers = [
            ("range", "bytes=0-9".to_owned()),
            ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
            ("x-amz-date", DATE.to_owned()),
        ];
        let auth = SIGNER.authorization(
            &Request {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
                headers: &headers,
                payload_sha256: EMPTY_SHA256,
            },
            DATE,
        );
        assert_eq!(
            signature(&auth),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, "
        ));
    }

    /// Catches: query parameters signed unsorted, unencoded, or with the wrong
    /// separator; the expected signature is the documented "GET Bucket (List Objects)"
    /// example.
    #[test]
    fn documented_list_objects_example() {
        let headers = [
            ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
            ("x-amz-date", DATE.to_owned()),
        ];
        let query = [("prefix", "J".to_owned()), ("max-keys", "2".to_owned())];
        let auth = SIGNER.authorization(
            &Request {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/",
                query: &query,
                headers: &headers,
                payload_sha256: EMPTY_SHA256,
            },
            DATE,
        );
        assert_eq!(
            signature(&auth),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    /// Catches: an encoder that leaves a reserved character (`/`, `+`, `=`, space) bare,
    /// which makes a continuation token sign differently from how it is sent.
    #[test]
    fn uri_encoding() {
        assert_eq!(uri_encode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(uri_encode("a/b+c= d"), "a%2Fb%2Bc%3D%20d");
        assert_eq!(
            canonical_query(&[("b", "2".into()), ("a", "x/y".into())]),
            "a=x%2Fy&b=2"
        );
    }

    /// Catches: calendar arithmetic off by a day at month, leap-year or century edges,
    /// and a retention date rounded down (earlier than asked).
    #[test]
    fn utc_formats() {
        let at = |s: u64, n: u32| UNIX_EPOCH + Duration::new(s, n);
        assert_eq!(Utc::of(at(0, 0), false).amz(), "19700101T000000Z");
        assert_eq!(Utc::of(at(1_369_353_600, 0), false).amz(), DATE);
        // 2000-02-29 (a leap day in a century year divisible by 400) at 23:59:59.
        assert_eq!(
            Utc::of(at(951_868_799, 0), false).iso8601(),
            "2000-02-29T23:59:59Z"
        );
        // 2100-03-01, after the non-leap 2100-02-28.
        assert_eq!(
            Utc::of(at(4_107_542_400, 0), false).iso8601(),
            "2100-03-01T00:00:00Z"
        );
        assert_eq!(Utc::of(at(59, 1), true).iso8601(), "1970-01-01T00:01:00Z");
        assert_eq!(Utc::of(at(59, 1), false).iso8601(), "1970-01-01T00:00:59Z");
    }
}

//! No message a daemon sends or receives names a cold store or its credential.
//!
//! A daemon-only node holds no cold-store credential and reaches bytes only through
//! storage (docs/design/architecture-v2.md, section 9). The flags keep the credential
//! off its command line and out of its environment (kbf-cli's tests), its unit loads
//! none (kbf-deploy's `linux/modes_tests.rs`), and the layering keeps the object-store
//! client out of the daemon crates (kbf-it's tests/layering.rs); this file keeps it off
//! the wire's schema: no message of any service in a package the daemon has clients for
//! (`DAEMON_PACKAGES`: the work stream and segments, storage's action store, ByteStream
//! and the REAPI services), and no message of the storage map (which `Welcome` and
//! `MapUpdate` are to carry), has a field or a type named for a cold store or its
//! credential, at any depth.
//!
//! The protos are compiled here as `build.rs` compiles them, and the walk starts from
//! every method of every service in those packages, so a new RPC or a new service there
//! is covered with no edit here.
//!
//! Limits: it reads names, not values, so a cold-store URI put in a field with a generic
//! name (`string config`) is review's to refuse. A `google.protobuf.Any` on the walk
//! (`Operation.metadata`, `Status.details`, `ExecutedActionMetadata.auxiliary_metadata`)
//! can carry any message by type URL, and the walk cannot see what it carries. A
//! reviewed name that holds a matched word but names no cold store is listed in
//! `REVIEWED` with its reason; each entry must still be on the walk.
//!
//! Catches:
//! - a field such as `string cold_store = 4;` or `string presigned_url = 4;` added to
//!   `Welcome`, the map, a lease `Start` or any message they hold, however deep;
//! - a message type named for a cold store or its credential reachable from them under
//!   an innocent field name (`ColdStore store = 9;`);
//! - a new service in one of those packages (`service Grants`) whose messages name one;
//! - a walk that sees nothing: a package, service or the map renamed or removed, or a
//!   closure too small to hold `Welcome`, the map's `Member` and the CAS's requests;
//! - a `REVIEWED` entry that no longer matches a name on the walk.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

use prost_types::field_descriptor_proto::Type;
use prost_types::{DescriptorProto, FileDescriptorSet};

/// The packages a daemon has clients for: every service in them is walked. The work
/// stream and segments (`kbf.worker.v1`), storage's action store (`kbf.storage.v1`),
/// and the REAPI ByteStream and CAS it fetches inputs and uploads outputs with.
const DAEMON_PACKAGES: &[&str] = &[
    "kbf.worker.v1",
    "kbf.storage.v1",
    "google.bytestream",
    "build.bazel.remote.execution.v2",
];

/// Services the walk must find among those packages, so a package renamed or emptied
/// does not shrink the walk unseen.
const DAEMON_SERVICES: &[&str] = &[
    ".kbf.worker.v1.Worker",
    ".kbf.worker.v1.Segments",
    ".kbf.storage.v1.ActionStore",
    ".google.bytestream.ByteStream",
    ".build.bazel.remote.execution.v2.ContentAddressableStorage",
];

/// Messages a daemon receives outside those services' method types: the storage map,
/// which `Welcome` and `MapUpdate` are to carry (docs/design/architecture-v2.md, section 6).
const DAEMON_MESSAGES: &[&str] = &[".kbf.map.v1.Map"];

/// Matched anywhere in a name, compared in lower case with the separators removed (so
/// `coldStore`, `cold_store` and `ColdStore` all match).
const COLD_STORE_PARTS: &[&str] = &[
    "cold",
    "bucket",
    "endpoint",
    "objstore",
    "objectstore",
    "credential",
    "secret",
    "accesskey",
    "password",
    "region",
    "presign",
];

/// Matched only as a whole word of a name, or that word with an `s`: each is a part of
/// common words (`digests` holds `sts`, `security` holds `uri`), so a part match would
/// fire on today's wire.
const COLD_STORE_WORDS: &[&str] = &[
    "s3", "aws", "sts", "session", "token", "url", "uri", "key", "object",
];

/// Names on the walk that hold a matched word and were reviewed: (message, field), the
/// field empty for the message's own name.
const REVIEWED: &[(&str, &str)] = &[
    // A node report's capability, `key` = `value` (`arch` = `x86_64`).
    (".kbf.worker.v1.Capability", "key"),
    // A stored object by writer epoch and sequence number, opaque to the daemon: it reads
    // the bytes through storage, never from the cold store.
    (".kbf.worker.v1.ObjectRef", ""),
    (".kbf.worker.v1.BlobLocation", "object"),
    (".kbf.worker.v1.ReadRangeRequest", "object"),
    // REAPI's GetTree paging cursor, issued by the server that answers it.
    (
        ".build.bazel.remote.execution.v2.GetTreeRequest",
        "page_token",
    ),
    (
        ".build.bazel.remote.execution.v2.GetTreeResponse",
        "next_page_token",
    ),
    // The type of the message an Any carries (see the limits above).
    (".google.protobuf.Any", "type_url"),
];

fn descriptors() -> FileDescriptorSet {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let includes = [
        root.join("third_party/remote-apis"),
        root.join("third_party/googleapis"),
        root.join("third_party/grpc-proto"),
        root.clone(),
    ];
    protox::compile(
        [
            "kbf/worker/v1/worker.proto",
            "kbf/worker/v1/segments.proto",
            "kbf/storage/v1/storage.proto",
            "kbf/map/v1/map.proto",
            "google/bytestream/bytestream.proto",
            "build/bazel/remote/execution/v2/remote_execution.proto",
        ],
        includes,
    )
    .expect("the protos compile")
}

/// Every message of `set` by its full name (`.package.Outer.Inner`).
fn messages(set: &FileDescriptorSet) -> Vec<(String, &DescriptorProto)> {
    fn nested<'a>(
        prefix: &str,
        m: &'a DescriptorProto,
        out: &mut Vec<(String, &'a DescriptorProto)>,
    ) {
        let name = format!("{prefix}.{}", m.name());
        for inner in &m.nested_type {
            nested(&name, inner, out);
        }
        out.push((name, m));
    }
    let mut out = Vec::new();
    for file in &set.file {
        let prefix = if file.package().is_empty() {
            String::new()
        } else {
            format!(".{}", file.package())
        };
        for m in &file.message_type {
            nested(&prefix, m, &mut out);
        }
    }
    out
}

fn flat(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The words of a snake_case, camelCase or PascalCase name, in lower case. An acronym
/// ends before the capital that starts the next word (`URLPrefix` is `url`, `prefix`).
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let starts = c.is_ascii_uppercase()
            && prev.is_some_and(|p| {
                p.is_ascii_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
            });
        if starts && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        word.push(c.to_ascii_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn cold_word(name: &str) -> Option<&'static str> {
    let flat = flat(name);
    if let Some(part) = COLD_STORE_PARTS.iter().copied().find(|p| flat.contains(p)) {
        return Some(part);
    }
    let words = words(name);
    COLD_STORE_WORDS.iter().copied().find(|w| {
        words
            .iter()
            .any(|x| x == w || x.strip_suffix('s') == Some(*w))
    })
}

/// The message types the daemon's wire holds: the roots and every message type a field
/// of one names, transitively.
fn daemon_closure(set: &FileDescriptorSet) -> BTreeSet<String> {
    let all = messages(set);
    let by_name = |name: &str| all.iter().find(|(n, _)| n == name).map(|(_, m)| *m);
    let mut roots: Vec<String> = DAEMON_MESSAGES.iter().map(|s| (*s).to_owned()).collect();
    let mut services = BTreeSet::new();
    for package in DAEMON_PACKAGES {
        let in_package: Vec<_> = set
            .file
            .iter()
            .filter(|f| f.package() == *package)
            .flat_map(|f| f.service.iter())
            .collect();
        assert!(!in_package.is_empty(), "no service in {package}");
        for svc in in_package {
            let name = format!(".{package}.{}", svc.name());
            assert!(!svc.method.is_empty(), "{name} has no methods");
            for method in &svc.method {
                roots.push(method.input_type().to_owned());
                roots.push(method.output_type().to_owned());
            }
            services.insert(name);
        }
    }
    for service in DAEMON_SERVICES {
        assert!(
            services.contains(*service),
            "no service {service}: {services:?}"
        );
    }
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<String> = roots.into();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let m = by_name(&name).unwrap_or_else(|| panic!("no message {name}"));
        for field in &m.field {
            if field.r#type() == Type::Message {
                queue.push_back(field.type_name().to_owned());
            }
        }
    }
    seen
}

#[test]
fn the_daemon_wire_has_no_cold_store_field() {
    let set = descriptors();
    let closure = daemon_closure(&set);
    // The walk sees what it must: the work stream's Welcome and lease Start, the map and
    // its members, the segment reads, storage's action store, ByteStream and the CAS.
    for must in [
        ".kbf.worker.v1.Welcome",
        ".kbf.worker.v1.Start",
        ".kbf.map.v1.Map",
        ".kbf.map.v1.Member",
        ".kbf.worker.v1.ReadRangeResponse",
        ".kbf.storage.v1.GetResponse",
        ".google.bytestream.WriteRequest",
        ".build.bazel.remote.execution.v2.BatchUpdateBlobsRequest",
    ] {
        assert!(
            closure.contains(must),
            "{must} is not on the walk: {closure:?}"
        );
    }
    let all = messages(&set);
    let mut found = Vec::new();
    let mut reviewed = BTreeSet::new();
    let mut check = |owner: &str, field: &str, name: &str| {
        let Some(word) = cold_word(name) else { return };
        if REVIEWED.contains(&(owner, field)) {
            reviewed.insert((owner.to_owned(), field.to_owned()));
        } else if field.is_empty() {
            found.push(format!("message {owner} (\"{word}\")"));
        } else {
            found.push(format!("{owner}.{field} (\"{word}\")"));
        }
    };
    for name in &closure {
        let (_, m) = all
            .iter()
            .find(|(n, _)| n == name)
            .expect("the closure holds known messages");
        check(name, "", m.name());
        for field in &m.field {
            check(name, field.name(), field.name());
        }
    }
    assert!(
        found.is_empty(),
        "the daemon's wire names a cold store or its credential (a daemon-only node holds \
         none, docs/design/architecture-v2.md section 9):\n{}",
        found.join("\n")
    );
    let stale: Vec<_> = REVIEWED
        .iter()
        .filter(|(m, f)| !reviewed.contains(&((*m).to_owned(), (*f).to_owned())))
        .collect();
    assert!(
        stale.is_empty(),
        "reviewed names no longer on the walk or no longer matched: {stale:?}"
    );
}

/// Catches a matcher that cannot fire: each spelling of a cold-store name a proto could
/// use is found, and the names on today's wire that hold a short word as a part are not.
#[test]
fn the_cold_store_matcher_fires_on_each_spelling() {
    for name in [
        "cold_store",
        "ColdStore",
        "coldStoreUri",
        "s3_endpoint",
        "S3Location",
        "bucket",
        "object_store",
        "ObjStoreConfig",
        "aws_access_key_id",
        "AwsConfig",
        "secret_access_key",
        "credentials",
        "region",
        "presigned_url",
        "PresignedURL",
        "URLPrefix",
        "upload_uri",
        "session_token",
        "StsSession",
        "signing_key",
        "keys",
        "object",
        "objects_url",
    ] {
        assert!(cold_word(name).is_some(), "{name}");
    }
    for name in [
        "addr",
        "zone",
        "weight",
        "epoch",
        "denied",
        "action_digest",
        "digests",
        "requests",
        "exists",
        "security",
        "during",
        "curl",
        "keystone",
        "objective",
        "laws",
    ] {
        assert_eq!(cold_word(name), None, "{name}");
    }
}

/// Catches a word splitter that runs words together or breaks one up, which would let a
/// whole-word match miss (`presignedURL`) or fire inside a word.
#[test]
fn words_split_each_naming_style() {
    for (name, want) in [
        ("next_page_token", &["next", "page", "token"][..]),
        ("presignedURL", &["presigned", "url"]),
        ("URLPrefix", &["url", "prefix"]),
        ("S3Location", &["s3", "location"]),
        ("ObjectRef", &["object", "ref"]),
        ("a__b", &["a", "b"]),
        ("", &[]),
    ] {
        assert_eq!(words(name), want, "{name}");
    }
}

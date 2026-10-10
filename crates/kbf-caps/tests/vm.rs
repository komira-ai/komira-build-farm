//! The VM keys: `vm.image` matched by membership on its digest, and the report-only
//! `vm.slots`, `vm.max_cpus` and `vm.max_mem_gib`.

use kbf_caps::{
    FromPlatformError, NodeCaps, REPORT_ONLY_KEYS, ReportError, Request, RequestError, Unmet,
};

/// Two recipe digests, as `sha256:<hex>`.
const X: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const Y: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn image(name: &str, digest: &str) -> String {
    format!("{name}@{digest}")
}

fn from(props: &[(&str, &str)]) -> Result<Request, FromPlatformError> {
    Request::from_platform(props.iter().copied())
}

fn report(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    NodeCaps::from_report(entries.iter().copied())
}

/// A Mac reporting the VM images `images`, and two VM slots.
fn mac(images: &[&str]) -> NodeCaps {
    let mut entries = vec![("arch", "arm64"), ("os", "macos"), ("vm.slots", "2")];
    entries.extend(images.iter().map(|i| ("vm.image", *i)));
    report(&entries).expect("a valid report")
}

/// Catches: `vm.image` compared as the whole string. The name is a label for people:
/// a node that built the recipe under another name holds the same image and must serve
/// the request, and one name over another recipe's digest is another image and must
/// not. Also catches `vm.image` left out of the membership keys: the request is then an
/// unknown key, and the report entry is skipped.
#[test]
fn vm_image_is_matched_on_the_digest() {
    let wants_x = from(&[("OSFamily", "darwin"), ("vm.image", &image("a", X))]).unwrap();
    assert!(
        wants_x.matches(&mac(&[&image("b", X)])),
        "the same digest under another name"
    );
    assert!(wants_x.matches(&mac(&[&image("a", X)])));
    let wants_y = from(&[("vm.image", &image("a", Y))]).unwrap();
    let a_y = image("a", Y);
    assert_eq!(
        wants_y.unmet(&mac(&[&image("a", X)])),
        [Unmet::Member {
            key: "vm.image",
            want: &a_y
        }],
        "the same name over another digest"
    );
    assert_eq!(
        wants_y
            .unmet(&mac(&[]))
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [format!("vm.image=a@{Y}")],
        "the why-not text names the image as requested"
    );
}

/// Catches: a report's second `vm.image` refused as a repeated entry (a node holding
/// two images could not register) or kept in place of the first, so a node serves only
/// one of its images.
#[test]
fn a_report_lists_every_vm_image() {
    let node = mac(&[
        &image("xcode-26", Y),
        &image("xcode-16", X),
        &image("again", X),
    ]);
    let digests: Vec<&str> = node.members["vm.image"]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(digests, [X, Y], "the digests, each once");
    assert!(!node.exact.contains_key("vm.image"));
    for digest in [X, Y] {
        let wants = from(&[("vm.image", &image("any", digest))]).unwrap();
        assert!(wants.matches(&node), "{digest}");
    }
}

/// Catches: a `vm.image` value accepted without a well-formed digest. A tag can move,
/// so an action naming `macos-26:latest` would be cached against whatever image a node
/// holds under that tag; and a value that is not compared on a digest can never match a
/// report, so the action would wait on no node instead of being refused at `Execute`.
#[test]
fn a_vm_image_without_a_digest_is_refused() {
    let short = "sha256:".to_owned() + &"1".repeat(63);
    let long = "sha256:".to_owned() + &"1".repeat(65);
    let upper = "sha256:".to_owned() + &"A".repeat(64);
    let not_hex = "sha256:".to_owned() + &"g".repeat(64);
    let sha512 = "sha512:".to_owned() + &"1".repeat(64);
    let bad = [
        "macos-26".to_owned(),
        "macos-26:latest".to_owned(),
        "macos-26@".to_owned(),
        image("macos-26", &short),
        image("macos-26", &long),
        image("macos-26", &upper),
        image("macos-26", &not_hex),
        image("macos-26", &sha512),
        image("", X),
        image("macos 26", X),
        image("a@b", X),
        X.to_owned(),
    ];
    for value in &bad {
        assert_eq!(
            from(&[("vm.image", value)]),
            Err(FromPlatformError::Invalid(RequestError::NoImageDigest(
                value.clone()
            ))),
            "request {value:?}"
        );
        assert_eq!(
            report(&[("arch", "arm64"), ("vm.image", value)]),
            Err(ReportError::NoImageDigest(value.clone())),
            "report {value:?}"
        );
    }
    assert!(matches!(
        from(&[("vm.image", "")]),
        Err(FromPlatformError::Invalid(RequestError::BadValue { .. }))
    ));
    assert_eq!(
        RequestError::NoImageDigest("macos-26:latest".to_owned()).to_string(),
        "vm.image \"macos-26:latest\" names no digest; name an image as \
         <name>@sha256:<64 lowercase hex digits>"
    );
}

/// Catches: a request naming two images (a VM boots one), or one image under two
/// spellings of the key, read as either one.
#[test]
fn a_request_names_one_vm_image() {
    let x = image("a", X);
    let y = image("a", Y);
    for second in ["vm.image", "VM.Image"] {
        assert_eq!(
            from(&[("vm.image", &x), (second, &y)]),
            Err(FromPlatformError::Invalid(RequestError::Repeated(
                "vm.image".to_owned()
            ))),
            "{second}"
        );
    }
}

/// Catches: a report-only key accepted in a request. Read as a capability, `vm.slots=2`
/// would pick nodes by how many VMs they can run, not book one; ignored, as an unknown
/// property is, it would run the action anywhere. Either way the action's digest
/// carries a property that changes nothing. Read in any case, as every kbf key is.
#[test]
fn report_only_keys_are_refused_in_a_request() {
    assert_eq!(
        REPORT_ONLY_KEYS,
        ["vm.slots", "vm.max_cpus", "vm.max_mem_gib"]
    );
    for key in REPORT_ONLY_KEYS {
        let upper = key.to_ascii_uppercase();
        for sent in [key, upper.as_str()] {
            assert_eq!(
                from(&[(sent, "2")]),
                Err(FromPlatformError::Invalid(RequestError::ReportOnly(
                    key.to_owned()
                ))),
                "{sent}"
            );
        }
        assert_eq!(
            Request::parse([(key, "2")]),
            Err(RequestError::ReportOnly(key.to_owned()))
        );
        assert_eq!(
            kbf_caps::property_name(&key.to_ascii_uppercase()).as_deref(),
            Some(key)
        );
    }
    assert_eq!(
        RequestError::ReportOnly("vm.slots".to_owned()).to_string(),
        "\"vm.slots\" is reported by a node and cannot be requested; VM slots are booked \
         through kbf-lease=vm"
    );
}

/// Catches: a report-only entry refused in a report (a VM node could not register), or
/// read without its checks: a value that is not a whole number, or one entry twice, is
/// a daemon bug the server must not register.
#[test]
fn report_only_keys_are_whole_numbers_once_in_a_report() {
    let node = report(&[
        ("arch", "arm64"),
        ("vm.slots", "2"),
        ("vm.max_cpus", "12"),
        ("vm.max_mem_gib", "33"),
    ])
    .unwrap();
    assert!(node.exact.is_empty() && node.members.is_empty() && node.consumables.is_empty());
    for key in REPORT_ONLY_KEYS {
        assert_eq!(
            report(&[("arch", "arm64"), (key, "two")]),
            Err(ReportError::NotANumber {
                key: key.to_owned(),
                value: "two".to_owned()
            }),
            "{key}"
        );
        assert_eq!(
            report(&[("arch", "arm64"), (key, "2"), (key, "2")]),
            Err(ReportError::Repeated(key.to_owned())),
            "{key}"
        );
    }
}

//! The registry client against a registry in this process: what it verifies, what it
//! refuses, and how it behaves on the wire.

mod support;

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use std::time::Duration;

use kbf_image::{Compression, Error, Limits, Ownership, Registry, Timeouts, Unpack};
use kbf_types::ImageRef;
use support::registry::{Blob, FakeRegistry, Image, Item, my_ids, sha256};
use support::scratch;

const REPO: &str = "test/tiny";

/// A named change to the registry's behaviour and the text its failure must hold.
type Case = (
    &'static str,
    Box<dyn Fn(&mut support::registry::Behaviour)>,
    &'static str,
);

fn tiny() -> Image {
    Image::of(vec![
        Blob::gzip_tar(&[
            Item::Dir {
                path: "etc",
                mode: 0o755,
            },
            Item::File {
                path: "etc/hostname",
                data: b"base\n",
                mode: 0o644,
            },
            Item::File {
                path: "etc/old",
                data: b"old\n",
                mode: 0o644,
            },
        ]),
        Blob::plain_tar(&[
            Item::File {
                path: "etc/new",
                data: b"new\n",
                mode: 0o644,
            },
            Item::File {
                path: "etc/.wh.old",
                data: b"",
                mode: 0o644,
            },
        ]),
    ])
}

fn registry_for(fake: &FakeRegistry) -> Registry {
    Registry::new().allow_plain_http(&fake.addr)
}

fn image_ref(fake: &FakeRegistry, image: &Image) -> ImageRef {
    ImageRef::parse(&image.reference(&fake.addr, REPO)).expect("reference")
}

async fn fetch(
    fake: &FakeRegistry,
    image: &Image,
    name: &str,
) -> (std::path::PathBuf, Result<Vec<kbf_image::LayerFile>, Error>) {
    let dir = scratch(name);
    let got = registry_for(fake)
        .fetch(&image_ref(fake, image), &dir)
        .await;
    (dir, got)
}

fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a fetch that does not leave the layers as verified files in order, with
/// their compression: the files are the blobs byte for byte, and unpacking them gives
/// the image's filesystem (the lower layer's file deleted by the upper layer's whiteout).
#[tokio::test]
async fn a_fetch_leaves_the_images_layers_ready_to_unpack() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    let (dir, got) = fetch(&fake, &image, "fetch-ok").await;
    let layers = got.expect("fetched");
    assert_eq!(
        layers
            .iter()
            .map(|l| (l.name.as_str(), l.compression))
            .collect::<Vec<_>>(),
        [
            ("0.tar.gz", Compression::Gzip),
            ("1.tar", Compression::None)
        ]
    );
    assert_eq!(files_in(&dir), ["0.tar.gz", "1.tar"]);
    for (layer, blob) in layers.iter().zip(&image.layers) {
        assert_eq!(
            std::fs::read(dir.join(&layer.name)).expect("read"),
            blob.bytes
        );
    }
    let sources: Vec<_> = layers
        .iter()
        .map(|l| (dir.join(&l.name), l.compression))
        .collect();
    let (uid, gid) = my_ids();
    let root = dir.join("rootfs");
    Unpack::new(Ownership::AllTo { uid, gid })
        .run(&root, &sources)
        .expect("unpack");
    assert_eq!(std::fs::read(root.join("etc/hostname")).unwrap(), b"base\n");
    assert_eq!(std::fs::read(root.join("etc/new")).unwrap(), b"new\n");
    assert!(
        !root.join("etc/old").exists(),
        "the whiteout deleted the lower layer's file"
    );
}

/// Catches the client sending credentials where it should not, and answering a Bearer
/// challenge wrongly: the first request is anonymous, the token request names the
/// repository's pull scope, the retry carries the token, and the blob's redirect target
/// (another path of the same host here, a CDN in life) gets no `Authorization`.
#[tokio::test]
async fn a_token_challenge_is_answered_and_a_redirect_gets_no_credentials() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    fake.behave(|b| {
        b.token = Some("sesame".to_owned());
        b.redirect_blobs = true;
    });
    let (_, got) = fetch(&fake, &image, "fetch-token").await;
    got.expect("fetched");
    let log = fake.requests();
    let manifest = format!("/v2/{REPO}/manifests/{}", image.digest);
    assert_eq!(log[0], format!("GET {manifest} []"), "{log:?}");
    assert!(
        log[1].starts_with("GET /token?service=fake-registry&scope=repository:test/tiny:pull"),
        "{log:?}"
    );
    assert_eq!(log[2], format!("GET {manifest} [Bearer sesame]"), "{log:?}");
    let blobs: Vec<_> = log.iter().filter(|l| l.contains("/blobs/")).collect();
    assert_eq!(blobs.len(), 3, "{log:?}");
    assert!(
        blobs.iter().all(|l| l.ends_with("[Bearer sesame]")),
        "{log:?}"
    );
    let cdn: Vec<_> = log.iter().filter(|l| l.contains("/cdn/")).collect();
    assert_eq!(cdn.len(), 3, "{log:?}");
    assert!(cdn.iter().all(|l| l.ends_with("[]")), "{log:?}");
}

/// Catches a token answer in the other spelling (`access_token`) failing, and the ways
/// an authentication exchange can fail being taken for success.
#[tokio::test]
async fn authentication_failures_say_what_failed() {
    let image = tiny();
    let cases: Vec<Case> = vec![
        (
            "the registry refuses the token",
            Box::new(|b| {
                b.refuse_token = true;
                b.token_answer = Some(r#"{"token":"x"}"#.to_owned())
            }),
            "refused the token",
        ),
        (
            "401 with no challenge",
            Box::new(|b| {
                b.refuse_token = true;
                b.no_challenge = true
            }),
            "no WWW-Authenticate",
        ),
        (
            "Basic",
            Box::new(|b| {
                b.refuse_token = true;
                b.challenge_scheme = Some("Basic".to_owned())
            }),
            "Basic",
        ),
        (
            "token answer not JSON",
            Box::new(|b| {
                b.token = Some("t".to_owned());
                b.token_answer = Some("<html>".to_owned())
            }),
            "not JSON",
        ),
        (
            "token answer with no token",
            Box::new(|b| {
                b.token = Some("t".to_owned());
                b.token_answer = Some("{}".to_owned())
            }),
            "no token",
        ),
    ];
    for (name, setup, want) in cases {
        let fake = FakeRegistry::start().await;
        fake.publish(&image);
        fake.behave(setup);
        let (_, got) = fetch(&fake, &image, "fetch-auth").await;
        let why = got.expect_err(name).to_string();
        assert!(why.contains(want), "{name}: {why}");
    }
    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    fake.behave(|b| {
        b.token = Some("viaaccess".to_owned());
        b.token_answer = Some(r#"{"access_token":"viaaccess"}"#.to_owned());
    });
    fetch(&fake, &image, "fetch-access")
        .await
        .1
        .expect("access_token is a token");
}

/// Catches a layer whose bytes are not the manifest's digest being used (the "wrong blob
/// digest accepted" mutant): same length and different length, in the first and the
/// last layer, each an error naming the blob, with no file left from the fetch.
#[tokio::test]
async fn a_tampered_layer_is_refused_and_nothing_is_left() {
    let image = tiny();
    for which in [0, 1] {
        let digest = sha256(&image.layers[which].bytes);
        let mut same_length = image.layers[which].bytes.clone();
        let last = same_length.len() - 1;
        same_length[last] ^= 0xff;
        let mut longer = image.layers[which].bytes.clone();
        longer.push(0);
        let shorter = image.layers[which].bytes[..image.layers[which].bytes.len() - 1].to_vec();
        for (name, bytes, want) in [
            ("same length", same_length, "hashes to"),
            ("longer", longer, "the manifest says"),
            ("shorter", shorter, "the manifest says"),
        ] {
            let fake = FakeRegistry::start().await;
            fake.publish(&image);
            fake.publish_blob_as(&digest, &bytes);
            let (dir, got) = fetch(&fake, &image, "fetch-tamper").await;
            let why = got.expect_err(name).to_string();
            assert!(
                why.contains(&digest) && why.contains(want),
                "layer {which} {name}: {why}"
            );
            assert_eq!(files_in(&dir), Vec::<String>::new(), "layer {which} {name}");
        }
    }
}

/// Catches a tampered config being believed, and a manifest that is not the pinned one
/// (the registry answers a digest with other bytes), and a manifest naming a config the
/// registry cannot serve.
#[tokio::test]
async fn a_tampered_config_or_manifest_is_refused() {
    let image = tiny();
    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    let config_digest = sha256(&image.config);
    fake.publish_blob_as(
        &config_digest,
        br#"{"architecture":"amd64","os":"linux","x":1}"#,
    );
    let (_, got) = fetch(&fake, &image, "fetch-config").await;
    assert!(
        matches!(got, Err(Error::BlobSize { .. } | Error::BlobDigest { .. })),
        "{got:?}"
    );

    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    let other = Image::of(vec![Blob::plain_tar(&[Item::Dir {
        path: "evil",
        mode: 0o755,
    }])]);
    fake.publish_manifest_as(
        &image.digest,
        "application/vnd.oci.image.manifest.v1+json",
        &other.manifest,
    );
    let (_, got) = fetch(&fake, &image, "fetch-manifest").await;
    assert!(matches!(got, Err(Error::ManifestDigest { .. })), "{got:?}");
}

/// Catches an image index being used (the "index accepted" mutant): the pin names an
/// index, and the registry serves one.
#[tokio::test]
async fn an_image_index_is_refused() {
    let fake = FakeRegistry::start().await;
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":1,"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#,
        sha256(b"x")
    );
    let digest = sha256(index.as_bytes());
    fake.publish_manifest_as(
        &digest,
        "application/vnd.oci.image.index.v1+json",
        index.as_bytes(),
    );
    let image = ImageRef::parse(&format!("docker://{}/{REPO}@{digest}", fake.addr)).unwrap();
    let got = registry_for(&fake)
        .fetch(&image, &scratch("fetch-index"))
        .await;
    assert!(
        matches!(got, Err(Error::Index(ref d)) if *d == digest),
        "{got:?}"
    );
}

/// Catches an image for another architecture or OS running here, and one that states
/// no platform being refused for it.
#[tokio::test]
async fn an_image_for_another_platform_is_refused() {
    let fake = FakeRegistry::start().await;
    let wrong = Image::with_config(
        vec![Blob::plain_tar(&[])],
        br#"{"architecture":"s390x","os":"linux"}"#.to_vec(),
    );
    fake.publish(&wrong);
    let (_, got) = fetch(&fake, &wrong, "fetch-platform").await;
    let why = got.expect_err("refused").to_string();
    assert!(why.contains("linux/s390x"), "{why}");
    let os = Image::with_config(
        vec![],
        format!(
            r#"{{"architecture":"{}","os":"windows"}}"#,
            support::registry::this_arch()
        )
        .into_bytes(),
    );
    fake.publish(&os);
    let (_, got) = fetch(&fake, &os, "fetch-platform-os").await;
    assert!(matches!(got, Err(Error::Platform { .. })), "{got:?}");
    // No stated platform: the digest is the pin.
    let none = Image::with_config(vec![], b"{}".to_vec());
    fake.publish(&none);
    fetch(&fake, &none, "fetch-platform-none")
        .await
        .1
        .expect("accepted");
    // A config that is not JSON.
    let junk = Image::with_config(vec![], b"not json".to_vec());
    fake.publish(&junk);
    let (_, got) = fetch(&fake, &junk, "fetch-platform-junk").await;
    assert!(matches!(got, Err(Error::Malformed(_))), "{got:?}");
    // Another node's architecture is a different pin.
    let image = Image::with_config(vec![], br#"{"architecture":"arm64","os":"linux"}"#.to_vec());
    fake.publish(&image);
    let got = registry_for(&fake)
        .with_arch("aarch64")
        .fetch(&image_ref(&fake, &image), &scratch("fetch-platform-arm"))
        .await;
    got.expect("an arm64 image on an arm64 node");
}

/// Catches the limits not holding: a manifest, a config and a layer count above their
/// limits, and a registry that answers a status other than success.
#[tokio::test]
async fn limits_and_statuses_are_errors() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    let small = |limits: Limits| registry_for(&fake).with_limits(limits);
    let target = image_ref(&fake, &image);
    let limits = Limits::default();
    let got = small(Limits {
        manifest: 10,
        ..limits
    })
    .fetch(&target, &scratch("fetch-lim1"))
    .await;
    assert!(matches!(got, Err(Error::TooLarge { .. })), "{got:?}");
    let got = small(Limits {
        config: 10,
        ..limits
    })
    .fetch(&target, &scratch("fetch-lim2"))
    .await;
    assert!(matches!(got, Err(Error::TooLarge { .. })), "{got:?}");
    let got = small(Limits {
        layers: 1,
        ..limits
    })
    .fetch(&target, &scratch("fetch-lim3"))
    .await;
    assert!(matches!(got, Err(Error::TooLarge { .. })), "{got:?}");

    let missing = Image::of(vec![Blob::plain_tar(&[])]);
    let got = registry_for(&fake)
        .fetch(&image_ref(&fake, &missing), &scratch("fetch-404"))
        .await;
    assert!(
        matches!(got, Err(Error::Status { status: 404, .. })),
        "{got:?}"
    );
    fake.behave(|b| b.status = Some(503));
    let (_, got) = fetch(&fake, &image, "fetch-503").await;
    assert!(
        matches!(got, Err(Error::Status { status: 503, .. })),
        "{got:?}"
    );
}

/// Catches an unwritable directory for the layers being taken for success.
#[tokio::test]
async fn an_unwritable_directory_is_an_error() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    let dir = scratch("fetch-nodir").join("absent");
    let got = registry_for(&fake)
        .fetch(&image_ref(&fake, &image), &dir)
        .await;
    assert!(matches!(got, Err(Error::Io { .. })), "{got:?}");
}

/// Catches the transport's failures being swallowed or hung on: nothing listening, a
/// redirect loop, a redirect without a target, a server that stalls before its headers
/// or in the middle of a body, a handshake that never ends. A short timeout only makes the wait short; each
/// outcome is the error, not a time.
#[tokio::test]
async fn transport_failures_are_errors_not_hangs() {
    let image = tiny();
    let short = Timeouts {
        connect: Duration::from_millis(500),
        head: Duration::from_millis(500),
        idle: Duration::from_millis(500),
    };
    let reference = |addr: &str| ImageRef::parse(&image.reference(addr, REPO)).unwrap();
    let closed = FakeRegistry::closed_address().await;
    let got = Registry::new()
        .allow_plain_http(&closed)
        .fetch(&reference(&closed), &scratch("fetch-closed"))
        .await;
    assert!(got.expect_err("closed").to_string().contains("connect"));

    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    let timed = || registry_for(&fake).with_timeouts(short);
    for (name, setup, want) in [
        (
            "loop",
            Box::new(|b: &mut support::registry::Behaviour| b.redirect_loop = true)
                as Box<dyn Fn(&mut support::registry::Behaviour)>,
            "redirects",
        ),
        (
            "no location",
            Box::new(|b| b.redirect_without_location = true),
            "Location",
        ),
        (
            "stalled head",
            Box::new(|b| b.stall_head = true),
            "no response headers",
        ),
        ("stalled body", Box::new(|b| b.stall_body = true), "stalled"),
    ] {
        fake.behave(|b| *b = Default::default());
        fake.behave(setup);
        let got = timed()
            .fetch(&image_ref(&fake, &image), &scratch("fetch-transport"))
            .await;
        let why = got.expect_err(name).to_string();
        assert!(why.contains(want), "{name}: {why}");
    }

    let (silent, _held) = FakeRegistry::silent().await;
    let got = Registry::with_roots(vec![identity().0])
        .with_timeouts(short)
        .fetch(&reference(&silent), &scratch("fetch-silent"))
        .await;
    assert!(
        got.expect_err("silent")
            .to_string()
            .contains("connecting timed out")
    );
}

/// Catches a redirect to an address that is not allowed (https registry, http target)
/// being followed, and a relative redirect being lost.
#[tokio::test]
async fn redirects_stay_where_they_are_allowed() {
    let image = tiny();
    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    fake.behave(|b| {
        b.redirect_blobs = true;
        b.redirect_to = Some("http://203.0.113.1:9/cdn/x".to_owned());
    });
    let (_, got) = fetch(&fake, &image, "fetch-redirect-away").await;
    assert!(matches!(got, Err(Error::Insecure(_))), "{got:?}");
    fake.behave(|b| b.redirect_to = Some("ftp://203.0.113.1/cdn/x".to_owned()));
    let (_, got) = fetch(&fake, &image, "fetch-redirect-ftp").await;
    assert!(matches!(got, Err(Error::Insecure(_))), "{got:?}");
}

/// Catches an image with no layers failing: a `scratch` image is a valid, empty root.
#[tokio::test]
async fn an_image_with_no_layers_has_nothing_to_fetch() {
    let fake = FakeRegistry::start().await;
    let image = Image::of(vec![]);
    fake.publish(&image);
    let (dir, got) = fetch(&fake, &image, "fetch-scratch").await;
    assert_eq!(got.expect("fetched"), []);
    assert_eq!(files_in(&dir), Vec::<String>::new());
}

/// Catches a config whose bytes are not its digest being believed when the length is
/// right (the size check alone would pass it).
#[tokio::test]
async fn a_config_of_the_right_length_and_the_wrong_bytes_is_refused() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    let mut bytes = image.config.clone();
    let last = bytes.len() - 2;
    bytes[last] = b'x';
    fake.publish_blob_as(&sha256(&image.config), &bytes);
    let (_, got) = fetch(&fake, &image, "fetch-config-bytes").await;
    assert!(matches!(got, Err(Error::BlobDigest { .. })), "{got:?}");
}

/// Catches the token request built wrongly for the challenges registries send: without a
/// scope the client asks for the pull scope of its repository, without a service it asks
/// for none, and a realm that has a query already gets `&`, not a second `?`. Also a
/// token endpoint that answers an error is that error, and a token that cannot be a
/// header value fails the request, not the process.
#[tokio::test]
async fn token_requests_follow_the_challenge() {
    let image = tiny();
    let cases: Vec<Case> = vec![
        (
            "no scope",
            Box::new(|b| b.challenge_no_scope = true),
            "GET /token?service=fake-registry&scope=repository:test/tiny:pull",
        ),
        (
            "no service",
            Box::new(|b| b.challenge_no_service = true),
            "GET /token?scope=repository:test/tiny:pull",
        ),
        (
            "a query in the realm",
            Box::new(|b| b.realm_query = true),
            "GET /token?client=kbf&service=fake-registry&scope=repository:test/tiny:pull",
        ),
    ];
    for (name, setup, want) in cases {
        let fake = FakeRegistry::start().await;
        fake.publish(&image);
        fake.behave(|b| b.token = Some("t".to_owned()));
        fake.behave(setup);
        let (_, got) = fetch(&fake, &image, "fetch-token-request").await;
        got.unwrap_or_else(|e| panic!("{name}: {e}"));
        let log = fake.requests();
        assert!(log[1].starts_with(want), "{name}: {log:?}");
    }

    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    fake.behave(|b| {
        b.token = Some("t".to_owned());
        b.token_status = Some(500);
    });
    let (_, got) = fetch(&fake, &image, "fetch-token-500").await;
    assert!(
        matches!(got, Err(Error::Status { status: 500, .. })),
        "{got:?}"
    );

    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    fake.behave(|b| {
        b.token = Some("a\nb".to_owned());
        b.token_answer = Some("{\"token\":\"a\\nb\"}".to_owned());
    });
    let (_, got) = fetch(&fake, &image, "fetch-token-newline").await;
    assert!(matches!(got, Err(Error::Transport { .. })), "{got:?}");
}

/// Catches a registry that closes without answering, or in the middle of a body, being
/// taken for an empty answer; and chunked answers with a trailer being read wrongly.
#[tokio::test]
async fn a_connection_that_ends_early_is_an_error_and_trailers_end_a_body() {
    let image = tiny();
    for (name, setup) in [
        (
            "hang up",
            Box::new(|b: &mut support::registry::Behaviour| b.hang_up = true)
                as Box<dyn Fn(&mut support::registry::Behaviour)>,
        ),
        ("short body", Box::new(|b| b.short_body = true)),
    ] {
        let fake = FakeRegistry::start().await;
        fake.publish(&image);
        fake.behave(setup);
        let (dir, got) = fetch(&fake, &image, "fetch-early-end").await;
        assert!(
            matches!(got, Err(Error::Transport { .. })),
            "{name}: {got:?}"
        );
        assert_eq!(files_in(&dir), Vec::<String>::new(), "{name}");
    }
    let fake = FakeRegistry::start().await;
    fake.publish(&image);
    fake.behave(|b| b.trailers = true);
    let (dir, got) = fetch(&fake, &image, "fetch-trailers").await;
    assert_eq!(got.expect("fetched").len(), 2);
    assert_eq!(files_in(&dir), ["0.tar.gz", "1.tar"]);
}

/// Catches a write that fails (a full disk: `/dev/full`) being taken for success, with a
/// file left behind.
#[tokio::test]
async fn a_full_disk_is_an_error_and_leaves_no_file() {
    let fake = FakeRegistry::start().await;
    let image = tiny();
    fake.publish(&image);
    let dir = scratch("fetch-full");
    std::os::unix::fs::symlink("/dev/full", dir.join("0.tar.gz")).expect("link");
    let got = registry_for(&fake)
        .fetch(&image_ref(&fake, &image), &dir)
        .await;
    assert!(matches!(got, Err(Error::Io { .. })), "{got:?}");
    assert_eq!(files_in(&dir), Vec::<String>::new());
}

/// Catches a name no URL can hold, or a server name no certificate can be for, being
/// dialled, and the client's debug text and defaults being absent.
#[tokio::test]
async fn names_that_cannot_be_dialled_are_errors_before_any_connection() {
    let image = tiny();
    let target = |repository: &str| {
        ImageRef::parse(&format!("docker://{repository}@{}", image.digest)).expect("reference")
    };
    let registry = Registry::default();
    for (name, repository) in [
        ("a space in the path", "127.0.0.1:1/a b"),
        ("a label starting with a hyphen", "-bad.example/x"),
    ] {
        let got = registry
            .fetch(&target(repository), &scratch("fetch-names"))
            .await;
        assert!(
            matches!(got, Err(Error::Transport { .. })),
            "{name}: {got:?}"
        );
    }
    let got = Registry::new()
        .with_arch("riscv64")
        .fetch(&target("127.0.0.1:1/a"), &scratch("fetch-arch"))
        .await;
    assert!(matches!(got, Err(Error::Arch(_))), "{got:?}");
    assert!(format!("{registry:?}").starts_with("Registry"));
}

/// A self-signed certificate for `127.0.0.1` and an acceptor that serves it.
fn identity() -> (CertificateDer<'static>, TlsAcceptor) {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(
        std::net::Ipv4Addr::LOCALHOST.into(),
    )];
    let cert = params.self_signed(&key).expect("self-signed");
    let der = cert.der().clone();
    let key = PrivateKeyDer::try_from(key.serialize_der()).expect("key der");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .expect("server config");
    (der, TlsAcceptor::from(Arc::new(config)))
}

fn tls_image() -> Image {
    Image::of(vec![Blob::gzip_tar(&[Item::File {
        path: "f",
        data: b"over tls",
        mode: 0o644,
    }])])
}

/// Catches TLS not being used or not being checked: an https registry whose certificate
/// is trusted is fetched from (and no plain-HTTP allowance is needed); the same registry
/// with another trust root, or with the host's roots, which do not hold its certificate,
/// is refused at the handshake, and the empty set of roots is an error of its own.
#[tokio::test]
async fn a_registry_is_reached_over_tls_only_if_its_certificate_is_trusted() {
    let (cert, acceptor) = identity();
    let fake = FakeRegistry::start_with(Some(acceptor)).await;
    let image = tls_image();
    fake.publish(&image);
    let target = ImageRef::parse(&image.reference(&fake.addr, "test/tiny")).unwrap();

    let trusting = Registry::with_roots(vec![cert.clone()]);
    let layers = trusting
        .fetch(&target, &scratch("tls-ok"))
        .await
        .expect("fetched");
    assert_eq!(layers.len(), 1);

    let (other, _) = identity();
    let wrong = Registry::with_roots(vec![other]);
    let why = wrong
        .fetch(&target, &scratch("tls-wrong"))
        .await
        .expect_err("untrusted")
        .to_string();
    assert!(why.contains("TLS"), "{why}");

    let system = Registry::new();
    let why = system
        .fetch(&target, &scratch("tls-system"))
        .await
        .expect_err("untrusted")
        .to_string();
    assert!(why.contains("TLS"), "{why}");

    let none = Registry::with_roots(Vec::new());
    let got = none.fetch(&target, &scratch("tls-none")).await;
    assert!(
        matches!(&got, Err(Error::Transport { why, .. }) if why.contains("no trusted root")),
        "{got:?}"
    );
}

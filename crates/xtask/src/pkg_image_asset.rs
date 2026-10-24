//! `pkg-image-asset --image <oci.tar> [--service frontend|coordinator] --platform <platform>
//! --commit <sha> (--kbf-tarball <file> | --binary <file>) (--out <dir> | --verify-only)`: the
//! frontend or coordinator image as a release asset, `kbf-frontend-image-<commit>-<platform>.oci.tar`
//! or `kbf-coordinator-image-<commit>-<platform>.oci.tar`, and the checks that tie it to the
//! bytes the attestation covers. The image is an OCI image layout in one tar (what `docker buildx build
//! --output type=oci` writes); `docker image load` and `ctr images import` read it.
//!
//! The image is opened (its `index.json`, the manifest, the config and the layers it names, each
//! blob checked against the digest that names it) and refused unless:
//!
//! - its config says `os` linux and the `architecture` of `--platform`;
//! - its entrypoint is exactly `/usr/bin/kbf --mode=frontend` (the configuration is the
//!   mounted `/etc/kbf/kbf.toml`, so no flag carries a secret or an address), or for the
//!   coordinator exactly `/usr/bin/kbf-coordinator`;
//! - it runs as a user that is not root (a numeric `User`, not `0`, not `root`);
//! - `usr/bin/kbf`, as the layers leave it, is byte for byte the `kbf` inside `--kbf-tarball`,
//!   the tarball `build-commit check-asset` has already shown names the commit: the image
//!   carries the bytes the build identified, not a rebuild. It is mode 0755. The coordinator
//!   has no tarball: its `usr/bin/kbf-coordinator` is held to `--binary`, the file the job
//!   built and `build-commit check` has shown names the commit. The verify job has no such
//!   file, so `--verify-only` on a coordinator image makes the other checks alone.
//!
//! Then, without `--verify-only`, it copies the image to
//! `<out>/<service>-image-<commit>-<platform>.oci.tar` and appends its line to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), refusing an image already listed. With
//! `--verify-only` it makes the checks on `--image`, which must already be named as the asset
//! (the file the attest job signed, downloaded again by the verify job) and writes nothing.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, read};
use crate::pkg_asset::{self, Mode};

/// The end of the asset's file name: an OCI image layout in a tar.
pub const SUFFIX: &str = ".oci.tar";

/// A service the release ships as an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Image {
    /// `kbf --mode=frontend`.
    Frontend,
    /// `kbf-coordinator`.
    Coordinator,
}

impl Image {
    /// The name of the asset before its commit.
    pub fn asset_name(self) -> &'static str {
        match self {
            Image::Frontend => "kbf-frontend-image",
            Image::Coordinator => "kbf-coordinator-image",
        }
    }

    /// The image the asset name `binary` (before `-<commit>-<platform>`) belongs to.
    pub fn named(binary: &str) -> Option<Image> {
        [Image::Frontend, Image::Coordinator]
            .into_iter()
            .find(|i| i.asset_name() == binary)
    }

    /// The program the image runs.
    fn program(self) -> &'static str {
        match self {
            Image::Frontend => "kbf",
            Image::Coordinator => "kbf-coordinator",
        }
    }

    /// Where the image holds the program, as a layer tar names it (no leading slash).
    fn path_in_image(self) -> String {
        format!("usr/bin/{}", self.program())
    }

    /// The entrypoint of the image.
    fn entrypoint(self) -> &'static [&'static str] {
        match self {
            Image::Frontend => &["/usr/bin/kbf", "--mode=frontend"],
            Image::Coordinator => &["/usr/bin/kbf-coordinator"],
        }
    }

    /// How the checks name the image.
    fn label(self) -> &'static str {
        match self {
            Image::Frontend => "frontend",
            Image::Coordinator => "coordinator",
        }
    }

    /// The file name of the asset.
    pub fn file_name(self, commit: &str, platform: Platform) -> String {
        format!("{}-{commit}-{}{SUFFIX}", self.asset_name(), platform.name())
    }
}

/// The bytes the image's program must equal.
#[derive(Clone, Copy)]
pub enum Reference<'a> {
    /// The `kbf` inside a `kbf-<commit>-<platform>.tar.gz`.
    KbfTarball(&'a Path),
    /// A built program (the coordinator has no tarball).
    Binary(&'a Path),
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub image: Image,
    pub platform: Platform,
    /// What the program of the image must equal; `None` only to check an asset where the
    /// build's own file is not at hand (`--verify-only` of a coordinator image).
    pub reference: Option<Reference<'a>>,
}

/// The `architecture` an image for `platform` carries.
fn architecture(platform: Platform) -> &'static str {
    match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
    }
}

/// The regular files of the tar `bytes`, plain or gzip-compressed (a layer is either): path
/// without a leading `./` or `/`, mode, bytes.
fn files(bytes: &[u8]) -> Result<Vec<(String, u32, Vec<u8>)>, String> {
    let plain;
    let tar: &[u8] = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| format!("not a gzip stream: {e}"))?;
        plain = out;
        &plain
    } else {
        bytes
    };
    let not_a_tar = |e: std::io::Error| format!("not a tar: {e}");
    let mut archive = tar::Archive::new(tar);
    let mut found = Vec::new();
    for entry in archive.entries().map_err(not_a_tar)? {
        let mut entry = entry.map_err(not_a_tar)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let mut body = Vec::new();
        let mode = entry.header().mode().map_err(not_a_tar)?;
        entry.read_to_end(&mut body).map_err(not_a_tar)?;
        let path = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_owned();
        found.push((path, mode & 0o777, body));
    }
    Ok(found)
}

/// The blob `digest` names (`sha256:<hex>`) in the layout `blobs`, or what is wrong with it:
/// absent, or bytes that do not hash to the digest.
fn blob<'a>(
    blobs: &'a BTreeMap<String, Vec<u8>>,
    digest: &str,
    what: &str,
) -> Result<&'a [u8], String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(format!("{what} {digest:?} is not a sha256 digest"));
    };
    let bytes = blobs
        .get(&format!("blobs/sha256/{hex}"))
        .ok_or_else(|| format!("holds no blob for the {what} {digest}"))?;
    if Sha256::of(bytes).to_string() != hex {
        return Err(format!(
            "the blob of the {what} {digest} has another digest"
        ));
    }
    Ok(bytes)
}

/// The JSON of the blob `digest` names.
fn json(blobs: &BTreeMap<String, Vec<u8>>, digest: &str, what: &str) -> Result<Value, String> {
    serde_json::from_slice(blob(blobs, digest, what)?)
        .map_err(|e| format!("the {what} {digest} is not JSON: {e}"))
}

/// The string at `value[key]`.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// What is wrong with `image` (the bytes of the tar) as the image of `c`: one line per
/// problem, none when it meets every check above.
///
/// # Errors
/// The file `c` names as the reference cannot be read.
pub fn problems(image: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let expected = match c.reference {
        None => None,
        Some(Reference::Binary(path)) => {
            Some((read(path)?, format!("the built {}", c.image.program())))
        }
        Some(Reference::KbfTarball(path)) => {
            let shipped = pkg_asset::members(&read(path)?)
                .ok()
                .and_then(|m| m.into_iter().find(|m| m.path == "kbf"));
            let Some(shipped) = shipped else {
                return Ok(vec![format!("{} holds no kbf", path.display())]);
            };
            Some((shipped.bytes, "the kbf of the attested tarball".to_owned()))
        }
    };
    let expected = expected.as_ref().map(|(b, w)| (b.as_slice(), w.as_str()));
    Ok(match opened(image, c.image, c.platform, expected) {
        Ok(found) | Err(found) => found,
    })
}

/// The problems of `image`; the `Err` is the same list, for the early exits of the walk.
fn opened(
    image: &[u8],
    kind: Image,
    platform: Platform,
    expected: Option<(&[u8], &str)>,
) -> Result<Vec<String>, Vec<String>> {
    let one = |p: String| vec![p];
    let outer = files(image).map_err(|e| one(format!("is not an OCI image tar: {e}")))?;
    let blobs: BTreeMap<String, Vec<u8>> = outer.into_iter().map(|(p, _, b)| (p, b)).collect();
    let index: Value = blobs
        .get("index.json")
        .ok_or_else(|| one("holds no index.json: not an OCI image layout".to_owned()))
        .and_then(|b| serde_json::from_slice(b).map_err(|e| one(format!("index.json: {e}"))))?;
    let manifests = index["manifests"].as_array().map_or(&[][..], Vec::as_slice);
    let [entry] = manifests else {
        return Err(one(format!(
            "index.json lists {} manifests, not exactly one",
            manifests.len()
        )));
    };
    let digest = text(entry, "digest").unwrap_or_default();
    let manifest = json(&blobs, digest, "manifest").map_err(one)?;
    let config_digest = text(&manifest["config"], "digest").unwrap_or_default();
    let config = json(&blobs, config_digest, "config").map_err(one)?;
    let mut problems = Vec::new();
    let (os, arch) = (text(&config, "os"), text(&config, "architecture"));
    if os != Some("linux") || arch != Some(architecture(platform)) {
        problems.push(format!(
            "config says {}/{}, not linux/{}",
            os.unwrap_or("(none)"),
            arch.unwrap_or("(none)"),
            architecture(platform)
        ));
    }
    let entrypoint: Vec<&str> = config["config"]["Entrypoint"]
        .as_array()
        .map_or(Vec::new(), |a| a.iter().filter_map(Value::as_str).collect());
    if entrypoint != kind.entrypoint() {
        problems.push(format!(
            "entrypoint is {entrypoint:?}, not {:?}",
            kind.entrypoint()
        ));
    }
    let user = text(&config["config"], "User").unwrap_or_default();
    let uid = user.split(':').next().unwrap_or_default();
    if uid.is_empty() || uid == "root" || uid.parse::<u32>().is_ok_and(|n| n == 0) {
        problems.push(format!(
            "config runs as user {:?}, which is root or unset",
            user
        ));
    }
    // The layers in order: a later one replaces an earlier one's file.
    let in_image_path = kind.path_in_image();
    let mut in_image = None;
    let layers = manifest["layers"].as_array().map_or(&[][..], Vec::as_slice);
    for layer in layers {
        let digest = text(layer, "digest").unwrap_or_default();
        let bytes = blob(&blobs, digest, "layer").map_err(one)?;
        let held = files(bytes).map_err(|e| one(format!("layer {digest}: {e}")))?;
        if let Some(found) = held.into_iter().find(|(p, _, _)| *p == in_image_path) {
            in_image = Some(found);
        }
    }
    match in_image {
        None => problems.push(format!("holds no {in_image_path}")),
        Some((_, mode, bytes)) => {
            if let Some((reference, what)) = expected
                && bytes != reference
            {
                problems.push(format!(
                    "{in_image_path} (sha256:{}) is not {what} (sha256:{})",
                    Sha256::of(&bytes),
                    Sha256::of(reference)
                ));
            }
            if mode != 0o755 {
                problems.push(format!("{in_image_path} has mode {mode:04o}, not 0755"));
            }
        }
    }
    Ok(problems)
}

/// The `pkg-image-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the image is refused, or a file cannot be read or written.
pub fn asset(image: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    let said = match c.reference {
        Some(Reference::KbfTarball(p) | Reference::Binary(p)) => format!(
            "a {} {} image of the {} in {}",
            c.platform.name(),
            c.image.label(),
            c.image.program(),
            p.display()
        ),
        None => format!("a {} {} image", c.platform.name(), c.image.label()),
    };
    let name = c.image.file_name(commit, c.platform);
    let found = |bytes: &[u8]| problems(bytes, c);
    pkg_asset::publish(image, (commit, c.platform), &name, mode, found, &said)
}

#[cfg(test)]
mod tests;

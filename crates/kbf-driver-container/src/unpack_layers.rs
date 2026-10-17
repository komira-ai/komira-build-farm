//! The `__unpack-layers` mode of the `kbf-daemon` binary: applies an image's layer
//! tars to a root filesystem, run inside a helper container of the OCI backend.
//!
//! `__unpack-layers <stage> <blobs> <layer>...` makes `<stage>/rootfs` from the layer
//! files in `<blobs>`, lowest first (`<n>.tar` or `<n>.tar.gz`: the names `kbf-image`
//! fetches them under), then the mount points the runtime binds over. The helper has an
//! action's user namespace, so id 0 of the tars is the first subordinate id and every
//! file ends up owned by the host id the action sees as the image's own, with no shifting
//! pass. Its user namespace maps neither the daemon user nor anything outside the stage
//! and blob directories it is given, so a hostile tar can write nothing the daemon user
//! owns.
//!
//! The binary's `main` reaches it through [`crate::chown_dirs::dispatch`], before it
//! parses its flags. The argument list is an interface between daemon versions: a driver
//! may run a newer binary after an update renamed it over the old one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kbf_image::{Compression, Ownership, Unpack, ensure_paths};

/// The argument after argv0 that selects this mode.
pub const ARG: &str = "__unpack-layers";

/// The directory of the root filesystem under the stage directory.
pub const ROOTFS: &str = "rootfs";

/// The mount points the driver binds over in an unpacked image, as directories, and the
/// files (`/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`) it binds over.
const MOUNT_DIRS: [&str; 13] = [
    "dev",
    "proc",
    "sys",
    "etc",
    "tmp",
    "var/tmp",
    "run",
    "root",
    "kbf/root",
    "dev/pts",
    "dev/shm",
    "dev/mqueue",
    "sys/fs/cgroup",
];
const MOUNT_FILES: [&str; 3] = ["etc/hosts", "etc/hostname", "etc/resolv.conf"];

/// What the mode was asked.
struct Request {
    stage: PathBuf,
    layers: Vec<(PathBuf, Compression)>,
}

/// Reads `<stage> <blobs> <layer>...`.
fn parse(args: &[OsString]) -> Option<Request> {
    let [stage, blobs, layers @ ..] = args else {
        return None;
    };
    let blobs = Path::new(blobs);
    let mut out = Vec::new();
    for layer in layers {
        let name = layer.to_str()?;
        // A bare file name of the blob directory: nothing to walk out of it with.
        if Path::new(name).file_name() != Some(layer.as_os_str()) {
            return None;
        }
        let compression = if name.ends_with(".tar.gz") {
            Compression::Gzip
        } else if name.ends_with(".tar") {
            Compression::None
        } else {
            return None;
        };
        out.push((blobs.join(name), compression));
    }
    Some(Request {
        stage: PathBuf::from(stage),
        layers: out,
    })
}

/// The mode, its arguments after [`ARG`] being `<stage> <blobs> <layer>...`: 0 once the
/// root filesystem is made, 1 when a layer or a mount point could not be applied (the
/// reason on stderr), 2 for arguments it cannot read.
#[must_use]
pub fn run(args: &[OsString]) -> u8 {
    let Some(request) = parse(args) else {
        eprintln!(
            "kbf-daemon {ARG}: usage: {ARG} <stage> <blobs> <NAME.tar|NAME.tar.gz>..., not {args:?}"
        );
        return 2;
    };
    let rootfs = request.stage.join(ROOTFS);
    // Each owner is the tar header's; a header outside the container's ids is an error.
    if let Err(e) = Unpack::new(Ownership::Header).run(&rootfs, &request.layers) {
        eprintln!("kbf-daemon {ARG}: {e}");
        return 1;
    }
    if let Err(e) = ensure_paths(&rootfs, &MOUNT_DIRS, &MOUNT_FILES) {
        eprintln!("kbf-daemon {ARG}: mount points: {e}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(format!("unpack-layers-{name}-{}", std::process::id()));
        let _ = crate::remove::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// A tar of one file `path`, owned by the running user (which an unprivileged
    /// process may give files to).
    fn tar_of(path: &str, data: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_uid(u64::from(rustix::process::geteuid().as_raw()));
        header.set_gid(u64::from(rustix::process::getegid().as_raw()));
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut header, path, data)
            .expect("append");
        builder.into_inner().expect("finish")
    }

    fn args(stage: &Path, blobs: &Path, layers: &[&str]) -> Vec<OsString> {
        let mut args = vec![stage.into(), blobs.into()];
        args.extend(layers.iter().map(OsString::from));
        args
    }

    /// Catches a mode that does not make the root filesystem the runtime needs: the
    /// layers' files (a gzip and a plain layer, in order), and every mount point, as
    /// directories and files, with an image's own `/etc` kept.
    #[test]
    fn the_mode_makes_the_root_and_its_mount_points() {
        use std::io::Write as _;
        let dir = scratch("ok");
        let (stage, blobs) = (dir.join("stage"), dir.join("blobs"));
        std::fs::create_dir_all(&stage).expect("stage");
        std::fs::create_dir_all(&blobs).expect("blobs");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_of("etc/os-release", b"image"))
            .expect("gzip");
        std::fs::write(blobs.join("0.tar.gz"), gz.finish().expect("gzip end")).expect("layer");
        std::fs::write(blobs.join("1.tar"), tar_of("usr/bin/tool", b"tool")).expect("layer");
        assert_eq!(run(&args(&stage, &blobs, &["0.tar.gz", "1.tar"])), 0);
        let root = stage.join(ROOTFS);
        assert_eq!(
            std::fs::read(root.join("etc/os-release")).expect("file"),
            b"image"
        );
        assert_eq!(
            std::fs::read(root.join("usr/bin/tool")).expect("file"),
            b"tool"
        );
        for dir in MOUNT_DIRS {
            assert!(root.join(dir).is_dir(), "{dir}");
        }
        for file in MOUNT_FILES {
            assert!(root.join(file).is_file(), "{file}");
        }
        assert_eq!(dispatch_is_not_this_mode(), None);
    }

    /// The test harness itself is not in the mode.
    fn dispatch_is_not_this_mode() -> Option<u8> {
        crate::chown_dirs::dispatch()
    }

    /// Catches arguments the mode cannot read being run (a missing blob directory, a
    /// layer name that is a path or has no tar suffix, or is not text) and the two ways
    /// the work can fail, a layer that is not there and an image whose `etc` is a file so
    /// a mount point cannot be made, reported as success.
    #[test]
    fn the_mode_refuses_bad_arguments_and_says_why_work_failed() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = scratch("bad");
        let (stage, blobs) = (dir.join("stage"), dir.join("blobs"));
        std::fs::create_dir_all(&stage).expect("stage");
        std::fs::create_dir_all(&blobs).expect("blobs");
        std::fs::write(
            blobs.join("0.tar"),
            tar_of("etc", b"a file, not a directory"),
        )
        .expect("layer");
        for bad in [
            vec![stage.clone().into_os_string()],
            args(&stage, &blobs, &["../escape.tar"]),
            args(&stage, &blobs, &["sub/0.tar"]),
            args(&stage, &blobs, &["0.zip"]),
            args(&stage, &blobs, &["0"]),
            vec![
                stage.clone().into(),
                blobs.clone().into(),
                OsString::from_vec(vec![0xff]),
            ],
        ] {
            assert_eq!(run(&bad), 2, "{bad:?}");
        }
        assert!(
            !stage.join(ROOTFS).exists(),
            "a refused request made a root"
        );
        assert_eq!(run(&args(&stage, &blobs, &["absent.tar"])), 1);
        std::fs::remove_dir_all(stage.join(ROOTFS)).expect("remove the failed root");
        assert_eq!(run(&args(&stage, &blobs, &["0.tar"])), 1);
    }
}

//! The `kbf-deploy` command line.
//!
//! ```text
//! kbf-deploy minio buckets --endpoint URL --bucket NAME... --policy NAME --account NAME
//!                          --mc PATH [--key-file PATH [--key-owner USER]] [--remove] [--apply]
//! kbf-deploy minio nodeport --kubectl PATH --kubeconfig PATH [--context NAME]
//!                           --namespace NS --service NAME --node-port N [--remove] [--apply]
//! kbf-deploy linux render-units --config FILE --out DIR [--kbf-uid UID] [--mode MODE]
//! kbf-deploy linux check-units --config FILE --dir DIR [--installable] [--mode MODE [--kbf-bin PATH]]
//! kbf-deploy linux install-units --config FILE [--bin-dir DIR] [--restart-minio] [--apply]
//! kbf-deploy linux install-units --config FILE --uninstall [--apply]
//! kbf-deploy linux image-pull --config FILE [--image REF]... [--platform OS/ARCH] [--apply]
//! kbf-deploy linux swap --config FILE [--kubelet-allows-swap] [--remove] [--apply]
//! kbf-deploy linux meta-backup --meta-dir DIR [--hot-dir DIR] --out FILE [--metrics URL]
//!                              [--unit NAME] [--ready-secs N] [--apply]
//! kbf-deploy linux meta-verify --archive FILE --into DIR
//! kbf-deploy mdm fetch --dir DIR [--only TOOL]... [--apply]
//! kbf-deploy mdm install --config FILE --bin-dir DIR [--apply]
//! kbf-deploy mdm install --config FILE --uninstall [--apply]
//! kbf-deploy mdm tls --config FILE [--apply]
//! kbf-deploy mdm tls --config FILE --check-godaddy
//! kbf-deploy mdm push-cert --config FILE --cert PEM --key PEM [--apply]
//! kbf-deploy mdm push-cert --config FILE --profile-only [--topic TOPIC] [--apply]
//! kbf-deploy mdm dep get-cert --config FILE --out FILE [--apply]
//! kbf-deploy mdm dep upload-token --config FILE --token FILE [--apply]
//! kbf-deploy mdm dep define-profile --config FILE [--apply]
//! kbf-deploy mdm dep assign --config FILE --serial SERIAL... [--apply]
//! kbf-deploy mdm verify --config FILE [--via ADDR:PORT]
//! kbf-deploy mdm erase-request --serial SERIAL --reason TEXT [--lease ID] --key KEY
//!                           --out FILE [--valid-for MINUTES] [--ssh-keygen PATH] [--apply]
//! kbf-deploy envoy render --config FILE --out DIR
//! kbf-deploy envoy check --config FILE [--dir DIR]
//! kbf-deploy envoy apply --config FILE --kubectl PATH --kubeconfig PATH
//!                        [--refuse-example FILE] [--apply]
//! kbf-deploy pilot server-install --config FILE --bin FILE [--bind ADDR] [--pki DIR]
//!                                 [--s3-env-file FILE] [--apply]
//! kbf-deploy pilot server-install --config FILE --uninstall [--apply]
//! kbf-deploy pilot buckconfig --config FILE --worktree DIR [--remove] [--apply]
//! kbf-deploy log (--log-dir DIR | --hot-store DIR) [--lines N]
//! kbf-deploy rollout --inventory FILE --commit SHA --run-id ID
//!                    [--idle-timeout SECONDS] [--pause-file FILE] [--apply]
//! ```
//!
//! Bad flags exit 2 for every subcommand. The `minio` subcommands change a host: without
//! `--apply` they print their plan as `DRY-RUN:` lines and change nothing, and they exit
//! 0 done (or nothing to do) or 1 refused or failed. `linux render-units` and
//! `check-units` only write into or read the directory they are given, and exit 0 done,
//! 1 a check found a broken rule, or 2 refused (a bad setting or file, printed as
//! `REFUSED: ...` on stderr). `linux install-units`, `image-pull` and `swap` change the
//! host as root, plan by default as the `minio` subcommands do, and exit 0 done (or
//! nothing to do), 1 a step or the check after an apply failed (`FAIL ...`), or 2
//! refused (`REFUSED: ...` on stderr). `linux meta-backup` and `meta-verify` exit 0
//! done, 1 a failure after the stop (the unit restarted) or an archive that does not
//! verify (`FAIL ...` on stderr), or 2 refused ([`crate::linux::meta`]). `envoy render` only writes into the directory
//! it is given, and exits 0 done or 2 refused (`REFUSED: ...` on stderr); `envoy check`
//! only reads, and exits 0 every check passed, 1 a check failed (`FAIL <check>: ...` on
//! stdout) or 2 refused. `envoy apply` reads the cluster and has it validate the render in
//! a dry run as in an apply, changes it only with `--apply`, and exits 0 done (or
//! planned), 1 a step or the check after an apply failed (`FAIL ...` on stderr), or 2
//! refused before any step (`REFUSED: ...` on stderr; a render check's `FAIL` lines on
//! stdout).
//! The `mdm` verbs exit as the `linux` host verbs do; `mdm verify` exits 1 when a check
//! fails, `mdm dep assign` when Apple did not assign every device, and `mdm tls
//! --check-godaddy` when the account may not use the API ([`crate::mdm`]).
//! The `pilot` verbs run as the operator's own user, plan by default as the `minio`
//! subcommands do, and exit 0 done (or nothing to do), 1 a step failed or kbf-server
//! never reported ready after its start, or 2 refused (`REFUSED: ...` on stderr)
//! ([`crate::pilot`]).
//! `log` only reads: it prints the last `--lines` (default 100) lines of a kbf-server's
//! durable log, across `kbf-server.log` and its rotated files, oldest first, and exits 0,
//! or 2 refused (`REFUSED: ...` on stderr: no such directory, or no log in it)
//! ([`crate::log`]).
//! `rollout` plans by default as the `minio` subcommands do, and exits 0 done (or planned),
//! 1 a host failed, 2 refused, or 3 stopped with nothing wrong (paused, or a host still
//! busy) ([`crate::rollout`]).
//! `minio buckets` reads the MinIO admin key pair from `MINIO_ROOT_USER` and
//! `MINIO_ROOT_PASSWORD` in the environment (secrets are never flags: argv is
//! world-readable).

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use kbf_types::ImageRef;

use crate::envoy;
use crate::https::Client;
use crate::linux::{self, HostError, LinuxConfig, images, install, install_mode, meta, swap};
use crate::log;
use crate::mdm::dep::{Dep, Serial};
use crate::mdm::erase::{self, EraseArgs, Made};
use crate::mdm::fetch::Fetch;
use crate::mdm::install::Install;
use crate::mdm::profile::Topic;
use crate::mdm::push_cert::{self, PushCert, With};
use crate::mdm::tls::Acme;
use crate::mdm::verify::Verify;
use crate::mdm::{self, MdmConfig, MdmError, Pins, Tool};
use crate::minio::{self, Admin, K8sName, KbfName, MinioError, buckets, nodeport};
use crate::pilot;
use crate::rollout;
use kbf_host::exec::{Exec, HostExec};
use kbf_host::path::{HostPath, Root};
use kbf_host::plan::{Mode, RunAs, execute};
use kbf_host::step::HttpUrl;

#[derive(Parser)]
#[command(
    name = "kbf-deploy",
    version,
    about = "Deploy kits for a kbf farm; a minio plan is printed by default and carried out with --apply"
)]
struct Cli {
    #[command(subcommand)]
    kit: Kit,
}

#[derive(Subcommand)]
enum Kit {
    /// The host side of a commit-confirm deploy: the releases under /usr/local/kbf and
    /// the deploy state.
    #[command(subcommand)]
    Host(crate::host::Host),
    /// A MinIO for kbf: its buckets and key, and a NodePort for its S3 port.
    #[command(subcommand)]
    Minio(Minio),
    /// A converged Linux storage node: MinIO, kbf-server and kbf-daemon.
    #[command(subcommand)]
    Linux(Linux),
    /// An Apple MDM on one Linux host: NanoHUB, the SCEP server, NanoDEP and an HTTPS
    /// proxy.
    #[command(subcommand)]
    Mdm(Mdm),
    /// A client front: a Tailscale operator Ingress in front of Envoy, which routes
    /// REAPI calls to the kbf-servers.
    #[command(subcommand)]
    Envoy(Envoy),
    /// A pilot kbf-server as a systemd user service, and the buck2 overlay that points
    /// one worktree at it.
    #[command(subcommand)]
    Pilot(pilot::cli::Pilot),
    /// A new commit put on the whole fleet, one host at a time: planned by default,
    /// carried out with --apply.
    Rollout(rollout::cli::RolloutArgs),
    /// The last lines of a kbf-server's durable log, across its rotated files, oldest
    /// first. Only reads.
    Log(log::LogArgs),
}

#[derive(Subcommand)]
enum Envoy {
    /// Write manifests.json (the Kubernetes objects) and envoy.json (Envoy's config)
    /// into a directory.
    Render {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the two files.
        #[arg(long)]
        out: PathBuf,
    },
    /// Check a render for what the REAPI path through the front depends on: every
    /// client call routed with no bound on a stream's life, TLS to the CA naming
    /// each server, health checks kbf-server answers, the Ingress and NetworkPolicy
    /// reaching Envoy. One `FAIL <check>: <why>` line per failure.
    Check {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// The directory render wrote; without it, the render of --config is checked.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Check the render and what the cluster must already hold, have the API server
    /// validate the render, and with --apply apply it (server-side, field manager
    /// kbf-front) and wait for Envoy and the Ingress's address.
    Apply {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// kubectl, by absolute path.
        #[arg(long, value_parser = HostPath::new)]
        kubectl: HostPath,
        /// A kubeconfig with the config's KBF_FRONT_KUBE_CONTEXT.
        #[arg(long, value_parser = HostPath::new)]
        kubeconfig: HostPath,
        /// Settings whose context, tailnet and servers --config must not share (the
        /// kit's own config.example); kbf's example when not given.
        #[arg(long)]
        refuse_example: Option<PathBuf>,
        /// Apply for real.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum Linux {
    /// Render the node's four systemd files into a directory.
    RenderUnits {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the files, laid out as under /etc/systemd/system.
        #[arg(long)]
        out: PathBuf,
        /// The kbf user's uid (the daemon's XDG_RUNTIME_DIR); without it the daemon's
        /// unit keeps `<kbf uid>`, which `check-units --installable` refuses.
        #[arg(long)]
        kbf_uid: Option<u32>,
        /// Render kbf.slice and the unit of this mode of the one kbf binary instead.
        #[arg(long, value_enum)]
        mode: Option<linux::Mode>,
    },
    /// Check rendered units against the host's priorities.
    CheckUnits {
        /// The settings the units were rendered with.
        #[arg(long)]
        config: PathBuf,
        /// The directory render-units wrote.
        #[arg(long)]
        dir: PathBuf,
        /// Also refuse a `<...>` placeholder.
        #[arg(long)]
        installable: bool,
        /// Check the unit of this mode of the one kbf binary instead.
        #[arg(long, value_enum)]
        mode: Option<linux::Mode>,
        /// The kbf that parses a built mode's flags (`kbf check-args`); without it the
        /// unit's own ExecStart= binary, and the parse is skipped when that is not there.
        #[arg(long, requires = "mode")]
        kbf_bin: Option<PathBuf>,
    },
    /// Install the binaries and units, and start kbf-server and kbf-daemon (as root).
    InstallUnits {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where new kbf-server and kbf-daemon binaries are; without it the installed
        /// ones stay.
        #[arg(long, value_parser = HostPath::new)]
        bin_dir: Option<HostPath>,
        /// Also restart MinIO, so its new priorities take effect now.
        #[arg(long)]
        restart_minio: bool,
        /// Roll back: stop the services and move the four files aside.
        #[arg(long, conflicts_with_all = ["bin_dir", "restart_minio"])]
        uninstall: bool,
        /// Install kbf.slice and the unit of this mode of the one kbf binary instead
        /// (with --bin-dir, the new kbf too), and start it.
        #[arg(long, value_enum, conflicts_with_all = ["restart_minio", "uninstall"])]
        mode: Option<linux::Mode>,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Pull the pinned images into the kbf user's Podman store, as that user (as root).
    ImagePull {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// An image instead of KBF_IMAGES (repeat for more): <repo>@sha256:<digest>.
        #[arg(long, value_parser = linux::config::image)]
        image: Vec<ImageRef>,
        /// The platform each image must be built for.
        #[arg(long, default_value = linux::images::PLATFORM)]
        platform: String,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Back up kbf-server's metadata log in a brief stop (as root): one tar file with
    /// a sha256 manifest; prints the downtime.
    MetaBackup(meta::cli::BackupArgs),
    /// Extract a meta-backup archive into an empty directory and check every file.
    MetaVerify(meta::cli::VerifyArgs),
    /// Check, read only, that a certificate is its row of the farm's certificate table:
    /// its one role, its extended key usage and its chain to the CA. The frontend's
    /// REAPI certificate is not the farm CA's and is no row.
    VerifyCert(linux::verify_cert::VerifyArgs),
    /// Make, activate and list the build swapfile (as root).
    Swap {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// kubelet runs here and its config sets failSwapOn: false.
        #[arg(long)]
        kubelet_allows_swap: bool,
        /// Roll back: swapoff, drop the fstab line, remove the file.
        #[arg(long, conflicts_with = "kubelet_allows_swap")]
        remove: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum Mdm {
    /// Download the pinned upstream archives into a directory, as yourself.
    Fetch {
        /// Where the archives go: absolute, outside every git work tree.
        #[arg(long)]
        dir: PathBuf,
        /// Only this tool (repeat for more); every pinned tool when not given.
        #[arg(long, value_parser = Tool::named)]
        only: Vec<Tool>,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Install the MDM services from the fetched archives (as root).
    Install {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where `mdm fetch` put the archives.
        #[arg(long, value_parser = HostPath::new)]
        bin_dir: Option<HostPath>,
        /// Roll back: stop the services and move the unit files aside.
        #[arg(long, conflicts_with = "bin_dir")]
        uninstall: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Get the MDM's TLS certificate over DNS-01 with lego, renewed by a timer, and
    /// start the proxy (as root).
    Tls {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Only ask GoDaddy's DNS API whether this account may use it (read only).
        #[arg(long, conflicts_with = "apply")]
        check_godaddy: bool,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Give NanoHUB the APNs push certificate, and write the enrollment profile the
    /// proxy serves (as root).
    PushCert {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// The push certificate (PEM) from the Apple Push Certificates Portal.
        #[arg(long, value_parser = HostPath::new, required_unless_present = "profile_only")]
        cert: Option<HostPath>,
        /// Its private key (PEM, unencrypted, mode 0600).
        #[arg(long, value_parser = HostPath::new, required_unless_present = "profile_only")]
        key: Option<HostPath>,
        /// Write the profile only, for --topic or the recorded topic.
        #[arg(long, conflicts_with_all = ["cert", "key"])]
        profile_only: bool,
        /// The push topic, com.apple.mgmt.External.<uuid> (with --profile-only).
        #[arg(long, requires = "profile_only", value_parser = |s: &str| Topic::new(s).map_err(|e| e.to_string()))]
        topic: Option<Topic>,
        /// Carry the plan out.
        #[arg(long)]
        apply: bool,
    },
    /// Link NanoDEP to Apple Business Manager and give it the ADE profile (as root).
    #[command(subcommand)]
    Dep(DepVerb),
    /// Check that the MDM is up and exposed only as intended (read only; use sudo).
    Verify {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Dial the proxy here instead of MDM_BIND:443 (a forwarded port, say); it must
        /// still prove MDM_DNS_NAME.
        #[arg(long)]
        via: Option<SocketAddr>,
    },
    /// Write an erase request for one Mac and sign it with your security key, on your
    /// own machine (as yourself, not root); check it as the gate will.
    EraseRequest {
        /// The Mac's serial number.
        #[arg(long)]
        serial: String,
        /// Why, in plain words (the gate's alerts quote it).
        #[arg(long)]
        reason: String,
        /// Hold the erase for this privileged lease instead of erasing now.
        #[arg(long)]
        lease: Option<String>,
        /// The security key's key file (`ssh-keygen -f`), with KEY.pub beside it.
        #[arg(long, value_parser = HostPath::new)]
        key: HostPath,
        /// Where the request goes; the signature goes to FILE.sig. Neither may exist.
        #[arg(long, value_parser = HostPath::new)]
        out: HostPath,
        /// Minutes from now the gate may accept the request (1 to 60).
        #[arg(long, default_value_t = 15)]
        valid_for: u32,
        /// The ssh-keygen to sign with (one with security key support).
        #[arg(long, value_parser = HostPath::new, default_value = erase::SSH_KEYGEN)]
        ssh_keygen: HostPath,
        /// Sign: write the request and its signature.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum DepVerb {
    /// Have NanoDEP make its token key pair; write the certificate to upload in ABM.
    GetCert {
        #[command(flatten)]
        with: DepArgs,
        /// Where the certificate goes: absolute, outside every git work tree.
        #[arg(long, value_parser = HostPath::new)]
        out: HostPath,
    },
    /// Give NanoDEP the server token (.p7m) ABM made for that certificate.
    UploadToken {
        #[command(flatten)]
        with: DepArgs,
        /// The token: absolute, outside every git work tree.
        #[arg(long, value_parser = HostPath::new)]
        token: HostPath,
    },
    /// Define the ADE profile at Apple and make it the one new devices get.
    DefineProfile {
        #[command(flatten)]
        with: DepArgs,
    },
    /// Assign the ADE profile to devices already in ABM.
    Assign {
        #[command(flatten)]
        with: DepArgs,
        /// A device's serial number (repeat for more).
        #[arg(long, required = true, value_parser = Serial::new)]
        serial: Vec<Serial>,
    },
}

#[derive(Args)]
struct DepArgs {
    /// The kit's settings, KEY=VALUE lines.
    #[arg(long)]
    config: PathBuf,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

#[derive(Subcommand)]
enum Minio {
    /// Make kbf's buckets, a policy for them and a service account key (see the docs of
    /// `kbf_deploy::minio::buckets`).
    Buckets(BucketsArgs),
    /// Add a NodePort Service for a MinIO Service's S3 port.
    Nodeport(NodePortArgs),
}

#[derive(Args)]
struct BucketsArgs {
    /// The MinIO, http://host[:port].
    #[arg(long, value_parser = minio::endpoint)]
    endpoint: HttpUrl,
    /// A bucket to make (repeat for more): kbf-<lowercase>.
    #[arg(long, required = true, value_parser = KbfName::new)]
    bucket: Vec<KbfName>,
    /// The name of the policy for the buckets.
    #[arg(long, value_parser = KbfName::new)]
    policy: KbfName,
    /// The name of the service account minted for the key file.
    #[arg(long, value_parser = KbfName::new)]
    account: KbfName,
    /// The MinIO client, by absolute path.
    #[arg(long, value_parser = HostPath::new)]
    mc: HostPath,
    /// Where the minted key pair goes (mode 0600); kept when it exists.
    #[arg(long, value_parser = HostPath::new)]
    key_file: Option<HostPath>,
    /// The user who owns the key file (the plan then applies only as root).
    #[arg(long, requires = "key_file")]
    key_owner: Option<String>,
    /// Remove the buckets with their objects, the account, the key file and the policy.
    #[arg(long)]
    remove: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
struct NodePortArgs {
    /// kubectl, by absolute path.
    #[arg(long, value_parser = HostPath::new)]
    kubectl: HostPath,
    /// The cluster admin's kubeconfig.
    #[arg(long, value_parser = HostPath::new)]
    kubeconfig: HostPath,
    /// The kubeconfig context; its current one when not given.
    #[arg(long)]
    context: Option<String>,
    /// The MinIO Service's namespace.
    #[arg(long, value_parser = K8sName::new)]
    namespace: K8sName,
    /// The MinIO Service.
    #[arg(long, value_parser = K8sName::new)]
    service: K8sName,
    /// The port every node opens, 30000 to 32767.
    #[arg(long, value_parser = clap::value_parser!(u16).range(30000..=32767))]
    node_port: u16,
    /// Delete the Service (only one carrying the kbf-deploy label).
    #[arg(long)]
    remove: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

/// The binary's entry point: [`run`] on the host, with the process's environment.
pub fn main(args: Vec<OsString>) -> ExitCode {
    let mut exec = HostExec::new(Root::host());
    run(
        args,
        &mut exec,
        &host_env,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// A variable of the process's environment.
fn host_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// Parse `args` and run the subcommand through `exec`, reading secrets with `env`.
pub fn run<E: Exec>(
    args: Vec<OsString>,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // --help and --version are "errors" that go to stdout and exit 0.
            let text = e.render().to_string();
            let _ = if e.use_stderr() {
                err.write_all(text.as_bytes())
            } else {
                out.write_all(text.as_bytes())
            };
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let result = match cli.kit {
        Kit::Host(h) => return crate::host::run(h, out, err),
        Kit::Minio(Minio::Buckets(a)) => minio_buckets(a, exec, env, out),
        Kit::Minio(Minio::Nodeport(a)) => minio_nodeport(a, exec, out),
        Kit::Linux(l) => return linux_kit(l, exec, out, err),
        Kit::Mdm(m) => return mdm_kit(m, exec, out, err),
        Kit::Envoy(e) => return envoy_kit(e, exec, out, err),
        Kit::Log(a) => return log::run(&a, out, err),
        Kit::Rollout(a) => return rollout::cli::run(a, None, out, err),
        Kit::Pilot(p) => {
            return pilot::cli::run(p, exec, env, out, err, &pilot::cli::sleep_a_second);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The `linux` kit: 0 done, 1 a check found a broken rule, 2 refused.
fn linux_kit<E: Exec>(
    l: Linux,
    exec: &mut E,
    mut out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = match l {
        Linux::RenderUnits {
            config,
            out: dir,
            kbf_uid,
            mode,
        } => match mode {
            None => linux::render_units(&config, &dir, kbf_uid),
            Some(mode) => linux::render_mode_unit(&config, &dir, kbf_uid, mode),
        }
        .map(|line| (vec![line], 0)),
        Linux::CheckUnits {
            config,
            dir,
            installable,
            mode,
            kbf_bin,
        } => match mode {
            None => linux::check_units(&config, &dir, installable),
            Some(mode) => {
                linux::check_mode_unit(exec, &config, &dir, mode, installable, kbf_bin.as_deref())
            }
        }
        .map(|report| {
            let code = u8::from(!report.failures.is_empty());
            let mut lines: Vec<String> = report.failures.iter().map(ToString::to_string).collect();
            lines.extend(report.notes);
            lines.push(report.summary);
            (lines, code)
        }),
        Linux::InstallUnits {
            config,
            bin_dir,
            restart_minio,
            uninstall,
            mode: unit_mode,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    if let Some(mode) = unit_mode {
                        let a = install_mode::InstallMode {
                            config,
                            mode,
                            bin_dir,
                        };
                        install_mode::run(exec, &a, self::mode(apply), &mut out)
                    } else if uninstall {
                        install::run_uninstall(exec, &config, &stamp(), mode(apply), &mut out)
                    } else {
                        let a = install::Install {
                            config,
                            bin_dir,
                            restart_minio,
                        };
                        install::run(exec, &a, mode(apply), &mut out)
                    }
                });
            return host_verb(done, err);
        }
        Linux::ImagePull {
            config,
            image,
            platform,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    let a = images::Pull {
                        config,
                        images: image,
                        platform,
                    };
                    images::run(exec, &a, mode(apply), &mut out)
                });
            return host_verb(done, err);
        }
        Linux::MetaBackup(a) => {
            return meta::cli::backup(a, exec, &meta::metrics::http_get, out, err);
        }
        Linux::MetaVerify(a) => return meta::cli::verify(&a, out, err),
        Linux::VerifyCert(a) => return linux::verify_cert::cli(&a, out, err),
        Linux::Swap {
            config,
            kubelet_allows_swap,
            remove,
            apply,
        } => {
            let done = LinuxConfig::load(&config)
                .map_err(HostError::from)
                .and_then(|config| {
                    let a = swap::Swap {
                        config,
                        kubelet_allows_swap,
                        remove,
                    };
                    swap::run(exec, &a, mode(apply), &mut out)
                });
            return host_verb(done, err);
        }
    };
    match result {
        Ok((lines, code)) => {
            for line in lines {
                // A closed stdout (`| head`) is not worth a panic; the exit code still says it.
                let _ = writeln!(out, "{line}");
            }
            ExitCode::from(code)
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// How long a download may stall, and a check through the proxy may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// The `mdm` kit: 0 done (or every check passed), 1 a step, download or check failed,
/// 2 refused.
fn mdm_kit<E: Exec>(m: Mdm, exec: &mut E, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let done = match m {
        Mdm::Fetch { dir, only, apply } => Client::system(FETCH_TIMEOUT)
            .map_err(MdmError::from)
            .and_then(|client| {
                let f = Fetch { dir, only };
                mdm::fetch::run(
                    &f,
                    &Pins::built_in(),
                    exec.euid(),
                    &client,
                    mode(apply),
                    out,
                )
            })
            .map(|()| true),
        Mdm::Install {
            config,
            bin_dir,
            uninstall,
            apply,
        } => MdmConfig::load(&config)
            .map_err(MdmError::from)
            .and_then(|config| match (uninstall, bin_dir) {
                (true, _) => mdm::install::run_uninstall(exec, &config, &stamp(), mode(apply), out),
                (false, Some(bin_dir)) => {
                    let a = Install { config, bin_dir };
                    mdm::install::run(exec, &a, &Pins::built_in(), mode(apply), out)
                }
                (false, None) => Err(MdmError::NoBinDir),
            })
            .map(|()| true),
        Mdm::Tls {
            config,
            check_godaddy,
            apply,
        } => MdmConfig::load(&config)
            .map_err(MdmError::from)
            .and_then(|c| {
                if check_godaddy {
                    let client = Client::system(VERIFY_TIMEOUT)?;
                    return mdm::tls::check_godaddy(&*exec, &c, &client, None, out);
                }
                let acme = Acme::load(&config)?;
                mdm::tls::run(exec, &c, &acme, mode(apply), out).map(|()| true)
            }),
        Mdm::PushCert {
            config,
            cert,
            key,
            profile_only: _,
            topic,
            apply,
        } => MdmConfig::load(&config)
            .map_err(MdmError::from)
            .and_then(|config| {
                let with = match (cert, key) {
                    (Some(cert), Some(key)) => With::Pair { cert, key },
                    _ => With::ProfileOnly(topic),
                };
                let a = PushCert { config, with };
                push_cert::run(exec, &a, SystemTime::now(), mode(apply), out)
            })
            .map(|()| true),
        Mdm::Dep(verb) => {
            let (with, d) = match verb {
                DepVerb::GetCert { with, out } => (with, Dep::GetCert { out }),
                DepVerb::UploadToken { with, token } => (with, Dep::UploadToken { token }),
                DepVerb::DefineProfile { with } => (with, Dep::DefineProfile),
                DepVerb::Assign { with, serial } => (with, Dep::Assign { serials: serial }),
            };
            MdmConfig::load(&with.config)
                .map_err(MdmError::from)
                .and_then(|c| mdm::dep::run(exec, &c, &d, mode(with.apply), out))
        }
        Mdm::Verify { config, via } => {
            MdmConfig::load(&config)
                .map_err(MdmError::from)
                .and_then(|config| {
                    let client = Client::system(VERIFY_TIMEOUT)?;
                    let v = Verify {
                        proxy: via.unwrap_or(SocketAddr::from((config.bind, 443))),
                        config: &config,
                        client: &client,
                        now: SystemTime::now(),
                    };
                    mdm::verify::run(&*exec, &v, out)
                })
        }
        Mdm::EraseRequest {
            serial,
            reason,
            lease,
            key,
            out: file,
            valid_for,
            ssh_keygen,
            apply,
        } => erase::random_nonce()
            .and_then(|nonce| {
                let a = EraseArgs {
                    serial,
                    lease,
                    reason,
                    key,
                    out: file,
                    valid_for,
                    ssh_keygen,
                };
                let now = i64::try_from(stamp_secs()).unwrap_or(i64::MAX);
                erase::run(exec, &a, Made { now, nonce }, mode(apply), out)
            })
            .map(|()| true),
    };
    match done {
        Ok(passed) => ExitCode::from(u8::from(!passed)),
        Err(e) if e.exit_code() == 1 => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// The `envoy` kit: 0 done (or every check passed), 1 a check (or an apply's step)
/// failed, 2 refused.
fn envoy_kit<E: Exec>(
    e: Envoy,
    exec: &mut E,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let done = match e {
        Envoy::Apply {
            config,
            kubectl,
            kubeconfig,
            refuse_example,
            apply,
        } => {
            let example = refuse_example.as_deref();
            let done = envoy_apply(
                &config,
                kubectl,
                kubeconfig,
                example,
                mode(apply),
                exec,
                out,
            );
            return match done {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) if e.exit_code() == 1 => {
                    let _ = writeln!(err, "FAIL {e}");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    let _ = writeln!(err, "REFUSED: {e}");
                    ExitCode::from(2)
                }
            };
        }
        Envoy::Render { config, out: dir } => envoy::render_files(&config, &dir),
        Envoy::Check { config, dir } => match envoy::check_files(&config, dir.as_deref()) {
            Ok(failures) if failures.is_empty() => Ok(envoy::check_passed()),
            Ok(failures) => {
                for f in &failures {
                    let _ = writeln!(out, "{}", f.line());
                }
                return ExitCode::FAILURE;
            }
            Err(e) => Err(e),
        },
    };
    match done {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// `envoy apply`, with a private kubectl cache directory.
fn envoy_apply<E: Exec>(
    config: &Path,
    kubectl: HostPath,
    kubeconfig: HostPath,
    example: Option<&Path>,
    mode: Mode,
    exec: &mut E,
    mut out: &mut dyn Write,
) -> Result<(), envoy::apply::ApplyError> {
    let config = envoy::EnvoyConfig::load(config)?;
    let example = envoy::apply::example(example)?;
    let scratch = Scratch::new().map_err(envoy::apply::ApplyError::Scratch)?;
    let args = envoy::apply::Apply {
        config,
        example,
        kubectl,
        kubeconfig,
        cache_dir: scratch.path(),
        wait: envoy::apply::Wait::DEFAULT,
    };
    envoy::apply::run(exec, &args, mode, &mut out)
}

/// A host-changing `linux` verb's exit: 0 done, 1 a step failed (or the check after
/// an apply did), 2 refused.
fn host_verb(done: Result<(), HostError>, err: &mut dyn Write) -> ExitCode {
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ HostError::Deploy(kbf_host::HostError::Step { .. })) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
        Err(e @ HostError::AfterApply(_)) => {
            let _ = writeln!(err, "FAIL {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

/// The directory name `install-units --uninstall` moves the files into: seconds since
/// the Unix epoch.
pub(crate) fn stamp() -> String {
    stamp_secs().to_string()
}

/// Seconds since the epoch, now.
fn stamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn mode(apply: bool) -> Mode {
    if apply { Mode::Apply } else { Mode::DryRun }
}

fn minio_buckets<E: Exec>(
    a: BucketsArgs,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    mut out: &mut dyn Write,
) -> Result<(), MinioError> {
    let admin = Admin {
        user: secret(env, "MINIO_ROOT_USER")?,
        password: secret(env, "MINIO_ROOT_PASSWORD")?,
    };
    let scratch = Scratch::new().map_err(MinioError::Scratch)?;
    let args = buckets::Buckets {
        endpoint: a.endpoint,
        buckets: a.bucket,
        policy: a.policy,
        account: a.account,
        key_file: a.key_file,
        key_owner: a.key_owner,
        mc: a.mc,
        mc_config_dir: scratch.path(),
        remove: a.remove,
    };
    let plan = buckets::plan(&*exec, &args, &admin, &mut out)?;
    execute(&plan, args.run_as(), mode(a.apply), exec, &mut out)?;
    Ok(())
}

fn minio_nodeport<E: Exec>(
    a: NodePortArgs,
    exec: &mut E,
    mut out: &mut dyn Write,
) -> Result<(), MinioError> {
    let scratch = Scratch::new().map_err(MinioError::Scratch)?;
    let args = nodeport::NodePort {
        kubectl: a.kubectl,
        kubeconfig: a.kubeconfig,
        context: a.context,
        cache_dir: scratch.path(),
        namespace: a.namespace,
        service: a.service,
        node_port: a.node_port,
        remove: a.remove,
    };
    let plan = nodeport::plan(&*exec, &args, &mut out)?;
    execute(&plan, RunAs::User, mode(a.apply), exec, &mut out)?;
    Ok(())
}

/// A secret from the environment: set, UTF-8 and not empty.
fn secret(env: &dyn Fn(&str) -> Option<OsString>, key: &'static str) -> Result<String, MinioError> {
    env(key)
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
        .ok_or(MinioError::NoAdmin(key))
}

/// A private directory (mode 0700) for a tool's state, removed when dropped.
struct Scratch(PathBuf);

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> io::Result<Scratch> {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kbf-deploy.{}.{n}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;

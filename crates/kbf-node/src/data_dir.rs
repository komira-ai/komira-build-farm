//! The daemon's data directory: one directory, `--data-dir`, holding everything the
//! daemon keeps on disk (`docs/design/worker-cache.md`):
//!
//! ```text
//! <data-dir>/leases   lease directories (container, native and whole-machine drivers)
//! <data-dir>/cache    the input cache leases' inputs come from: linked (container), cloned (native, macOS)
//! <data-dir>/trees    the shared input trees: mounted read-only (container), cloned whole (native, macOS)
//! ```
//!
//! They share one filesystem, which the cache and the trees need to link or clone
//! instead of copy. The cache and the trees are on by default under the container
//! driver and under the native driver on macOS (a lease's inputs there are clones, so
//! an action writes its own copy and the sandbox keeps the cache and the trees, outside
//! every lease directory, out of its reach). The native driver on Linux never uses them
//! (its actions run as the daemon's user, who owns the cache and could write it), so
//! it makes no `cache` or `trees`. Nothing is sized by a
//! flag: the budgets come from the filesystem ([`Budgets`]), and `--data-max-bytes`
//! only lowers them. The daemon logs every directory and budget it resolved at start.
//!
//! A cache that cannot be used never stops the daemon: it warns with the reason
//! (`cache: off:<reason>`) and runs uncached.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use kbf_threads::disk::{FreeSpace, Space, keep_free, percent_of};

/// The default `--data-dir`.
pub const DEFAULT_DATA_DIR: &str = "/var/lib/kbf";

/// The mode of the data directory itself: the OCI backend's container root is a
/// subordinate id, an "other" to the daemon's directories, and must search every
/// directory down to a tree.
const DATA_MODE: u32 = 0o755;

/// The mode of `leases` and `cache`: the daemon's alone.
const PRIVATE_MODE: u32 = 0o700;

/// The mode of `leases` under the OCI backend: its container ids are "others" to the
/// daemon and must search down to the lease directories.
const OPEN_MODE: u32 = 0o711;

/// The cache's share of what [`Budgets`] sizes from, in percent; the trees' is a
/// quarter. The rest stays for the leases.
const CACHE_PERCENT: u64 = 50;
const TREES_PERCENT: u64 = 25;

/// The `--data-dir` and `--data-max-bytes` flags.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct DataArgs {
    /// The directory the daemon keeps everything in, on a local disk: `leases/` (lease
    /// directories, the container and native drivers), and under the container driver (and the
    /// native driver on macOS)
    /// `cache/` (the input cache, `docs/design/worker-cache.md`) and `trees/` (shared
    /// read-only input trees, `docs/design/fast-sandbox.md#4-shared-trees`). Made if
    /// missing; absolute. The cache and trees are on, sized from the filesystem: half
    /// and a quarter of its size less what it keeps free (5%, at least 1 GiB), and
    /// the cache evicts entries no running lease links to keep that much free. A
    /// directory another daemon owns, or that cannot be used, is left alone: the
    /// daemon warns and runs uncached.
    ///
    /// Default `/var/lib/kbf`. A daemon with the fake driver alone touches no directory
    /// unless this is given.
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Lowers the size the cache and the trees are budgeted from (half and a quarter
    /// of it), under the filesystem's automatic one.
    #[arg(long, value_name = "BYTES", value_parser = clap::value_parser!(u64).range(1..))]
    pub data_max_bytes: Option<u64>,
    /// The node's one cache, when the process runs local storage on the same disk
    /// (`kbf-all-in-one`, [`crate::Cli::share_cache`]); no flag sets it.
    #[arg(skip)]
    pub local_cache: Option<std::sync::Arc<kbf_daemon::cache::LocalCache>>,
}

impl crate::Cli {
    /// Makes the input cache the node's one cache with local storage, `local`: for a
    /// binary that runs local storage on the same disk in its own process
    /// (`kbf-all-in-one`; see `kbf_storage::local_cache`).
    pub fn share_cache(&mut self, local: std::sync::Arc<kbf_daemon::cache::LocalCache>) {
        self.data.local_cache = Some(local);
    }

    /// `--data-dir`, as the daemon resolves it (`/var/lib/kbf` when it is not given).
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.data.dir()
    }
}

impl DataArgs {
    /// `--data-dir`, or [`DEFAULT_DATA_DIR`].
    pub(crate) fn dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
    }

    /// `<data-dir>/leases`.
    pub(crate) fn leases(&self) -> PathBuf {
        self.dir().join("leases")
    }
}

/// What the cache and the trees may take, from the data directory's filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budgets {
    /// The most bytes the input cache keeps.
    pub cache_max_bytes: u64,
    /// Below this many free bytes, the cache evicts entries no lease links.
    pub cache_min_free_bytes: u64,
    /// The most file bytes of trees kept.
    pub trees_max_bytes: u64,
}

impl Budgets {
    /// From the filesystem's size and the optional `--data-max-bytes`; `None` when
    /// the filesystem is no larger than what it keeps free.
    pub(crate) fn of(space: Space, max: Option<u64>) -> Option<Self> {
        // What both are sized from: the filesystem less its floor, at most the cap.
        let base = space.of_capacity(100, max);
        let cache = percent_of(base, CACHE_PERCENT);
        let trees = percent_of(base, TREES_PERCENT);
        // The trees' quarter is the smaller: none of it is none of the cache's either.
        (trees > 0).then_some(Self {
            cache_max_bytes: cache,
            cache_min_free_bytes: keep_free(space.total),
            trees_max_bytes: trees,
        })
    }
}

/// What a start resolved the data directory to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// `--data-dir`.
    pub dir: PathBuf,
    /// Whether a driver that uses them runs (the container driver; the native driver on
    /// macOS), and so `cache` and `trees` are made.
    pub cached: bool,
    /// The budgets; `None` for a filesystem too small for a cache.
    pub budgets: Option<Budgets>,
    /// Whether `leases` is searchable by others (the OCI backend).
    pub open_leases: bool,
}

/// Why the data directory is not usable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DataDirError {
    /// `--data-dir` is not absolute.
    #[error("--data-dir {0} must be absolute")]
    Relative(PathBuf),
    /// The filesystem cannot be read.
    #[error("--data-dir {dir}: {source}")]
    Space {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A directory cannot be made.
    #[error("--data-dir: make {dir}: {source}")]
    Make {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Plan {
    /// Resolves `args` for a daemon that runs a driver using the cache or not, sizing from
    /// `space`. Changes nothing on disk.
    ///
    /// # Errors
    /// `--data-dir` is relative, or its filesystem cannot be read.
    pub(crate) fn new(
        args: &DataArgs,
        cached: bool,
        space: &dyn FreeSpace,
    ) -> Result<Self, DataDirError> {
        let dir = args.dir();
        if !dir.is_absolute() {
            return Err(DataDirError::Relative(dir));
        }
        let on = space.space(&dir).map_err(|source| DataDirError::Space {
            dir: dir.clone(),
            source,
        })?;
        Ok(Self {
            dir,
            cached,
            budgets: Budgets::of(on, args.data_max_bytes),
            open_leases: false,
        })
    }

    /// Whether `leases` must be searchable by others: the OCI backend's container ids
    /// are not the daemon user's.
    #[must_use]
    pub(crate) fn with_open_leases(mut self, open: bool) -> Self {
        self.open_leases = open;
        self
    }

    /// `<dir>/leases`.
    pub(crate) fn leases(&self) -> PathBuf {
        self.dir.join("leases")
    }

    /// `<dir>/cache`.
    pub(crate) fn cache(&self) -> PathBuf {
        self.dir.join("cache")
    }

    /// `<dir>/trees`.
    pub(crate) fn trees(&self) -> PathBuf {
        self.dir.join("trees")
    }

    /// Makes the data directory (0755) and `leases` (0700; 0711 with the OCI backend), and
    /// `cache` (0700) for a driver that uses it. `trees` is made by its store, with its
    /// own mode. A directory that exists keeps its mode, except that an existing `leases`
    /// closed to others is opened (0711) under the OCI backend, logged once.
    ///
    /// # Errors
    /// A directory cannot be made.
    pub(crate) fn create(&self) -> Result<(), DataDirError> {
        make(&self.dir, DATA_MODE)?;
        if self.open_leases {
            make(&self.leases(), OPEN_MODE)?;
            open(&self.leases())?;
        } else {
            make(&self.leases(), PRIVATE_MODE)?;
        }
        if self.cached {
            make(&self.cache(), PRIVATE_MODE)?;
        }
        Ok(())
    }

    /// The lines the start logs: every directory and budget.
    pub(crate) fn describe(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "data dir {}: leases {}",
            self.dir.display(),
            self.leases().display()
        )];
        match (self.cached, &self.budgets) {
            (false, _) => lines.push(
                "cache: off:no driver that uses it (the native driver's actions run as the \
                 daemon's user off macOS and could write the cache); \
                 trees: off:no driver that uses them"
                    .to_owned(),
            ),
            (true, None) => lines.push(
                "cache: off:filesystem too small for a cache; trees: off:filesystem too small"
                    .to_owned(),
            ),
            (true, Some(b)) => {
                lines.push(format!(
                    "cache {}: up to {} bytes, evicting to keep {} bytes free",
                    self.cache().display(),
                    b.cache_max_bytes,
                    b.cache_min_free_bytes
                ));
                lines.push(format!(
                    "trees {}: up to {} bytes",
                    self.trees().display(),
                    b.trees_max_bytes
                ));
            }
        }
        lines
    }
}

fn make(dir: &Path, mode: u32) -> Result<(), DataDirError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .map_err(made(dir))
}

/// Adds search for group and others to `dir` when others cannot search it, which an
/// earlier start (before the OCI backend was selected) left; logged once, then it is
/// open and this does nothing.
fn open(dir: &Path) -> Result<(), DataDirError> {
    let meta = std::fs::metadata(dir).map_err(made(dir))?;
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o001 == 0 {
        let now = std::fs::Permissions::from_mode(mode | 0o011);
        std::fs::set_permissions(dir, now).map_err(made(dir))?;
        let said = format!(
            "data dir: {} was {mode:o}, now {:o}: the OCI backend's container ids must search it",
            dir.display(),
            mode | 0o011
        );
        tracing::info!("{said}");
    }
    Ok(())
}

/// The error for a directory that cannot be made or opened.
fn made(dir: &Path) -> impl FnOnce(std::io::Error) -> DataDirError {
    move |source| DataDirError::Make {
        dir: dir.to_owned(),
        source,
    }
}

/// The container driver's cache and trees, for the driver to open; `None`, after a
/// warning, when the filesystem is too small or the
/// daemon's own path is unknown.
///
/// No plan, no cache. The cache is the node's one cache with local storage if `data`
/// has one ([`DataArgs::local_cache`]).
#[cfg(target_os = "linux")]
pub(crate) fn given(
    plan: Option<&Plan>,
    data: &DataArgs,
) -> Option<kbf_driver_container::CacheUse> {
    use kbf_daemon::cache::{CacheConfig, Disabled};
    let plan = plan?;
    let shown = plan.cache().display().to_string();
    let budgets = plan.budgets.ok_or(Disabled::NoBudget);
    // The helper is this binary, by the path it was started from: after an update
    // renames a new binary over it, that path names the new one.
    let helper = std::env::current_exe()
        .ok()
        .ok_or(Disabled::Unusable("the daemon's own path".to_owned()));
    match budgets.and_then(|budgets| helper.map(|helper| (budgets, helper))) {
        Ok((budgets, helper)) => {
            let mut config = CacheConfig::new(plan.cache(), plan.leases(), budgets.cache_max_bytes);
            config.min_free_bytes = budgets.cache_min_free_bytes;
            config.local.clone_from(&data.local_cache);
            let trees = kbf_daemon::trees::TreeConfig::new(plan.trees(), budgets.trees_max_bytes);
            Some(kbf_driver_container::CacheUse {
                config,
                helper,
                trees: Some(trees),
            })
        }
        Err(why) => {
            let reason = why.reason();
            tracing::warn!("cache: off:{reason} ({shown}: {why}); leases run uncached");
            None
        }
    }
}

/// The native driver's cache on macOS: `<data-dir>/cache`, opened (the daemon owns its
/// lock and layout) and linked from `<data-dir>/leases`, so every lease's inputs are
/// clones of its entries. `None` (warned about as `cache: off:<reason>`, leases run
/// uncached) for a filesystem too small for one, or a cache that cannot be used or
/// another daemon owns. The cache lies outside every lease directory, which the
/// sandbox lets nothing write.
#[cfg(target_os = "macos")]
pub(crate) fn native_cache(plan: &Plan) -> Option<std::sync::Arc<kbf_daemon::cache::Cache>> {
    use kbf_daemon::cache::{Cache, CacheConfig, Disabled};
    let shown = plan.cache().display().to_string();
    let why = plan.budgets.ok_or(Disabled::NoBudget).and_then(|budgets| {
        let mut config = CacheConfig::new(plan.cache(), plan.leases(), budgets.cache_max_bytes);
        config.min_free_bytes = budgets.cache_min_free_bytes;
        Cache::open(config)
    });
    match why {
        Ok(cache) => {
            let stats = cache.stats();
            let (bytes, entries, mode) = (stats.bytes, stats.entries, cache.link_mode());
            tracing::info!("cache: {shown}: {bytes} bytes in {entries} entries, link mode {mode}");
            Some(cache)
        }
        Err(why) => {
            let reason = why.reason();
            tracing::warn!("cache: off:{reason} ({shown}: {why}); leases run uncached");
            None
        }
    }
}

/// The native driver's shared trees on macOS: `<data-dir>/trees`, opened; `None`
/// (warned about as `trees: off:<reason>`, inputs are cloned file by file) when there is
/// no cache to build them from, the filesystem is too small for them or the store cannot
/// be used.
#[cfg(target_os = "macos")]
pub(crate) fn native_trees(
    plan: &Plan,
    cache: Option<&std::sync::Arc<kbf_daemon::cache::Cache>>,
) -> Option<std::sync::Arc<kbf_daemon::trees::Trees>> {
    let shown = plan.trees().display().to_string();
    let why = if cache.is_none() {
        Err((
            "no-cache",
            "the trees are built from the cache's entries".to_owned(),
        ))
    } else {
        match plan.budgets {
            None => Err(("no-budget", "filesystem too small".to_owned())),
            Some(budgets) => {
                let config =
                    kbf_daemon::trees::TreeConfig::new(plan.trees(), budgets.trees_max_bytes);
                kbf_daemon::trees::Trees::open(config).map_err(|e| ("unusable", e.to_string()))
            }
        }
    };
    match why {
        Ok(trees) => {
            let stats = trees.store().stats();
            let (count, bytes) = (stats.trees, stats.bytes);
            tracing::info!(
                "trees: {shown}: {count} trees, {bytes} bytes; cloned whole into leases"
            );
            Some(std::sync::Arc::new(trees))
        }
        Err((reason, why)) => {
            tracing::warn!("trees: off:{reason} ({shown}: {why}); inputs are cloned file by file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use clap::Parser as _;
    use kbf_threads::disk::{FreeSpace, Space};

    use super::{Budgets, DEFAULT_DATA_DIR, DataArgs, DataDirError, Plan};

    const GIB: u64 = 1 << 30;

    /// A filesystem of one size.
    struct Fake(Space);

    impl FreeSpace for Fake {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Ok(self.0)
        }
    }

    /// A filesystem that cannot be read.
    struct Broken;

    impl FreeSpace for Broken {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        data: DataArgs,
    }

    fn args(flags: &[&str]) -> DataArgs {
        Cli::parse_from(std::iter::once("kbf-daemon").chain(flags.iter().copied())).data
    }

    /// Catches: a default data directory other than `/var/lib/kbf`, the lease directory
    /// not under it, and `--data-max-bytes` of 0 (a budget of nothing) accepted.
    #[test]
    fn the_data_directory_defaults_and_holds_the_leases() {
        let data = args(&[]);
        assert_eq!(data.data_dir, None);
        assert_eq!(data.dir(), Path::new(DEFAULT_DATA_DIR));
        assert_eq!(data.data_max_bytes, None);
        assert_eq!(args(&["--data-dir=/d"]).dir(), Path::new("/d"));
        assert_eq!(data.leases(), Path::new("/var/lib/kbf/leases"));
        assert!(Cli::try_parse_from(["kbf-daemon", "--data-max-bytes=0"]).is_err());
        assert_eq!(args(&["--data-max-bytes=7"]).data_max_bytes, Some(7));
    }

    /// Catches budgets that are not the filesystem's: on a 1000 GiB disk the cache gets
    /// half and the trees a quarter of what is left after the 5% floor, the cache evicts
    /// to keep that floor free; on a 10 GiB disk the floor is the 1 GiB minimum; a
    /// `--data-max-bytes` lowers them (and never raises them); a disk no larger than
    /// its floor has no cache.
    #[test]
    fn budgets_come_from_the_filesystem_and_the_cap_only_lowers_them() {
        let disk = |total| Space {
            total,
            available: total / 2,
        };
        let big = Budgets::of(disk(1000 * GIB), None).expect("room");
        let room = 950 * GIB;
        assert_eq!(big.cache_max_bytes, room / 2);
        assert_eq!(big.trees_max_bytes, room / 4);
        assert_eq!(big.cache_min_free_bytes, 50 * GIB);
        let small = Budgets::of(disk(10 * GIB), None).expect("room");
        assert_eq!(small.cache_min_free_bytes, GIB);
        assert_eq!(small.cache_max_bytes, 9 * GIB / 2);
        let capped = Budgets::of(disk(1000 * GIB), Some(100 * GIB)).expect("room");
        assert_eq!(capped.cache_max_bytes, 50 * GIB);
        assert_eq!(capped.trees_max_bytes, 25 * GIB);
        assert_eq!(capped.cache_min_free_bytes, 50 * GIB);
        let above = Budgets::of(disk(1000 * GIB), Some(5000 * GIB)).expect("room");
        assert_eq!(above, big);
        assert_eq!(Budgets::of(disk(GIB), None), None);
        assert_eq!(Budgets::of(disk(0), None), None);
    }

    /// Catches a plan that ignores the filesystem it is given (the numbers from a real
    /// `statvfs` in a test), a relative directory accepted, and an unreadable
    /// filesystem passed over.
    #[test]
    fn the_plan_sizes_from_its_filesystem_source() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let plan = Plan::new(&args(&["--data-dir=/d"]), true, &Fake(space)).expect("plan");
        assert_eq!(plan.budgets, Budgets::of(space, None));
        assert_eq!(plan.cache(), Path::new("/d/cache"));
        assert_eq!(plan.trees(), Path::new("/d/trees"));
        assert_eq!(plan.leases(), Path::new("/d/leases"));
        let lines = plan.describe().join("\n");
        assert!(
            lines.contains("cache /d/cache: up to 102005473280 bytes"),
            "{lines}"
        );
        assert!(
            lines.contains("trees /d/trees: up to 51002736640 bytes"),
            "{lines}"
        );
        let native = Plan::new(&args(&["--data-dir=/d"]), false, &Fake(space)).expect("plan");
        assert!(native.describe().join("\n").contains("cache: off:"));
        let relative = Plan::new(&args(&["--data-dir=d"]), true, &Fake(space));
        assert!(matches!(relative, Err(DataDirError::Relative(_))));
        let broken = Plan::new(&args(&["--data-dir=/d"]), true, &Broken);
        assert!(matches!(broken, Err(DataDirError::Space { .. })));
        let tiny = Space {
            total: GIB,
            available: GIB,
        };
        let none = Plan::new(&args(&["--data-dir=/d"]), true, &Fake(tiny)).expect("plan");
        assert_eq!(none.budgets, None);
        assert!(none.describe().join("\n").contains("too small"));
    }

    /// Catches the layout made with the wrong modes: the data directory 0755 (a
    /// container root must search down to a tree), `leases` and `cache` 0700; `cache`
    /// made only for the container driver, and `trees` left to its store; an existing
    /// directory's mode left as the operator set it.
    #[test]
    fn the_layout_is_made_with_its_modes() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let base = crate::tests::scratch("data-dir-layout");
        let mode = |p: &Path| std::fs::metadata(p).expect("made").permissions().mode() & 0o777;
        let flags = [format!("--data-dir={}", base.join("a").display())];
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        let plan = Plan::new(&args(&flags), true, &Fake(space)).expect("plan");
        plan.create().expect("create");
        assert_eq!(mode(&plan.dir), 0o755);
        assert_eq!(mode(&plan.leases()), 0o700);
        assert_eq!(mode(&plan.cache()), 0o700);
        assert!(!plan.trees().exists(), "the trees store makes its own");
        let flags = [format!("--data-dir={}", base.join("b").display())];
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        let native = Plan::new(&args(&flags), false, &Fake(space)).expect("plan");
        native.create().expect("create");
        assert!(native.leases().is_dir());
        assert!(!native.cache().exists());
        std::fs::set_permissions(plan.leases(), std::fs::Permissions::from_mode(0o750))
            .expect("chmod");
        plan.create().expect("again");
        assert_eq!(mode(&plan.leases()), 0o750, "an existing mode is kept");
    }

    /// Catches the container driver's cache and trees not being the plan's: the cache in
    /// `<data>/cache` linking from `<data>/leases`, its budget, its eviction floor and
    /// the trees' budget the plan's, the trees in `<data>/trees` with the daemon's own
    /// thresholds; and no cache at all for a filesystem too small for one or no plan.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_container_driver_is_given_the_plans_cache_and_trees() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let data = args(&["--data-dir=/d"]);
        let plan = Plan::new(&data, true, &Fake(space)).expect("plan");
        let given = super::given(Some(&plan), &data).expect("a cache");
        let budgets = plan.budgets.expect("budgets");
        assert_eq!(given.config.dir, Path::new("/d/cache"));
        assert_eq!(given.config.scratch, Path::new("/d/leases"));
        assert_eq!(given.config.max_bytes, budgets.cache_max_bytes);
        assert_eq!(given.config.min_free_bytes, budgets.cache_min_free_bytes);
        assert_eq!(given.config.link_concurrency, 64);
        let trees = given.trees.expect("trees");
        assert_eq!(trees.dir, Path::new("/d/trees"));
        assert_eq!(trees.max_bytes, budgets.trees_max_bytes);
        assert_eq!(trees.min_files, kbf_daemon::trees::MIN_FILES);
        assert_eq!(trees.max_mounts, kbf_daemon::trees::MAX_MOUNTS);
        let tiny = Space {
            total: GIB,
            available: GIB,
        };
        let plan = Plan::new(&data, true, &Fake(tiny)).expect("plan");
        assert!(super::given(Some(&plan), &data).is_none());
        assert!(super::given(None, &data).is_none());
    }

    /// Catches the native driver on macOS not being given the plan's cache and trees:
    /// the cache in `<data>/cache` (opened, with its layout), the trees in `<data>/trees`
    /// (0711, outside `leases`), no trees without a cache, a store that cannot be opened
    /// (a file where it goes) turned off while the cache stays, and no cache for a
    /// filesystem too small for one.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_native_driver_on_macos_is_given_the_plans_cache_and_trees() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let plan_in = |name: &str| {
            let base = crate::tests::scratch(name);
            let flags = [format!("--data-dir={}", base.display())];
            let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
            let plan = Plan::new(&args(&flags), true, &Fake(space)).expect("plan");
            plan.create().expect("create");
            plan
        };
        let plan = plan_in("data-dir-native");
        let cache = super::native_cache(&plan).expect("a cache");
        assert!(plan.cache().join("v1").is_dir(), "no layout");
        assert_eq!(cache.link_mode().to_string(), "clone");
        let trees = super::native_trees(&plan, Some(&cache)).expect("trees");
        let mode = std::fs::metadata(plan.trees())
            .expect("made")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o711);
        assert!(!plan.trees().starts_with(plan.leases()));
        drop(trees);
        assert!(
            super::native_trees(&plan, None).is_none(),
            "built from nothing"
        );

        let blocked = plan_in("data-dir-native-blocked");
        std::fs::write(blocked.trees(), b"not a directory").expect("plant a file");
        let cache = super::native_cache(&blocked).expect("a cache");
        assert!(super::native_trees(&blocked, Some(&cache)).is_none());

        let tiny = Space {
            total: GIB,
            available: GIB,
        };
        let mut small = plan_in("data-dir-native-tiny");
        small.budgets = Budgets::of(tiny, None);
        assert!(super::native_cache(&small).is_none());
    }

    /// Catches a directory that cannot be made taken as made: `--data-dir` under a file
    /// stops the start with the directory named, rather than failing later in a driver.
    #[test]
    fn a_directory_that_cannot_be_made_is_an_error_naming_it() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let base = crate::tests::scratch("data-dir-under-a-file");
        let file = base.join("a-file");
        std::fs::write(&file, b"not a directory").expect("plant a file");
        let flags = [format!("--data-dir={}", file.join("data").display())];
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        let plan = Plan::new(&args(&flags), true, &Fake(space)).expect("plan");
        let error = plan.create().expect_err("under a file");
        assert!(matches!(error, DataDirError::Make { .. }), "{error}");
        assert!(error.to_string().contains("a-file/data"), "{error}");
    }

    /// Catches the OCI backend's start failing on a fresh data directory: `leases` made
    /// 0700, which the backend refuses ("not searchable by others"). With the backend
    /// selected `leases` is 0711, and an existing 0700 one is repaired to 0711; without,
    /// 0700 stays, and a mode the operator opened further is not narrowed.
    #[test]
    fn leases_are_searchable_by_others_under_the_oci_backend() {
        let space = Space {
            total: 200 * GIB,
            available: 150 * GIB,
        };
        let base = crate::tests::scratch("data-dir-oci-leases");
        let mode = |p: &Path| std::fs::metadata(p).expect("made").permissions().mode() & 0o777;
        let plan_at = |name: &str, open: bool| {
            let flags = [format!("--data-dir={}", base.join(name).display())];
            let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
            Plan::new(&args(&flags), true, &Fake(space))
                .expect("plan")
                .with_open_leases(open)
        };
        let fresh = plan_at("fresh", true);
        fresh.create().expect("create");
        assert_eq!(mode(&fresh.leases()), 0o711, "a fresh leases dir");
        assert_eq!(mode(&fresh.cache()), 0o700, "the cache stays the daemon's");
        let old = plan_at("old", false);
        old.create().expect("create");
        assert_eq!(
            mode(&old.leases()),
            0o700,
            "without the container driver leases stays 0700"
        );
        let old = plan_at("old", true);
        old.create().expect("repair");
        assert_eq!(mode(&old.leases()), 0o711, "an existing 0700 is repaired");
        std::fs::set_permissions(old.leases(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        old.create().expect("again");
        assert_eq!(mode(&old.leases()), 0o755, "a wider mode is kept");
    }
}

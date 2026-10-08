//! Apple's public software catalogue (`gdmf.apple.com/v2/pmv`) for macOS
//! (`docs/design/mdm-backend.md` section M2.2, `fleet-updates.md` sections 3.2 and 7.2).
//!
//! "Update available" is not asked of a Mac: the gate reads this catalogue at most
//! once a day and reports each listed build with its posting and expiry dates, and the
//! server compares it with each Mac's build. A build can be enforced only while the
//! catalogue lists it; after its expiry date it cannot.
//!
//! The document has `PublicAssetSets` (what Apple offers everyone) and `AssetSets`
//! (also what managed devices may still be held at). Each holds a `macOS` list of
//! entries with `ProductVersion`, `Build`, `PostingDate`, `ExpirationDate` and
//! `SupportedDevices`. [`parse_pmv`] keeps both lists, marking each entry with the one it
//! came from. A document whose `macOS` entries do not all parse is refused whole, so a
//! caller keeps its previous catalogue rather than one with a build missing or an
//! expiry misread.
//!
//! **No network here.** [`DailyCatalogue`] reads through an injected
//! [`CatalogueFetcher`]; the HTTPS fetcher (which may need Apple's root configured
//! explicitly, M2.2) belongs to the program that runs it. Filtering by a Mac's model
//! through `SupportedDevices` is an open assumption (M2.2), so entries carry the list
//! and nothing filters on it yet.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::names::{Date, OsVersion, is_build};

/// One macOS release the catalogue lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogueEntry {
    /// `ProductVersion`, for example `27.0.1`.
    pub product_version: OsVersion,
    /// `Build`, for example `26A434`: ASCII letters and digits.
    pub build: String,
    /// `PostingDate`.
    pub posting_date: Date,
    /// `ExpirationDate`: after it the build can no longer be enforced.
    pub expiration_date: Date,
    /// `SupportedDevices`: Apple's board identifiers.
    pub supported_devices: Vec<String>,
    /// From `PublicAssetSets` (true) or `AssetSets` (false).
    pub public: bool,
}

/// Why a catalogue document was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    /// Not JSON.
    #[error("the catalogue is not JSON: {0}")]
    Json(String),
    /// `PublicAssetSets.macOS` is missing, or that or `AssetSets.macOS` is not a list.
    #[error("the catalogue's {0}.macOS is missing or not a list")]
    NotAList(&'static str),
    /// An entry is malformed.
    #[error("macOS entry {index} of {set}: {problem}")]
    Entry {
        /// `PublicAssetSets` or `AssetSets`.
        set: &'static str,
        /// Its position in the list.
        index: usize,
        /// What is wrong with it.
        problem: String,
    },
}

/// Parses the catalogue's macOS entries, public ones first.
///
/// # Errors
/// [`CatalogueError`] if the document is not JSON, has no `PublicAssetSets.macOS`
/// list, has an `AssetSets.macOS` that is not a list, or has a malformed entry.
pub fn parse_pmv(document: &[u8]) -> Result<Vec<CatalogueEntry>, CatalogueError> {
    let doc: Value =
        serde_json::from_slice(document).map_err(|e| CatalogueError::Json(e.to_string()))?;
    let mut entries = Vec::new();
    for (set, public) in [("PublicAssetSets", true), ("AssetSets", false)] {
        let list = match doc.get(set).and_then(|s| s.get("macOS")) {
            Some(Value::Array(list)) => list,
            None if !public => continue,
            _ => return Err(CatalogueError::NotAList(set)),
        };
        for (index, entry) in list.iter().enumerate() {
            let parsed = parse_entry(entry, public).map_err(|problem| CatalogueError::Entry {
                set,
                index,
                problem,
            })?;
            entries.push(parsed);
        }
    }
    Ok(entries)
}

fn parse_entry(entry: &Value, public: bool) -> Result<CatalogueEntry, String> {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no string {key}"))
    };
    let product_version = OsVersion::parse(text("ProductVersion")?).map_err(|e| e.to_string())?;
    let build = text("Build")?;
    if !is_build(build) {
        return Err(format!("Build {build:?} is not ASCII letters and digits"));
    }
    let posting_date = Date::parse(text("PostingDate")?).map_err(|e| e.to_string())?;
    let expiration_date = Date::parse(text("ExpirationDate")?).map_err(|e| e.to_string())?;
    let supported_devices = match entry.get("SupportedDevices") {
        None => Vec::new(),
        Some(Value::Array(devices)) => devices
            .iter()
            .map(|d| d.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or("SupportedDevices holds a non-string")?,
        Some(_) => return Err("SupportedDevices is not a list".to_owned()),
    };
    Ok(CatalogueEntry {
        product_version,
        build: build.to_owned(),
        posting_date,
        expiration_date,
        supported_devices,
        public,
    })
}

/// What a fetch returns: the document's bytes, or why it could not be read.
pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// Reads the catalogue document. The real one speaks HTTPS to Apple; tests inject one
/// that returns fixed bytes.
pub trait CatalogueFetcher: Send + Sync {
    /// Fetches the document once.
    fn fetch(&self) -> FetchFuture<'_>;
}

/// The longest a catalogue is kept before it is read again: Apple asks services to
/// read it at most once a day.
pub const REFRESH_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// How long after a failed read the next one is tried.
pub const RETRY_AFTER_FAILURE_MS: u64 = 60 * 60 * 1000;

/// The catalogue as last read successfully.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// When it was read, milliseconds since the Unix epoch.
    pub fetched_at_unix_ms: u64,
    /// Its macOS entries.
    pub entries: Vec<CatalogueEntry>,
}

/// What [`DailyCatalogue::refresh`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Too soon after the last read; nothing was fetched.
    NotDue,
    /// Read and parsed; it is now the current snapshot.
    Updated,
    /// The fetch or the parse failed; the previous snapshot, if any, stays current.
    Failed(String),
}

/// The catalogue, read at most once per [`REFRESH_INTERVAL_MS`] (and once per
/// [`RETRY_AFTER_FAILURE_MS`] after a failure). Time is an input, so tests need no
/// clock.
pub struct DailyCatalogue<F> {
    fetcher: F,
    next_read_unix_ms: u64,
    current: Option<Snapshot>,
}

impl<F: CatalogueFetcher> DailyCatalogue<F> {
    /// A catalogue never read; the first [`refresh`](Self::refresh) reads it.
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            next_read_unix_ms: 0,
            current: None,
        }
    }

    /// The last successful read, if any.
    pub fn current(&self) -> Option<&Snapshot> {
        self.current.as_ref()
    }

    /// Reads the catalogue if it is due at `now_unix_ms`.
    pub async fn refresh(&mut self, now_unix_ms: u64) -> Refresh {
        if now_unix_ms < self.next_read_unix_ms {
            return Refresh::NotDue;
        }
        let read = match self.fetcher.fetch().await {
            Ok(bytes) => parse_pmv(&bytes).map_err(|e| e.to_string()),
            Err(e) => Err(format!("fetch failed: {e}")),
        };
        match read {
            Ok(entries) => {
                self.current = Some(Snapshot {
                    fetched_at_unix_ms: now_unix_ms,
                    entries,
                });
                self.next_read_unix_ms = now_unix_ms.saturating_add(REFRESH_INTERVAL_MS);
                Refresh::Updated
            }
            Err(e) => {
                self.next_read_unix_ms = now_unix_ms.saturating_add(RETRY_AFTER_FAILURE_MS);
                Refresh::Failed(e)
            }
        }
    }
}

//! An action's shape: what stays the same across builds of one target's action, read
//! from its `Command` alone, and a short key over it ([`ShapeKey`]).
//!
//! buck2 sends no per-action identity (its `RequestMetadata` names none), and the
//! action digest changes with every input, so the shape is built from two things a
//! client cannot leave out: the program it runs and the paths it writes. Each is made
//! independent of the configuration and of the content hashes buck2 puts in paths:
//!
//! - **Output paths** (`pattern`): under `buck-out/<v>/<root>/` (`art`, `art-anon`,
//!   `art-bxl`, and the `gen` kinds of older buck2) everything up to the root goes,
//!   and with it the hashes buck2 puts where its owner's layout says:
//!   - a target's configuration hash after the cell (`<16 hex>` or
//!     `<16 hex>-<16 hex>`, kept aside as `cfg`), and the content hash or
//!     `output_artifacts` placeholder in the one segment right after its
//!     `__<name>__` (a hex directory further down is kept). So
//!     `buck-out/v2/art/cell/<cfg>/pkg/__t__/out.o`,
//!     `buck-out/v2/art/cell/pkg/__t__/output_artifacts/out.o` and
//!     `buck-out/v2/gen/cell/pkg/__t__/<content>/out.o` are all `cell/pkg/__t__/out.o`.
//!     A dynamic action's `__action__<k>__` stays, so its actions stay apart. A
//!     target name may hold a `/` (`__foo/bar__`): a name runs from a segment that
//!     starts with `__` to the first later one that ends with `__`;
//!   - an anonymous target's configuration hash glued to its package, and the
//!     attributes or content hash right before its `__<name>__`: any segment of hex
//!     digits there, whatever its length (buck2 writes the attributes hash
//!     unpadded), or a content hash glued to the cell (in content mode
//!     buck2 glues that one to the cell for the root package:
//!     `art-anon/anon<16 hex>/__t__/o` is `anon/__t__/o`). In content mode buck2
//!     also glues the cell to the package (`<cell><pkg>/…`), which stays glued: the
//!     key is stable, but not the one of the configuration-hash form;
//!   - a BXL function's execution-platform hash after the cell (`cfg`) and its
//!     arguments or content hash right after its `__<name>__`. buck2 writes these
//!     two unpadded: 13 to 16 hex digits are read as a hash, so the 1 in 65 536 with
//!     fewer is kept.
//!
//!   Scratch paths (`buck-out/<v>/tmp/`) are left out. Any other path is kept as sent.
//!   **Known limit:** that includes a remotely run test's
//!   `buck-out/v2/test/execution/<cell>/<16 hex>/<16 hex>/…`: buck2 hashes the whole
//!   target identifier, configuration included, into the first hash (and the test
//!   cases run into the second), so nothing there can be split out. Such an action
//!   keys once per target, configuration and set of test cases (stable across
//!   builds), with `cfg=-`. An action with no outputs has only its program in the
//!   key, so all such actions of one program share a key.
//! - **The program** (`program`): argv\[0\], normalized the same way when it is under
//!   `buck-out`, its file name when it is another absolute path, as sent otherwise
//!   (an argument file's path, after its `@`, the same way); then, while the last word
//!   is a bare program name (no `/` and no `@`: a program looked up on `PATH`, which
//!   may only run the next argument, as `env`, a shell or an interpreter does), the
//!   next argument that is
//!   not a flag or a `NAME=value` assignment, up to three words. A `-c`, or a cluster
//!   of single-letter flags ending in `c` (`-euc`), ends it, kept as the last word, so
//!   a `-c` script never enters the key (a script after `-e`, as Perl or Node take
//!   one, does: `-e` is a shell's flag too, so it is skipped). Any such cluster ends
//!   it, a linker's `-lc` too (`cc -lc a.o` is `cc -lc`). A buck2 genrule is
//!   `env <shell> <cell>/…/genrule.sh`, a prelude Python tool
//!   `<interpreter> <its script>`, a tool built in the repository (a `buck-out` path)
//!   its pattern alone. A bare-name compiler is read the same way: `cc -c a.c` is
//!   `cc -c`, `cc -o a.o a.c` is `cc a.o a.c`, `clang @<argsfile> a.c` is
//!   `clang @<its pattern>` (`clang @<file name>` for an absolute one). The rule names no program: what it unwraps is told by
//!   shape (bare name, path, flag), so no list of runners can go stale.
//!
//! **The target** ([`Target`], issue #796): the owner buck2 wrote a `buck-out` path
//! for, read back from the path: the cell (its first segment), the package (the
//! segments before the name) and the name (`__` stripped, a `/` in it kept), written
//! as a label, `cell//pkg:name` (`cell//:name` in the root package), with the kind of
//! owner its root names (a rule's target, an anonymous target, a BXL function). A
//! package directory may itself be spelled like a name (Jest's `__tests__`), so the
//! name is the one `__<name>__` buck2's layout puts where the name goes, and with
//! none, or more than one that fits, there is no target:
//!   - a target's, configuration form: a `__<name>__` with something under it, not a
//!     dynamic action's `__action__<k>__` right after another name.
//!     `js/__tests__/__t__/out` fits two ways, so names none;
//!   - a target's, content form: a `__<name>__` followed by its content hash or
//!     `output_artifacts`, or by a dynamic action's `__action__<k>__` and then one.
//!     `js/__tests__/__t__/<content>/out` is `cell//js/__tests__:t`;
//!   - a BXL function's, either form: a `__<name>__` followed by its hash (13 to 16
//!     hex digits, or `output_artifacts`). A BXL dynamic action writes its key bare
//!     between the name and the hash, so it names none;
//!   - an anonymous target's, configuration form: a `__<name>__` right after a hex
//!     segment (its attributes hash, which is not part of the package). A dynamic
//!     action's key, which buck2 glues to the name (`__x__<k>__`), stays in it
//!     (`x__<k>`). buck2's older `gen-anon` root is read this way even with no
//!     configuration hash;
//!   - an anonymous target's, content form: only the root package's,
//!     `<cell><content>/__<name>__/…`, is `cell//:name`. Outside it buck2 glues the
//!     cell to the package (`<cell><pkg>/<content>/__<name>__/…`), and where the cell
//!     ends cannot be read, so it names none.
//!
//! An empty cell, or a first segment that is itself a name, names none. An action's
//! target is that of its first output pattern (in pattern order) that names one, else
//! of its first program word that does (a test binary buck2 built, run remotely);
//! `None` when neither names one: a remotely run test's outputs under
//! `buck-out/v2/test/execution/` name none (that root holds no owner's layout), so
//! such a test run by a program outside `buck-out` has no target. It is read, never
//! guessed, and is not part of the key.
//!
//! The key is SHA-256 of a version tag and the program's words and the sorted,
//! deduplicated output patterns, each length-prefixed, cut to 8 bytes: stable across
//! Rust releases and processes (never `DefaultHasher`), and a new tag when the
//! normalizer changes, so new keys never mix with old ones. Nothing the client's
//! `RequestMetadata` says enters it: a client that starts sending a label must not
//! split every key.

use std::fmt;

pub use kbf_invocation::TargetKind;
use sha2::{Digest as _, Sha256};

/// The tag hashed ahead of every shape: a new one for any change to what a shape is.
const VERSION: &[u8] = b"kbf-shape-v1";

/// How many program words the shape keeps at most (`busybox sh <script>`).
const MAX_WORDS: usize = 3;

/// A stable key over an action's [`Shape`]: the first 8 bytes of its SHA-256,
/// shown as 16 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeKey([u8; 8]);

impl ShapeKey {
    /// The key's 8 bytes.
    #[must_use]
    pub const fn bytes(self) -> [u8; 8] {
        self.0
    }
}

impl fmt::Display for ShapeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// The owner an action's own `buck-out` paths name (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Which kind of owner.
    pub kind: TargetKind,
    /// `cell//pkg:name` (`cell//:name` in the cell's root package).
    pub label: String,
}

/// What an action is, independent of configuration and input contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// The key over `program` and `outputs`.
    pub key: ShapeKey,
    /// The program's words (`program`).
    pub program: Vec<String>,
    /// The output patterns, sorted and deduplicated (`pattern`).
    pub outputs: Vec<String>,
    /// The configuration hash of the first output (in pattern order) whose path named
    /// one; `None` for an action whose outputs name none (buck2's content-hash paths).
    /// Not part of the key: one target built in two configurations has one key, and
    /// this tells them apart.
    pub cfg: Option<String>,
    /// The target its paths name (see the module docs). Not part of the key.
    pub target: Option<Target>,
    /// Whether it is a test run: an output path is under `buck-out/<v>/test/execution/`
    /// (buck2's root for a remotely run test). Not part of the key.
    pub test: bool,
}

impl Shape {
    /// The shape of the action whose `Command` has `arguments` and names `outputs`
    /// (every output field, in any order, repeats allowed).
    #[must_use]
    pub fn of(arguments: &[String], outputs: &[String]) -> Self {
        let (program, program_target) = words(arguments);
        let test = outputs.iter().any(|o| is_test_execution(o));
        let mut patterns: Vec<Pattern> = outputs.iter().filter_map(|o| owned_pattern(o)).collect();
        patterns.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let cfg = patterns.iter().find_map(|(_, cfg, _)| cfg.clone());
        let target = patterns
            .iter()
            .find_map(|(_, _, target)| target.clone())
            .or(program_target);
        patterns.dedup_by(|a, b| a.0 == b.0);
        let outputs: Vec<String> = patterns.into_iter().map(|(p, _, _)| p).collect();
        let key = key(&program, &outputs);
        Self {
            key,
            program,
            outputs,
            cfg,
            target,
            test,
        }
    }
}

/// Whether `path` is under `buck-out/<v>/test/execution/`, buck2's root for the outputs
/// of a test run remotely.
fn is_test_execution(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .windows(4)
        .any(|w| w[0] == "buck-out" && w[2] == "test" && w[3] == "execution")
}

/// SHA-256 over the version tag, the program's words and the outputs, each part
/// preceded by its length and each list by its count, cut to 8 bytes.
fn key(program: &[String], outputs: &[String]) -> ShapeKey {
    let mut hash = Sha256::new();
    let mut part = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    part(VERSION);
    for list in [program, outputs] {
        part(&(list.len() as u64).to_le_bytes());
        for item in list {
            part(item.as_bytes());
        }
    }
    let digest = hash.finalize();
    let mut key = [0; 8];
    key.copy_from_slice(&digest[..8]);
    ShapeKey(key)
}

/// The words of the program an action runs (see the module docs; `["-"]` for an
/// empty argv), and the target of the first of them that is a `buck-out` path naming
/// one (an argument file's path, after `@`, is not a program and names none).
fn words(arguments: &[String]) -> (Vec<String>, Option<Target>) {
    let Some((first, rest)) = arguments.split_first() else {
        return (vec!["-".to_owned()], None);
    };
    let (first, mut target) = word(first);
    let mut words = vec![first];
    let mut rest = rest.iter();
    while words.len() < MAX_WORDS && words.last().is_some_and(|w| is_bare_program(w)) {
        match rest.find(|a| is_inline(a) || !skipped(a)) {
            Some(inline) if is_inline(inline) => {
                words.push(inline.clone());
                break;
            }
            Some(arg) => {
                let (next, named) = word(arg);
                words.push(next);
                target = target.or(named);
            }
            None => break,
        }
    }
    (words, target)
}

/// A word that may be a program looked up on `PATH`, which may only run the next
/// argument: no `/`, and not an argument file (`@…`, read by the program before it).
fn is_bare_program(word: &str) -> bool {
    !word.contains('/') && !word.starts_with('@')
}

/// The flag that hands a program an inline script, with any single-letter flags
/// before it (`<shell> -c '...'`, `<shell> -euc '...'`, `<interpreter> -Bc '...'`):
/// the program ends there. A compiler's `-c`, or a flag such as `-static`, ends it
/// too, which only makes the program shorter.
fn is_inline(arg: &str) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|flags| flags.ends_with('c') && flags.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// An argument a program's word skips on its way to the next: a flag, or a
/// `NAME=value` assignment (`env`'s).
fn skipped(arg: &str) -> bool {
    arg.starts_with('-') || arg.contains('=')
}

/// One program word: a `buck-out` path's pattern (`buck-out/tmp` for a scratch path),
/// an absolute path's file name, any other argument as sent; an argument file's path
/// (after its `@`) the same way (`@<pattern>`, `@<file name>`). With it, the target a
/// `buck-out` program path names (never an argument file's).
fn word(arg: &str) -> (String, Option<Target>) {
    let (at, path) = match arg.strip_prefix('@') {
        Some(path) => ("@", path),
        None => ("", arg),
    };
    match buck_out(path) {
        Some(Found::Pattern(pattern, _, target)) => {
            return (format!("{at}{pattern}"), target.filter(|_| at.is_empty()));
        }
        Some(Found::Scratch) => return (format!("{at}buck-out/tmp"), None),
        None => {}
    }
    if path.starts_with('/') {
        return (format!("{at}{}", basename(path)), None);
    }
    (arg.to_owned(), None)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// An output path's pattern, the configuration hash and the target it names.
type Pattern = (String, Option<String>, Option<Target>);

/// The pattern of an output path, the configuration hash it named and the target it
/// names, if any; `None` for a scratch path, which is left out of the shape.
fn owned_pattern(path: &str) -> Option<Pattern> {
    match buck_out(path) {
        Some(Found::Pattern(pattern, cfg, target)) => Some((pattern, cfg, target)),
        Some(Found::Scratch) => None,
        None => Some((path.to_owned(), None, None)),
    }
}

/// A path under a `buck-out` root kbf knows.
enum Found {
    /// Its pattern, the configuration hash it named, and the target it names.
    Pattern(String, Option<String>, Option<Target>),
    /// A scratch path (`buck-out/<v>/tmp/`).
    Scratch,
}

/// Whose outputs a `buck-out/<v>/<root>/` holds, which decides where buck2 puts its
/// hashes (its `make_hashed_path` for each kind of owner).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// A target: `<cell>/<cfg>/<pkg>/__<name>__/…`, or (content mode)
    /// `<cell>/<pkg>/__<name>__/<content>/…`.
    Target,
    /// An anonymous target: `<cell>/<cfg><pkg>/<attrs>/__<name>__/…`, or (content
    /// mode) `<cell><pkg>/<content>/__<name>__/…`. The attributes hash is unpadded
    /// (any hex segment there is taken for it).
    Anon,
    /// A BXL function: `<cell>/<exec>/<bxl path>/__<name>__/<args>/…`, or (content
    /// mode) `<cell>/<bxl path>/__<name>__/<content>/…`. The execution platform and
    /// arguments hashes are unpadded.
    Bxl,
}

/// The roots under `buck-out/<v>/` (`art` today, `gen` before it) and whose outputs
/// each holds.
const ROOTS: &[(&str, Owner)] = &[
    ("art", Owner::Target),
    ("gen", Owner::Target),
    ("art-anon", Owner::Anon),
    ("gen-anon", Owner::Anon),
    ("art-bxl", Owner::Bxl),
    ("gen-bxl", Owner::Bxl),
];

/// What `path` is under a `buck-out` root kbf knows; `None` for any other path.
fn buck_out(path: &str) -> Option<Found> {
    let segments: Vec<&str> = path.split('/').collect();
    let at = segments
        .windows(3)
        .position(|w| w[0] == "buck-out" && (w[2] == "tmp" || ROOTS.iter().any(|r| r.0 == w[2])))?;
    let Some(&(_, owner)) = ROOTS.iter().find(|r| r.0 == segments[at + 2]) else {
        return Some(Found::Scratch);
    };
    let mut rest: Vec<&str> = segments[at + 3..].to_vec();
    let cfg = take_cfg(owner, &mut rest);
    let names = Names::of(&rest);
    let mut kept: Vec<&str> = Vec::with_capacity(rest.len());
    for (i, &segment) in rest.iter().enumerate() {
        // The hash right after a `__<name>__` (only the next segment: a hex directory
        // further down is the action's own).
        let after_name = i > 0 && names.ends[i - 1];
        if after_name && hash_after_name(owner, segment) {
            continue;
        }
        // An anonymous target's attributes hash (any hex segment) or content hash,
        // right before its `__<name>__`: a segment of its own, or (content mode, root
        // package) glued to the cell.
        let before_name = names.starts.get(i + 1).copied().unwrap_or(false);
        if owner == Owner::Anon && before_name {
            if i > 0 && is_hex(segment) {
                continue;
            }
            if let Some(cell) = unglue_content_hash(segment) {
                kept.push(cell);
                continue;
            }
        }
        kept.push(segment);
    }
    // buck2's older `gen-anon` root has no content form: read without a
    // configuration hash, it is read as the configuration form.
    let form = if cfg.is_some() || segments[at + 2] == "gen-anon" {
        Form::Configuration
    } else {
        Form::Content
    };
    let target = target_of(owner, form, &rest, &names);
    Some(Found::Pattern(kept.join("/"), cfg, target))
}

/// Which of buck2's two layouts a path is in: with a configuration hash after the
/// cell, or (its content-based paths) a content hash instead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Configuration,
    Content,
}

/// The target `rest` (a path's segments after its root, the configuration hash taken
/// out, nothing else dropped) names: see the module docs for which `__<name>__` is the
/// name in each layout. `None` when no segment run, or more than one, can be it, with
/// an empty cell, or with a cell segment that is itself a name.
fn target_of(owner: Owner, form: Form, rest: &[&str], names: &Names) -> Option<Target> {
    let mut cell = *rest.first()?;
    if names.starts.first().copied().unwrap_or(false) {
        return None;
    }
    // No name starts at the cell, so every name has a segment before it.
    let spans = names.spans();
    // A dynamic action's `__action__<k>__`, right after its target's name: no name.
    let dynamic = |&(start, end): &Span| {
        start == end && names.ends[start - 1] && rest[start].starts_with("__action__")
    };
    // The hash buck2 puts right after the name, past a dynamic action's key (a target's).
    let hashed = |&(_, end): &Span| {
        let hash_at = |i: usize| rest.get(i).is_some_and(|s| hash_after_name(owner, s));
        hash_at(end + 1)
            || owner == Owner::Target
                && spans.iter().any(|s| s.0 == end + 1 && dynamic(s))
                && hash_at(end + 2)
    };
    let candidates: Vec<Span> = match (owner, form) {
        // Only the root package's form can be read: there the cell is glued to the
        // content hash alone; elsewhere buck2 glues it to the package.
        (Owner::Anon, Form::Content) => {
            cell = unglue_content_hash(cell)?;
            spans.iter().filter(|s| s.0 == 1).copied().collect()
        }
        // The name follows the attributes hash.
        (Owner::Anon, Form::Configuration) => spans
            .iter()
            .filter(|s| s.0 >= 2 && !rest[s.0 - 1].is_empty() && is_hex(rest[s.0 - 1]))
            .copied()
            .collect(),
        // The name has the output under it.
        (Owner::Target, Form::Configuration) => spans
            .iter()
            .filter(|s| s.1 + 1 < rest.len() && !dynamic(s))
            .copied()
            .collect(),
        (Owner::Target, Form::Content) | (Owner::Bxl, _) => spans
            .iter()
            .filter(|s| !dynamic(s) && hashed(s))
            .copied()
            .collect(),
    };
    let [(start, end)] = candidates[..] else {
        return None;
    };
    if cell.is_empty() {
        return None;
    }
    // An anonymous target's attributes hash sits between its package and its name.
    let package_end = match (owner, form) {
        (Owner::Anon, Form::Configuration) => start - 1,
        _ => start,
    };
    let spelled = rest[start..=end].join("/");
    let name = &spelled[2..spelled.len() - 2];
    let kind = match owner {
        Owner::Target => TargetKind::Target,
        Owner::Anon => TargetKind::Anon,
        Owner::Bxl => TargetKind::Bxl,
    };
    Some(Target {
        kind,
        label: format!("{cell}//{}:{name}", rest[1..package_end].join("/")),
    })
}

/// Takes the configuration hash out of `rest` (the segments after the root) and
/// returns it: a segment of its own after the cell, or (anonymous targets) the first
/// 16 characters of the package's first segment.
fn take_cfg(owner: Owner, rest: &mut Vec<&str>) -> Option<String> {
    let after_cell = *rest.get(1)?;
    if is_cfg(after_cell) || owner == Owner::Bxl && is_unpadded_hash(after_cell) {
        rest.remove(1);
        return Some(after_cell.to_owned());
    }
    if owner == Owner::Anon
        && after_cell.len() > 16
        && after_cell.is_char_boundary(16)
        && is_hex(&after_cell[..16])
    {
        rest[1] = &after_cell[16..];
        return Some(after_cell[..16].to_owned());
    }
    None
}

/// Whether `segment`, right after a `__<name>__`, is a hash: a content hash, the
/// `output_artifacts` placeholder, or (BXL) the unpadded arguments hash.
fn hash_after_name(owner: Owner, segment: &str) -> bool {
    segment == "output_artifacts"
        || is_content_hash(segment)
        || owner == Owner::Bxl && is_unpadded_hash(segment)
}

/// The cell of an anonymous target's content-mode root-package segment
/// (`<cell><16 hex>`), if `segment` is one.
fn unglue_content_hash(segment: &str) -> Option<&str> {
    let cut = segment.len().checked_sub(16).filter(|&cut| cut > 0)?;
    let (cell, hash) = segment.split_at_checked(cut)?;
    is_hex(hash).then_some(cell)
}

/// Where the `__<name>__` parts of a path start and end: a target's (`__foo__`), a
/// dynamic action's (`__action__3__`), an anonymous target's (`__foo__0__`). buck2
/// writes the name as it is, and a target name may hold a `/`, so a name runs over
/// segments: from one that starts with `__` to the first later one that ends with
/// `__` (`//pkg:foo/bar` is `__foo/bar__`). Of several segments starting with `__`
/// before an end, the last starts the name (a package directory `__private`
/// above it starts none).
struct Names {
    /// Segment `i` starts a name.
    starts: Vec<bool>,
    /// Segment `i` ends a name.
    ends: Vec<bool>,
}

/// The first and last segment of one `__<name>__`.
type Span = (usize, usize);

impl Names {
    fn of(segments: &[&str]) -> Self {
        let mut names = Self {
            starts: vec![false; segments.len()],
            ends: vec![false; segments.len()],
        };
        let mut open = None;
        for (i, segment) in segments.iter().enumerate() {
            if segment.len() > 4 && segment.starts_with("__") && segment.ends_with("__") {
                names.starts[i] = true;
                names.ends[i] = true;
                open = None;
            } else if segment.len() > 2 && segment.starts_with("__") {
                open = Some(i);
            } else if segment.len() > 2
                && segment.ends_with("__")
                && let Some(start) = open.take()
            {
                names.starts[start] = true;
                names.ends[i] = true;
            }
        }
        names
    }

    /// Each name's first and last segment, in path order (a name's end always
    /// follows its start, with no other start between).
    fn spans(&self) -> Vec<Span> {
        let mut spans = Vec::new();
        let mut first = 0;
        for (i, (&start, &end)) in self.starts.iter().zip(&self.ends).enumerate() {
            if start {
                first = i;
            }
            if end {
                spans.push((first, i));
            }
        }
        spans
    }
}

/// Lowercase hex digits only (true of an empty segment, which a `//` makes).
fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A content hash in a path: 16 lowercase hex digits.
fn is_content_hash(segment: &str) -> bool {
    segment.len() == 16 && is_hex(segment)
}

/// A hash buck2 writes without padding (`{:x}` of a `u64`): 13 to 16 hex digits.
/// Fewer digits happen for 1 hash in 65 536, and are kept; a directory named by 13
/// or more hex digits is not expected where these sit.
fn is_unpadded_hash(segment: &str) -> bool {
    (13..=16).contains(&segment.len()) && is_hex(segment)
}

/// A configuration hash: 16 lowercase hex digits, or two joined by `-` (a target's
/// configuration and its execution platform's).
fn is_cfg(segment: &str) -> bool {
    match segment.split_once('-') {
        Some((target, exec)) => is_content_hash(target) && is_content_hash(exec),
        None => is_content_hash(segment),
    }
}

#[cfg(test)]
#[path = "shape_tests.rs"]
mod tests;

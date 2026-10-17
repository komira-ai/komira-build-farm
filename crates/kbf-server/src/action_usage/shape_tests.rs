//! The shape normalizer on buck2-shaped paths and argv (`buck-out/v2/art/...` as
//! buck2 lays them out today, `gen` as it did), and the key's stability.

use super::*;

const CFG: &str = "7a3c1e9f0b2d4c68";
const CFG2: &str = "0f1e2d3c4b5a6978";
const EXEC: &str = "1122334455667788";
const CONTENT: &str = "0123456789abcdef";
const CONTENT2: &str = "fedcba9876543210";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The pattern of an output path and the configuration hash it named.
fn pattern(path: &str) -> Option<(String, Option<String>)> {
    owned_pattern(path).map(|(pattern, cfg, _)| (pattern, cfg))
}

/// The words of the program an action runs.
fn program(arguments: &[String]) -> Vec<String> {
    words(arguments).0
}

fn pat(path: &str) -> Option<(String, Option<String>)> {
    pattern(path)
}

fn some(p: &str, cfg: Option<&str>) -> Option<(String, Option<String>)> {
    Some((p.to_owned(), cfg.map(str::to_owned)))
}

/// Catches: the configuration hash kept (one target's key split per configuration),
/// the `output_artifacts` placeholder or a content hash kept (a content-hash path keys
/// apart from the configuration-hash path of the same output), or the configuration
/// hash dropped from `cfg` (telemetry could not split configurations).
#[test]
fn configuration_and_content_hash_forms_give_one_pattern() {
    let want = "komira/src/komira_encoding/__komira_encoding__/out/libfoo.a";
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/komira/{CFG}/src/komira_encoding/__komira_encoding__/out/libfoo.a"
        )),
        some(want, Some(CFG))
    );
    assert_eq!(
        pat(
            "buck-out/v2/art/komira/src/komira_encoding/__komira_encoding__/output_artifacts/out/libfoo.a"
        ),
        some(want, None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/komira/src/komira_encoding/__komira_encoding__/{CONTENT}/out/libfoo.a"
        )),
        some(want, None)
    );
}

/// Catches: an execution-platform pair (`<cfg>-<exec>`) left in the pattern, or cut
/// to its first half in `cfg`.
#[test]
fn an_exec_configuration_pair_is_stripped_whole() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/prelude/{CFG}-{EXEC}/rust/tools/__rustc_action__/rustc_action.py"
        )),
        some(
            "prelude/rust/tools/__rustc_action__/rustc_action.py",
            Some(&format!("{CFG}-{EXEC}"))
        )
    );
}

/// Catches: the `gen` root (older buck2, and its unhashed symlinks) keyed apart from
/// `art`, or a path's prefix before `buck-out` (an absolute project root) kept.
#[test]
fn gen_and_art_and_an_absolute_prefix_give_one_pattern() {
    let art = pat(&format!("buck-out/v2/art/cell/{CFG}/pkg/__t__/t.o"));
    let gen_ = pat(&format!("buck-out/v2/gen/cell/{CFG}/pkg/__t__/t.o"));
    let abs = pat(&format!(
        "/srv/repo/buck-out/v2/gen/cell/{CFG}/pkg/__t__/t.o"
    ));
    assert_eq!(art, some("cell/pkg/__t__/t.o", Some(CFG)));
    assert_eq!(gen_, art);
    assert_eq!(abs, art);
}

/// Catches: an anonymous target's configuration hash (glued to its package) or its
/// attributes hash kept, so every attribute change splits the key.
#[test]
fn an_anonymous_target_drops_both_hashes() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/x/3fa9c1/__x__0__/o"
        )),
        some("komira/src/x/__x__0__/o", Some(CFG))
    );
    assert_eq!(
        pat("buck-out/v2/gen-anon/komira/src/x/deadbeef/__x__0__/o"),
        some("komira/src/x/__x__0__/o", None)
    );
    // Outside an anonymous root a hex package segment is a package.
    assert_eq!(
        pat("buck-out/v2/art/komira/src/cafe/__x__/o"),
        some("komira/src/cafe/__x__/o", None)
    );
}

/// Catches: a dynamic action's `__action__<k>__` collapsed (its actions would share a
/// key), or its content hash kept.
#[test]
fn a_dynamic_action_keeps_its_index() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/pkg/__t__/__action__3__/{CONTENT}/link.o"
        )),
        some("cell/pkg/__t__/__action__3__/link.o", None)
    );
    assert_ne!(
        pat("buck-out/v2/art/cell/pkg/__t__/__action__3__/output_artifacts/link.o"),
        pat("buck-out/v2/art/cell/pkg/__t__/__action__4__/output_artifacts/link.o"),
    );
}

/// Catches: a scratch path kept (its whole-identifier hash differs every build, so
/// the key would too).
#[test]
fn scratch_paths_are_left_out() {
    let scratch = format!("buck-out/v2/tmp/komira/{CONTENT}/scratch");
    assert_eq!(pat(&scratch), None);
    let with = Shape::of(&strings(&["cc"]), &strings(&["a.o", &scratch]));
    let without = Shape::of(&strings(&["cc"]), &strings(&["a.o"]));
    assert_eq!(with.key, without.key);
    assert_eq!(with.outputs, ["a.o"]);
}

/// Catches: a path outside buck-out rewritten (Bazel's, or a plain source path), or a
/// hex segment dropped from it.
#[test]
fn other_paths_are_kept_as_sent() {
    for path in [
        "bazel-out/k8-opt/bin/a.o",
        "out/0123456789abcdef/a.o",
        "buck-out/v2/log/x",
    ] {
        assert_eq!(pat(path), some(path, None), "{path}");
    }
}

/// The program names here are stand-ins (`runner` for the shell a genrule runs,
/// `interp` for an interpreter): the rule names no program, so what it does with one
/// name it does with any.
///
/// Catches: a bare program not unwrapped (every genrule, Python tool or inline
/// script would share its runner's key), a path not ending the program (a tool's
/// arguments, which may vary, would enter the key), an inline script that enters the
/// key, a flag or `NAME=value` taken for a program, an absolute program path kept
/// whole (keys split by where a worker installs it), a buck-out program not
/// normalized, or more than three words.
#[test]
fn the_program_unwraps_bare_names_until_a_path() {
    let genrule = format!("buck-out/v2/art/prelude/{CFG}/genrule/__genrule__/sh/genrule.sh");
    assert_eq!(
        program(&strings(&["/usr/bin/env", "runner", "-e", &genrule])),
        ["env", "runner", "prelude/genrule/__genrule__/sh/genrule.sh"]
    );
    assert_eq!(
        program(&strings(&[
            "/usr/bin/env",
            "LANG=C",
            "-i",
            "interp3.12",
            "tool.py"
        ])),
        ["env", "interp3.12", "tool.py"]
    );
    let rustc = format!(
        "buck-out/v2/art/prelude/{CFG}-{EXEC}/rust/tools/__rustc_action__/{CONTENT}/rustc_action.py"
    );
    assert_eq!(
        program(&strings(&["interp", &rustc, "--", "rustc"])),
        [
            "interp",
            "prelude/rust/tools/__rustc_action__/rustc_action.py"
        ]
    );
    let busybox = format!("buck-out/v2/art/komira/tools/busybox/__busybox__/{CONTENT}/busybox");
    assert_eq!(
        program(&strings(&[&busybox, "runner", "-c", "echo one"])),
        ["komira/tools/busybox/__busybox__/busybox"]
    );
    assert_eq!(
        program(&strings(&["runner", "-e", "-c", "echo one"])),
        ["runner", "-c"]
    );
    assert_eq!(
        program(&strings(&["runner", "-e", "-c", "echo one"])),
        program(&strings(&["runner", "-c", "rm -f two"])),
    );
    assert_eq!(
        program(&strings(&["/usr/bin/clang", "-c", "a.c"])),
        ["clang", "-c"]
    );
    assert_eq!(
        program(&strings(&["cc", "-o", "a.o", "a.c"])),
        ["cc", "a.o", "a.c"]
    );
    let zig = format!(
        "buck-out/v2/art/komira/{CFG2}/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"
    );
    assert_eq!(
        program(&strings(&[&zig, "zig", "cc"])),
        ["komira/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"]
    );
    assert_eq!(
        program(&strings(&["tools/kbf-m2-act", "x"])),
        ["tools/kbf-m2-act"]
    );
    assert_eq!(program(&[]), ["-"]);
    assert_eq!(
        program(&strings(&["env", "env", "env", "env", "x"])),
        ["env", "env", "env"]
    );
    assert_eq!(program(&strings(&["runner", "-e"])), ["runner"]);
}

/// Catches: outputs not sorted or not deduplicated (buck2's compatibility mode sends
/// one path in both `output_files` and `output_directories`; the order is the
/// client's), so one action gets two keys.
#[test]
fn outputs_are_a_set() {
    let argv = strings(&["cc"]);
    let a = Shape::of(&argv, &strings(&["b.o", "a.o", "a.o"]));
    let b = Shape::of(&argv, &strings(&["a.o", "b.o"]));
    assert_eq!(a.key, b.key);
    assert_eq!(a.outputs, ["a.o", "b.o"]);
}

/// The key is pinned: a change of hash, of tag, or of framing is a new key for every
/// action ever logged, so it must come with a new version tag.
///
/// Catches: `DefaultHasher` or another hash swapped in, the version tag dropped or
/// changed without this test, a shorter or longer key.
#[test]
fn the_key_is_pinned() {
    let shape = Shape::of(
        &strings(&["/usr/bin/env", "runner", "-e", "genrule.sh"]),
        &strings(&[&format!(
            "buck-out/v2/art/komira/{CFG}/src/foo/__foo__/out/libfoo.a"
        )]),
    );
    assert_eq!(shape.program, ["env", "runner", "genrule.sh"]);
    assert_eq!(shape.outputs, ["komira/src/foo/__foo__/out/libfoo.a"]);
    // Recomputed outside Rust: SHA-256 over u64-LE-length-prefixed parts, the tag
    // first, then each list as its count followed by its items.
    assert_eq!(shape.key.to_string(), "d3a4df9da7659720");
}

/// Catches: parts joined without framing, so `a/b` + `c` and `a` + `b/c` (or a
/// program word and an output) hash alike.
#[test]
fn the_parts_are_framed() {
    let argv = strings(&["cc"]);
    assert_ne!(
        Shape::of(&argv, &strings(&["a/b", "c"])).key,
        Shape::of(&argv, &strings(&["a", "b/c"])).key,
    );
    assert_ne!(
        Shape::of(&strings(&["env", "x"]), &[]).key,
        Shape::of(&strings(&["env"]), &strings(&["x"])).key,
    );
}

/// Catches: a key that moves with the configuration or the content hashes (the whole
/// point: one target's action keeps its key across builds), and two of one target's
/// actions (two compiles) or two targets sharing a key.
#[test]
fn one_action_keeps_its_key_and_others_differ() {
    let argv = |cfg: &str| {
        strings(&[
            &format!(
                "buck-out/v2/art/komira/{cfg}/tools/build/mojo/__zig_cc_launcher__/zig_cc_launcher"
            ),
            "zig",
            "cc",
            "-c",
        ])
    };
    let compile = |cfg: &str, file: &str| {
        Shape::of(
            &argv(cfg),
            &strings(&[&format!(
                "buck-out/v2/art/komira/{cfg}/src/foo/__foo__/objects/{file}.o"
            )]),
        )
    };
    let opt = compile(CFG, "a");
    let dbg = compile(CFG2, "a");
    assert_eq!(opt.key, dbg.key);
    assert_eq!(
        (opt.cfg.as_deref(), dbg.cfg.as_deref()),
        (Some(CFG), Some(CFG2))
    );
    let content = Shape::of(
        &strings(&[
            &format!(
                "buck-out/v2/art/komira/tools/build/mojo/__zig_cc_launcher__/{CONTENT2}/zig_cc_launcher"
            ),
            "zig",
        ]),
        &strings(&["buck-out/v2/art/komira/src/foo/__foo__/output_artifacts/objects/a.o"]),
    );
    assert_eq!(content.key, opt.key);
    assert_eq!(content.cfg, None);
    assert_ne!(compile(CFG, "b").key, opt.key);
    let other_target = Shape::of(
        &argv(CFG),
        &strings(&[&format!(
            "buck-out/v2/art/komira/{CFG}/src/bar/__bar__/objects/a.o"
        )]),
    );
    assert_ne!(other_target.key, opt.key);
}

/// The edges of the path reader. Catches: a scratch program not named as one (its
/// hashed path would split the key), a `buck-out` path that ends at the cell
/// refused or mangled, a glued anonymous configuration hash taken where the package
/// does not start with 16 hex digits (or is cut inside a character), the cell itself
/// taken for an attributes hash, or an execution pair accepted with a malformed half.
#[test]
fn edge_paths() {
    let scratch = format!("buck-out/v2/tmp/komira/{CONTENT}/run.sh");
    assert_eq!(program(&strings(&[&scratch, "x"])), ["buck-out/tmp"]);
    assert_eq!(pat("buck-out/v2/art/komira"), some("komira", None));
    let glued = format!("buck-out/v2/art-anon/komira/{}é/__x__/o", &CONTENT[..15]);
    assert_eq!(
        pat(&glued),
        some(&format!("komira/{}é/__x__/o", &CONTENT[..15]), None)
    );
    assert_eq!(
        pat("buck-out/v2/art-anon/komira/srcsrcsrcsrcsrcsrc/__x__/o"),
        some("komira/srcsrcsrcsrcsrcsrc/__x__/o", None)
    );
    assert_eq!(
        pat("buck-out/v2/art-anon/abc/__x__/o"),
        some("abc/__x__/o", None)
    );
    assert_eq!(
        pat(&format!("buck-out/v2/art/cell/{CFG}-zz/pkg/__t__/o")),
        some(&format!("cell/{CFG}-zz/pkg/__t__/o"), None)
    );
    assert_eq!(
        pat("buck-out/v2/art/cell/my-pkg/__t__/o"),
        some("cell/my-pkg/__t__/o", None)
    );
}

/// BXL outputs, at the paths buck2 writes for komira's `test_weld.bxl` `check`
/// (configuration and content form). Catches: the `art-bxl` or `gen-bxl` root
/// unknown (the path kept whole, hashes and all, so the key splits every build), the
/// execution-platform hash kept or not taken as `cfg`, the arguments hash after the
/// function's `__<name>__` kept, or an unpadded (shorter) hash in either place kept.
#[test]
fn bxl_paths_drop_their_hashes() {
    let want = "komira/tools/build/lint/test_weld.bxl/__check__/test_weld/validation.json";
    assert_eq!(
        pat(
            "buck-out/v2/art-bxl/komira/453d11473805230f/tools/build/lint/test_weld.bxl/__check__/ecd72b18569a9587/test_weld/validation.json"
        ),
        some(want, Some("453d11473805230f"))
    );
    assert_eq!(
        pat(
            "buck-out/v2/art-bxl/komira/tools/build/lint/test_weld.bxl/__check__/083ffece723d3c8b/test_weld/validation.json"
        ),
        some(want, None)
    );
    // Unpadded: 13 and 15 digits.
    assert_eq!(
        pat(
            "buck-out/v2/gen-bxl/komira/53d11473805230f/tools/build/lint/test_weld.bxl/__check__/d72b18569a958/test_weld/validation.json"
        ),
        some(want, Some("53d11473805230f"))
    );
    // A short hex directory after the cell is a directory (fewer than 13 digits).
    assert_eq!(
        pat("buck-out/v2/art-bxl/komira/cafe/x.bxl/__f__/083ffece723d3c8b/o"),
        some("komira/cafe/x.bxl/__f__/o", None)
    );
}

/// The root-package anonymous target buck2's prelude makes for the C++ toolchain, in
/// content form, where buck2 glues the content hash to the cell; and a package
/// directory named by hex digits, which is not the hash (the hash always sits
/// between it and the name). Catches: the glued hash kept (the key moves with every
/// content change), the cell cut with it, a cut that leaves an empty cell or falls
/// inside a character, or a hex package directory dropped.
#[test]
fn an_anonymous_root_package_target_drops_its_glued_hash() {
    assert_eq!(
        pat(
            "buck-out/v2/art-anon/anon990e66921b1edf1c/___compiler_type_flags_anon_rule__/compiler_type_args"
        ),
        some(
            "anon/___compiler_type_flags_anon_rule__/compiler_type_args",
            None
        )
    );
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/komira/{CFG}3fa9c1/__x__/o")),
        some("komira/__x__/o", Some(CFG))
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/beef/3fa9c1/__x__/o"
        )),
        some("komira/src/beef/__x__/o", Some(CFG))
    );
    // A cell glued to a hash too short to be a content hash stays as it is.
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/{}/__x__/o", &CONTENT[..15])),
        some(&format!("{}/__x__/o", &CONTENT[..15]), None)
    );
    // Sixteen hex digits and no cell before them: not cut to an empty segment.
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/{CONTENT}/__x__/o")),
        some(&format!("{CONTENT}/__x__/o"), None)
    );
    // Sixteen bytes from the end inside a character: no hash there.
    assert_eq!(
        pat(&format!("buck-out/v2/art-anon/é{}/__x__/o", &CONTENT[..15])),
        some(&format!("é{}/__x__/o", &CONTENT[..15]), None)
    );
}

/// Catches: a hash or `output_artifacts` dropped anywhere but in the one segment
/// right after a `__<name>__` (an action's own hex-named directory, or a directory
/// named `output_artifacts`, would vanish from its pattern and two outputs merge),
/// or BXL's unpadded hash rule applied to a target's outputs.
#[test]
fn only_the_segment_after_the_name_is_a_hash() {
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/{CFG}/pkg/__t__/d/{CONTENT2}/output_artifacts/a.o"
        )),
        some(
            &format!("cell/pkg/__t__/d/{CONTENT2}/output_artifacts/a.o"),
            Some(CFG)
        )
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/pkg/__t__/{CONTENT}/{CONTENT2}/a.o"
        )),
        some(&format!("cell/pkg/__t__/{CONTENT2}/a.o"), None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/{CFG}/pkg/{CONTENT2}/__t__/a.o"
        )),
        some(&format!("cell/pkg/{CONTENT2}/__t__/a.o"), Some(CFG))
    );
    // Only BXL writes an unpadded hash after the name: a target's 13-digit directory
    // stays.
    assert_eq!(
        pat("buck-out/v2/art/cell/pkg/__t__/0123456789abc/a.o"),
        some("cell/pkg/__t__/0123456789abc/a.o", None)
    );
}

/// Catches: `cfg` taken from another output than the first in pattern order (or in
/// the order the client sent them), so one action's `cfg` changes with the order of
/// its outputs or names the execution platform's output rather than the target's.
#[test]
fn cfg_is_the_first_patterns() {
    let outputs = [
        format!("buck-out/v2/art/cell/{CFG}/pkg/__t__/b.o"),
        format!("buck-out/v2/art/cell/{CFG2}/pkg/__t__/a.o"),
        "c.o".to_owned(),
    ];
    let argv = strings(&["cc"]);
    let sent = Shape::of(&argv, &outputs);
    let reversed: Vec<String> = outputs.iter().rev().cloned().collect();
    assert_eq!(sent.cfg.as_deref(), Some(CFG2));
    assert_eq!(Shape::of(&argv, &reversed).cfg.as_deref(), Some(CFG2));
}

/// Catches: an inline script that enters the key when its flag is a cluster
/// (`-euc`, `-Bc`), so the script body (and any configuration hash in it) is in the
/// key; or a flag that only contains a `c` (`-C`, `--c`, `-ce` before a script file)
/// taken as one.
#[test]
fn a_flag_cluster_ending_in_c_is_inline() {
    assert_eq!(
        program(&strings(&["runner", "-euc", "echo a > x"])),
        ["runner", "-euc"]
    );
    assert_eq!(
        program(&strings(&["/usr/bin/env", "interp", "-Bc", "print(1)"])),
        ["env", "interp", "-Bc"]
    );
    assert_eq!(
        program(&strings(&["runner", "-C", "dir", "x"])),
        ["runner", "dir", "x"]
    );
    assert_eq!(
        program(&strings(&["runner", "--c", "dir"])),
        ["runner", "dir"]
    );
    assert_eq!(
        program(&strings(&["runner", "-ce", "script"])),
        ["runner", "script"]
    );
    assert_eq!(program(&strings(&["runner", "-"])), ["runner"]);
    // Documented: any single-letter cluster ending in `c` ends it, a linker's too.
    assert_eq!(program(&strings(&["cc", "-lc", "a.o"])), ["cc", "-lc"]);
}

/// Catches: an argument file's path kept as sent (its configuration hash splits
/// the key per configuration, a content hash every build; an absolute one's
/// directory, a temporary one, splits it per run), the `@` dropped, or the program
/// read on past an argument file (its source file entering the key).
#[test]
fn an_argument_file_is_normalized_after_its_at() {
    let argsfile =
        |cfg: &str| format!("@buck-out/v2/art/komira/{cfg}/src/foo/__foo__/foo.argsfile");
    let a = program(&strings(&["clang++", &argsfile(CFG)]));
    assert_eq!(a, ["clang++", "@komira/src/foo/__foo__/foo.argsfile"]);
    assert_eq!(program(&strings(&["clang++", &argsfile(CFG2)])), a);
    assert_eq!(
        program(&strings(&[
            "clang++",
            "@buck-out/v2/art/komira/src/foo/__foo__/output_artifacts/foo.argsfile"
        ])),
        a
    );
    assert_eq!(
        program(&strings(&[
            "clang++",
            &format!("@buck-out/v2/tmp/k/{CONTENT}/a")
        ])),
        ["clang++", "@buck-out/tmp"]
    );
    assert_eq!(
        program(&strings(&["clang++", "@args.txt", "a.c"])),
        ["clang++", "@args.txt",]
    );
    let absolute = |dir: &str| program(&strings(&["clang++", &format!("@/{dir}/args"), "a.c"]));
    assert_eq!(absolute("work/beef"), ["clang++", "@args"]);
    assert_eq!(absolute("work/cafe"), absolute("work/beef"));
}

/// A target whose name holds a `/` (`//pkg:foo/bar`), which buck2 writes as it is:
/// `__foo/bar__` spans two segments. Catches: a name read only within one segment,
/// so the content hash or `output_artifacts` after it is kept and the key moves every
/// build; a name start taken from a package directory `__private` above it, so an
/// anonymous target's attributes hash is not found before its name.
#[test]
fn a_name_with_a_slash_spans_segments() {
    let want = "cell/pkg/__foo/bar__/out.o";
    for path in [
        format!("buck-out/v2/art/cell/{CFG}/pkg/__foo/bar__/out.o"),
        format!("buck-out/v2/art/cell/pkg/__foo/bar__/{CONTENT}/out.o"),
        "buck-out/v2/art/cell/pkg/__foo/bar__/output_artifacts/out.o".to_owned(),
    ] {
        assert_eq!(pat(&path).map(|p| p.0).as_deref(), Some(want), "{path}");
    }
    let configured = Shape::of(
        &strings(&["cc"]),
        &strings(&[&format!("buck-out/v2/art/cell/{CFG}/pkg/__foo/bar__/out.o")]),
    );
    let content = Shape::of(
        &strings(&["cc"]),
        &strings(&[&format!(
            "buck-out/v2/art/cell/pkg/__foo/bar__/{CONTENT2}/out.o"
        )]),
    );
    assert_eq!(configured.key, content.key);
    assert_eq!(
        pat(&format!("buck-out/v2/art/cell/pkg/__a/b/c__/{CONTENT}/o")),
        some("cell/pkg/__a/b/c__/o", None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art/cell/pkg/__foo/bar__/__action__3__/{CONTENT}/o"
        )),
        some("cell/pkg/__foo/bar__/__action__3__/o", None)
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/x/3fa9c1/__x/y__/o"
        )),
        some("komira/src/x/__x/y__/o", Some(CFG))
    );
    assert_eq!(
        pat(&format!(
            "buck-out/v2/art-anon/komira/{CFG}src/__private/3fa9c1/__x/y__/o"
        )),
        some("komira/src/__private/__x/y__/o", Some(CFG))
    );
    // A segment ending in `__` with no start before it is no name.
    assert_eq!(
        pat(&format!("buck-out/v2/art/cell/pkg/bar__/{CONTENT}/o")),
        some(&format!("cell/pkg/bar__/{CONTENT}/o"), None)
    );
}

/// Catches: a BXL hash read from fewer than 13 hex digits (a 12-digit directory after
/// a function's name dropped), and an anonymous target reading an unpadded hash after
/// its name (buck2 writes none there: its hashes sit before the name).
#[test]
fn shorter_hex_directories_after_a_name_are_kept() {
    assert_eq!(
        pat("buck-out/v2/art-bxl/komira/x.bxl/__f__/083ffece723d/o"),
        some("komira/x.bxl/__f__/083ffece723d/o", None)
    );
    for dir in [&CONTENT[..13], &CONTENT[..15]] {
        assert_eq!(
            pat(&format!(
                "buck-out/v2/art-anon/komira/{CFG}src/x/3fa9c1/__x__0__/{dir}/o"
            )),
            some(&format!("komira/src/x/__x__0__/{dir}/o"), Some(CFG)),
            "{dir}"
        );
    }
}

/// KNOWN LIMIT, pinned: a test buck2 runs remotely writes under
/// `buck-out/v2/test/execution/<cell>/<16 hex>/<16 hex>/…`, where the first hash is
/// of the whole target identifier, configuration included, and the second of the
/// test cases run. No segment holds the configuration on its own, so the path is
/// kept as sent: one key per target, configuration and set of test cases, stable
/// across builds, and `cfg=-`. Catches a change to that (a fix must move this test).
#[test]
fn known_limit_a_test_execution_path_keys_per_configuration() {
    let path = |cfg_hash: &str| {
        format!("buck-out/v2/test/execution/komira/{cfg_hash}/{CONTENT2}/default/out")
    };
    assert_eq!(pat(&path(CFG)), some(&path(CFG), None));
    let shape = |cfg_hash: &str| Shape::of(&strings(&["runner"]), &strings(&[&path(cfg_hash)]));
    assert_eq!(shape(CFG).key, shape(CFG).key);
    assert_ne!(shape(CFG).key, shape(CFG2).key);
    assert_eq!(shape(CFG).cfg, None);
}

/// A test buck2 runs remotely writes under `buck-out/<v>/test/execution/`, which the
/// shape says (`Shape::test`, and so the run record's `test`): from any output, under
/// any version directory and any prefix. Catches: the flag never set, set from the
/// program rather than an output, set by a path that only names `test` or `execution`
/// somewhere else (a target called `test`), or set with no `buck-out` in the path.
#[test]
fn an_output_under_the_test_execution_root_marks_the_shape_a_test() {
    let test = |outputs: &[&str]| Shape::of(&strings(&["runner"]), &strings(outputs)).test;
    let execution = format!("buck-out/v2/test/execution/komira/{CFG}/{CONTENT2}/default/out");
    assert!(test(&[&execution]));
    assert!(test(&["a.o", &execution]), "any one output");
    assert!(test(&[&format!(
        "/work/buck-out/v3/test/execution/komira/{CFG}/{CONTENT2}/out"
    )]));
    assert!(!test(&[]));
    assert!(!test(&["a.o"]));
    assert!(!test(&[&format!(
        "buck-out/v2/art/komira/{CFG}/src/test/__t__/execution/out"
    )]));
    assert!(!test(&["out/test/execution/x"]), "no buck-out");
    assert!(!test(&["buck-out/v2/test/other/x"]));
    assert!(
        !Shape::of(&strings(&[&execution]), &strings(&["a.o"])).test,
        "a program path is not an output"
    );
}

fn target_of_paths(argv: &[&str], outputs: &[&str]) -> Option<(TargetKind, String)> {
    Shape::of(&strings(argv), &strings(outputs))
        .target
        .map(|t| (t.kind, t.label))
}

fn label(kind: TargetKind, label: &str) -> Option<(TargetKind, String)> {
    Some((kind, label.to_owned()))
}

/// Issue #796: the target a compile's own output paths name. Catches: the label
/// built from the wrong segments (the configuration hash or the content hash or
/// `output_artifacts` left in it, the cell or the package dropped), the content-hash
/// form giving another label than the configuration form, a name with a `/` cut at
/// it, or the root package written other than `cell//:name`.
#[test]
fn a_compile_names_its_target_in_every_form() {
    let want = label(
        TargetKind::Target,
        "komira//src/komira_encoding:komira_encoding",
    );
    for path in [
        format!(
            "buck-out/v2/art/komira/{CFG}/src/komira_encoding/__komira_encoding__/out/libfoo.a"
        ),
        format!("buck-out/v2/art/komira/{CFG}-{EXEC}/src/komira_encoding/__komira_encoding__/o"),
        "buck-out/v2/art/komira/src/komira_encoding/__komira_encoding__/output_artifacts/o"
            .to_owned(),
        format!("buck-out/v2/gen/komira/src/komira_encoding/__komira_encoding__/{CONTENT}/o"),
        format!("/srv/repo/buck-out/v2/art/komira/{CFG}/src/komira_encoding/__komira_encoding__/o"),
    ] {
        assert_eq!(target_of_paths(&["cc"], &[&path]), want, "{path}");
    }
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art/cell/{CFG}/pkg/__foo/bar__/o")]
        ),
        label(TargetKind::Target, "cell//pkg:foo/bar"),
        "a name keeps its slash"
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art/cell/{CFG}/__root__/o")]
        ),
        label(TargetKind::Target, "cell//:root"),
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!(
                "buck-out/v2/art/cell/pkg/__t__/__action__3__/{CONTENT}/o"
            )]
        ),
        label(TargetKind::Target, "cell//pkg:t"),
        "a dynamic action is its target's"
    );
}

/// Issue #796: an action whose outputs name two owners takes the target of the first
/// pattern in pattern order (sorted), as `cfg` does. Catches: the target taken from
/// another pattern (the last one, or the first the client sent), so one action's
/// target changes with the order of its outputs.
#[test]
fn target_is_the_first_patterns() {
    let z = format!("buck-out/v2/art/cell/{CFG}/pkg/__z__/o");
    let a = format!("buck-out/v2/art/cell/{CFG}/pkg/__a__/o");
    let want = label(TargetKind::Target, "cell//pkg:a");
    assert_eq!(target_of_paths(&["cc"], &[&z, &a]), want, "sent last");
    assert_eq!(target_of_paths(&["cc"], &[&a, &z]), want, "sent first");
}

/// Issue #796: anonymous and BXL owners give their kind. Catches: an anonymous
/// target's glued configuration or attributes hash left in its label, its kind lost
/// (read as a rule's target), the root-package form's glued content hash kept in the
/// cell, or a BXL function's hashes left in its label.
#[test]
fn anonymous_and_bxl_owners_name_their_kind() {
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!(
                "buck-out/v2/art-anon/komira/{CFG}src/x/3fa9c1/__x__/o"
            )]
        ),
        label(TargetKind::Anon, "komira//src/x:x")
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &["buck-out/v2/gen-anon/komira/src/x/deadbeef/__x__/o"]
        ),
        label(TargetKind::Anon, "komira//src/x:x")
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &["buck-out/v2/art-anon/anon990e66921b1edf1c/___compiler_type_flags_anon_rule__/o"]
        ),
        label(TargetKind::Anon, "anon//:_compiler_type_flags_anon_rule")
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[
                "buck-out/v2/art-bxl/komira/453d11473805230f/tools/build/lint/test_weld.bxl/__check__/ecd72b18569a9587/v.json"
            ]
        ),
        label(
            TargetKind::Bxl,
            "komira//tools/build/lint/test_weld.bxl:check"
        )
    );
    assert_eq!(
        [TargetKind::Target, TargetKind::Anon, TargetKind::Bxl].map(TargetKind::name),
        ["target", "anon", "bxl"]
    );
}

/// Issue #796: a test buck2 built and runs remotely names its target through its
/// program, its outputs naming none; one run by a program outside `buck-out` names
/// none, and is `None`, not a guess. Catches: the program word not read for a target,
/// an argument file's owner taken for the program's, the first output's target not
/// preferred over the program's, or a `test/execution` path read as a target.
#[test]
fn a_test_names_its_target_through_its_program_or_names_none() {
    let execution = format!("buck-out/v2/test/execution/komira/{CFG}/{CONTENT2}/default/out");
    let binary = format!("buck-out/v2/art/komira/{CFG}/src/t/__t_test__/t_test");
    assert_eq!(
        target_of_paths(&[&binary, "--list"], &[&execution]),
        label(TargetKind::Target, "komira//src/t:t_test")
    );
    assert_eq!(
        target_of_paths(&["/usr/bin/env", "FOO=1", &binary], &[&execution]),
        label(TargetKind::Target, "komira//src/t:t_test"),
        "a wrapped program word"
    );
    assert_eq!(
        target_of_paths(&["test-runner", "suite.cfg"], &[&execution]),
        None
    );
    assert_eq!(
        target_of_paths(
            &[
                "clang",
                &format!("@buck-out/v2/art/komira/{CFG}/src/a/__a__/args")
            ],
            &["a.o"]
        ),
        None,
        "an argument file is not a program"
    );
    assert_eq!(
        target_of_paths(
            &["buck-out/v2/art/prelude/rust/tools/__rustc_action__/output_artifacts/r.py"],
            &[&format!(
                "buck-out/v2/art/komira/{CFG}/src/foo/__foo__/libfoo.rlib"
            )]
        ),
        label(TargetKind::Target, "komira//src/foo:foo"),
        "the output's target, not the tool's"
    );
    assert_eq!(
        target_of_paths(&["cc"], &["buck-out/v2/art/__x__/o"]),
        None,
        "no cell"
    );
    for path in [
        "buck-out/v2/art//pkg/__x__/o".to_owned(),
        format!("buck-out/v2/art//pkg/__x__/{CONTENT}/o"),
        format!("buck-out/v2/art//{CFG}/pkg/__x__/o"),
    ] {
        assert_eq!(
            target_of_paths(&["cc"], &[&path]),
            None,
            "empty cell: {path}"
        );
    }
    assert_eq!(
        target_of_paths(&["cc"], &["buck-out/v2/art/cell/pkg/o"]),
        None,
        "no name"
    );
    assert_eq!(
        target_of_paths(&["cc"], &["buck-out/v2/art/cell/pkg/__open/o"]),
        None,
        "a name never closed"
    );
}

/// Issue #796, review: a package directory whose name starts and ends with `__`
/// (Jest's `__tests__`, `__mocks__`) looks like a target name. In the content-hash form
/// the real name is the one the content hash (or `output_artifacts`) follows, past a
/// dynamic action's `__action__<k>__`; in the configuration form two such segments
/// before the output cannot be told apart, so the target is `None`, not a guess.
/// Catches: the package directory read as the name (`cell//js:tests`), the content
/// hash not used to find the name, a dynamic action's `__action__<k>__` counted as a
/// second candidate (its target lost), or the output's own `__x__` file name taken.
#[test]
fn a_package_directory_that_looks_like_a_name_is_not_one() {
    let want = label(TargetKind::Target, "cell//js/__tests__:t");
    for path in [
        format!("buck-out/v2/art/cell/js/__tests__/__t__/{CONTENT}/out"),
        "buck-out/v2/art/cell/js/__tests__/__t__/output_artifacts/out".to_owned(),
        format!("buck-out/v2/art/cell/js/__tests__/__t__/__action__3__/{CONTENT}/out"),
        format!("buck-out/v2/gen/cell/js/__tests__/__mocks__/__t__/{CONTENT}/out"),
    ] {
        let want = if path.contains("__mocks__") {
            label(TargetKind::Target, "cell//js/__tests__/__mocks__:t")
        } else {
            want.clone()
        };
        assert_eq!(target_of_paths(&["cc"], &[&path]), want, "{path}");
    }
    for path in [
        format!("buck-out/v2/art/cell/{CFG}/js/__tests__/__t__/out"),
        format!("buck-out/v2/art/cell/{CFG}/js/__tests__/__foo/bar__/out"),
        format!("buck-out/v2/art/cell/{CFG}/js/__t__/__pycache__/out"),
    ] {
        assert_eq!(target_of_paths(&["cc"], &[&path]), None, "{path}");
    }
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!(
                "buck-out/v2/art/cell/{CFG}/js/__t__/__action__3__/out"
            )]
        ),
        label(TargetKind::Target, "cell//js:t"),
        "a dynamic action's key is not a second name"
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art/cell/{CFG}/js/__t__/__init__")]
        ),
        label(TargetKind::Target, "cell//js:t"),
        "the output's own file name is no candidate"
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art/cell/{CFG}/js/__init__")]
        ),
        None,
        "a name with nothing under it"
    );
    assert_eq!(
        target_of_paths(&["cc"], &["buck-out/v2/art/cell/js/__tests__/__t__/out"]),
        None,
        "the content form with no hash after any name"
    );
}

/// Issue #796, review: an anonymous target's content-mode path outside the root
/// package glues the cell to the package (`<cell><pkg>/<content>/__<name>__/…`), so
/// where the cell ends cannot be read. Catches: the glued segment taken for the cell
/// (`cellsrc//x:x`). The root-package form, whose cell is glued to the content hash
/// alone, and the configuration form keep their labels; a package directory that looks
/// like a name is told apart by the attributes hash before the real one.
#[test]
fn an_anonymous_content_mode_target_outside_the_root_names_none() {
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art-anon/cellsrc/x/{CONTENT}/__x__/o")]
        ),
        None
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art-anon/cell{CONTENT}/__x__/o")]
        ),
        label(TargetKind::Anon, "cell//:x")
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!(
                "buck-out/v2/art-anon/cell/{CFG}src/__tests__/3fa9c1/__x__/o"
            )]
        ),
        label(TargetKind::Anon, "cell//src/__tests__:x")
    );
    assert_eq!(
        target_of_paths(
            &["cc"],
            &[&format!("buck-out/v2/art-anon/cell/{CFG}3fa9c1/__x__/o")]
        ),
        label(TargetKind::Anon, "cell//:x"),
        "the root package in the configuration form"
    );
    for path in [
        format!("buck-out/v2/art-anon/cell/{CFG}/__x__/o"),
        format!("buck-out/v2/art-anon/cell/{CFG}src//__x__/o"),
        format!("buck-out/v2/art-anon/cell/{CFG}src/x/__x__/o"),
    ] {
        assert_eq!(
            target_of_paths(&["cc"], &[&path]),
            None,
            "no attributes hash before the name: {path}"
        );
    }
}

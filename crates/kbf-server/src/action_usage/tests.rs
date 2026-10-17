//! The line's fields, the outcome words, what a report measured, and the bound on the
//! keys an `action shape:` line was written for.

use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::CgroupUsage;

use super::*;

fn described(argv: &[&str], outputs: &[&str]) -> Described {
    Described {
        arguments: argv.iter().map(|s| (*s).to_owned()).collect(),
        outputs: outputs.iter().map(|s| (*s).to_owned()).collect(),
        label: ActionLabel::default(),
    }
}

fn report(code: Code, exit_code: i32, usage: Option<&ResourceUsage>) -> worker::Result {
    let metadata = usage.map(|u| ExecutedActionMetadata {
        auxiliary_metadata: vec![u.to_any()],
        ..ExecutedActionMetadata::default()
    });
    worker::Result {
        status: Some(rpc::Status {
            code: code as i32,
            ..rpc::Status::default()
        }),
        action_result: Some(ActionResult {
            exit_code,
            execution_metadata: metadata,
            ..ActionResult::default()
        }),
        ..worker::Result::default()
    }
}

fn usage() -> ResourceUsage {
    ResourceUsage {
        wall_micros: 1_000,
        cgroup: Some(CgroupUsage {
            memory_peak_bytes: Some(4096),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    }
}

/// Catches: a failed run's synthetic exit code (0) logged as if it exited 0, an OK
/// run's exit code lost (also for a result with no status, which the farm reads as
/// OK), and a line made for a report that measured nothing.
#[test]
fn only_an_ok_run_has_an_exit_code_and_only_a_measured_one_a_line() {
    let ok = Measured::of(&report(Code::Ok, 7, Some(&usage()))).expect("measured");
    assert_eq!(ok.exit_code, Some(7));
    let oom = Measured::of(&report(Code::ResourceExhausted, 0, Some(&usage()))).expect("measured");
    assert_eq!(oom.exit_code, None);
    assert_eq!(oom.usage, usage());
    let mut no_status = report(Code::Ok, 5, Some(&usage()));
    no_status.status = None;
    let no_status = Measured::of(&no_status).expect("measured");
    assert_eq!(no_status.exit_code, Some(5));
    assert_eq!(Measured::of(&report(Code::Ok, 0, None)), None);
    let bare = worker::Result::default();
    assert_eq!(Measured::of(&bare), None);
}

/// Catches: a field renamed or moved, a missing field, a label not quoted, `-` lost
/// for an absent one; and (issue #796) the client's `target_id` written where the
/// derived target goes, or the derived target missing from the line.
#[test]
fn the_line_names_the_hint_and_the_derived_target_apart() {
    let shapes = Shapes::default();
    let mut subject = shapes.subject(described(
        &["cc"],
        &["buck-out/v2/art/cell/7a3c1e9f0b2d4c68/pkg/__t__/t.o"],
    ));
    subject.label.target = Some("//pkg:t".to_owned());
    let measured = Measured::of(&report(Code::Ok, 0, Some(&usage()))).expect("measured");
    let lease = LeaseId::new(3, 9);
    let node = WorkerId::new("node-a");
    let line = line(
        &subject,
        &measured,
        Outcome::Failed(Failure::OutOfMemory),
        lease,
        &node,
    );
    let cgroup = usage().cgroup.expect("cgroup").log_fields(1_000);
    assert_eq!(
        line,
        format!(
            "exit_code=0 mnemonic=- target_hint=\"//pkg:t\" wall_us=1000 {cgroup} key={} \
             cfg=7a3c1e9f0b2d4c68 target=\"cell//pkg:t\" target_kind=target outcome=oom \
             lease={lease} node=node-a",
            subject.key
        )
    );
}

/// Catches: two failures given one word (the per-outcome telemetry could not tell
/// them apart), or a word changed by accident.
#[test]
fn every_outcome_has_its_own_word() {
    let action_result = kbf_types::Digest {
        function: kbf_types::DigestFunction::Sha256,
        hash: [0; 32],
        size_bytes: 0,
    };
    let words = [
        outcome(Outcome::Completed { action_result }),
        outcome(Outcome::Failed(Failure::Timeout)),
        outcome(Outcome::Failed(Failure::Infra)),
        outcome(Outcome::Failed(Failure::Invalid)),
        outcome(Outcome::Failed(Failure::OutOfMemory)),
        outcome(Outcome::Failed(Failure::NodeMemoryPressure)),
        outcome(Outcome::Failed(Failure::MissingInput)),
        outcome(Outcome::Failed(Failure::Lost(LostWhy::DaemonRestarted))),
        outcome(Outcome::Failed(Failure::Lost(LostWhy::Fenced))),
    ];
    assert_eq!(
        words,
        [
            "ok",
            "timeout",
            "infra",
            "invalid",
            "oom",
            "node-pressure",
            "missing-input",
            "lost-restart",
            "lost-fenced"
        ]
    );
}

/// Catches: a key described on every submission (a line per Execute), never again
/// after the set is full (keys first seen later would never be described), or a set
/// that grows without bound.
#[test]
fn a_key_is_described_once_and_the_set_is_bounded() {
    let shapes = Shapes::default();
    let key = Shape::of(&["cc".to_owned()], &[]).key;
    assert!(shapes.first_sight(key));
    assert!(!shapes.first_sight(key));
    let filler = |n: usize| Shape::of(&[format!("cc{n}")], &[]).key;
    for n in 1..SEEN_KEYS {
        shapes.first_sight(filler(n));
    }
    assert_eq!(shapes.seen.lock().expect("seen").len(), SEEN_KEYS);
    assert!(
        !shapes.first_sight(key),
        "held keys are still known when full"
    );
    assert!(
        shapes.first_sight(filler(SEEN_KEYS)),
        "a new key past the bound"
    );
    assert_eq!(shapes.seen.lock().expect("seen").len(), 1);
}

/// Catches: the first-seen line without the key or the program, all patterns of a
/// many-output action written out (a line per link of thousands of outputs), or the
/// cut not marked.
#[test]
fn a_shape_line_shows_eight_patterns_and_marks_the_rest() {
    let outputs: Vec<String> = (0..9).map(|n| format!("o{n}")).collect();
    let shape = Shape::of(&["/usr/bin/env".to_owned(), "runner".to_owned()], &outputs);
    assert_eq!(
        shape_line(&shape),
        format!(
            "key={} program=\"env runner\" outputs=9 patterns=\"o0 o1 o2 o3 o4 o5 o6 o7…\"",
            shape.key
        )
    );
    let few = Shape::of(&["cc".to_owned()], &outputs[..2]);
    assert_eq!(
        shape_line(&few),
        format!(
            "key={} program=\"cc\" outputs=2 patterns=\"o0 o1\"",
            few.key
        )
    );
}

/// Catches: the test flag dropped between the shape and the record the row keeps (a
/// run record would never say it is a test), or set on every record.
#[test]
fn the_record_says_whether_the_action_is_a_test() {
    let recorded = |outputs: &[&str]| {
        let (subject, _) = Subject::of(described(&["runner"], outputs));
        (subject.test, subject.recorded().test)
    };
    let execution = "buck-out/v2/test/execution/cell/7a3c1e9f0b2d4c68/fedcba9876543210/default/o";
    assert_eq!(recorded(&[execution]), (true, true));
    assert_eq!(recorded(&["a.o"]), (false, false));
}

/// Catches: the record given another key than the line's (bytes reordered), the
/// configuration hash or the target dropped, a kind mapped to another, or the
/// client's `target_id` recorded as the derived target.
#[test]
fn the_record_keeps_the_subject_s_key_cfg_and_target() {
    use kbf_invocation::TargetKind as Kind;
    let recorded = |outputs: &[&str]| {
        let mut described = described(&["cc"], outputs);
        described.label.target = Some("//client:says".to_owned());
        let (subject, _) = Subject::of(described);
        (subject.key, subject.recorded())
    };
    let (key, shape) = recorded(&["buck-out/v2/art/cell/7a3c1e9f0b2d4c68/pkg/__t__/t.o"]);
    assert_eq!(shape.key_text(), key.to_string());
    assert_eq!(shape.cfg.as_deref(), Some("7a3c1e9f0b2d4c68"));
    let target = shape.target.expect("a target");
    assert_eq!(
        (target.kind, target.label.value.as_str()),
        (Kind::Target, "cell//pkg:t")
    );
    for (path, kind) in [
        (
            "buck-out/v2/art-anon/cell/7a3c1e9f0b2d4c68pkg/3fa9c1/__t__/o",
            Kind::Anon,
        ),
        (
            "buck-out/v2/art-bxl/cell/x.bxl/__f__/0123456789abcdef/o",
            Kind::Bxl,
        ),
    ] {
        let (_, shape) = recorded(&[path]);
        assert_eq!(shape.target.map(|t| t.kind), Some(kind), "{path}");
    }
    let (_, shape) = recorded(&["out/a.o"]);
    assert_eq!(shape.target, None, "the client's target_id is no target");
}

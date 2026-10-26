//! Plans recorded before file edits existed must stay readable and keep their
//! digests; file edits must round-trip and unknown step kinds must be refused.

use cli2_core::*;

#[test]
fn a_process_only_plan_recorded_before_file_edits_is_unchanged() {
    let text = include_str!("fixtures/process_plan_v1.json");
    let plan: Plan = serde_json::from_str(text).expect("old plan parses");
    assert!(matches!(plan.steps[0], Step::Process(_)));
    assert!(
        plan.is_intact(),
        "the recorded digest must still verify: process steps serialize as before"
    );
    // And it re-serializes to the same JSON value.
    let back: serde_json::Value = serde_json::to_value(&plan).unwrap();
    let orig: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(back, orig);
}

fn edit_step() -> FileEditStep {
    FileEditStep {
        kind: FileEditKind::FileEdit,
        index: 0,
        capability_id: FILE_EDIT_CAPABILITY_ID.into(),
        root: "/p".into(),
        path: "a.txt".into(),
        expected_sha256: Some(content_hash(b"old\n")),
        hunks: vec![Hunk {
            start_line: 1,
            old_text: "old\n".into(),
            new_text: "new\n".into(),
        }],
        result_sha256: content_hash(b"new\n"),
        result_bytes: 4,
    }
}

#[test]
fn a_file_edit_step_round_trips_and_is_tagged() {
    let step = Step::FileEdit(edit_step());
    let v = serde_json::to_value(&step).unwrap();
    assert_eq!(v["kind"], "file_edit");
    let back: Step = serde_json::from_value(v).unwrap();
    assert_eq!(back, step);
}

#[test]
fn the_digest_covers_the_exact_patch_and_target() {
    let mut plan: Plan =
        serde_json::from_str(include_str!("fixtures/process_plan_v1.json")).unwrap();
    plan.steps = vec![Step::FileEdit(edit_step())];
    plan.seal();
    let base = plan.digest.clone();
    assert!(plan.is_intact());

    let mutate = |f: &dyn Fn(&mut FileEditStep)| {
        let mut p = plan.clone();
        let Step::FileEdit(e) = &mut p.steps[0] else {
            unreachable!()
        };
        f(e);
        p.compute_digest()
    };
    assert_ne!(base, mutate(&|e| e.hunks[0].new_text = "evil\n".into()));
    assert_ne!(base, mutate(&|e| e.path = "b.txt".into()));
    assert_ne!(base, mutate(&|e| e.root = "/q".into()));
    assert_ne!(base, mutate(&|e| e.expected_sha256 = None));
    assert_ne!(base, mutate(&|e| e.result_sha256 = content_hash(b"x")));
    assert_ne!(base, mutate(&|e| e.hunks[0].start_line = 2));
}

#[test]
fn unknown_step_kinds_are_refused() {
    let mut v = serde_json::to_value(Step::FileEdit(edit_step())).unwrap();
    v["kind"] = "delete_everything".into();
    assert!(serde_json::from_value::<Step>(v).is_err());
}

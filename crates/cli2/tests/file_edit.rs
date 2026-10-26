//! Governed file modification: a patch against a known version is planned,
//! reviewed as an exact diff, approved by digest and applied atomically; stale
//! patches cannot overwrite newer edits; an interrupted write is never
//! repeated; a write is a change, never proof that anything works.
//!
//! Deterministic: no model, no network. Strategies are scripted.

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cli2::controller::proposer::*;
use cli2::controller::Controller;
use cli2::runtime::Runtime;
use cli2_core::{
    content_hash, ApprovalLevel, Check, FileEditStatus, Hunk, Status, Step, Termination,
    VerificationStatus,
};
use cli2_protocol::devtask::*;
use cli2_protocol::{
    Approvability, ErrorCode, ExecutionSnapshot, FileReadParams, Lifecycle, PlanEditParams,
};
use support::*;

const MAKEFILE: &str = r#".PHONY: test check-validation
test: ## Run the project tests
	@sh tests/run.sh
check-validation: ## Check that the validation exists
	@if test -f src/validation.js; then echo "validation present"; else echo "validation missing" >&2; exit 1; fi
"#;

const TESTS_RUN: &str = r##"case "$(cat src/validation.js 2>/dev/null)" in
  *1000000*) echo "ok - rejects amounts over the limit"; echo "# tests 2, pass 2, fail 0" ;;
  *) echo "FAIL: upper bound missing in src/validation.js" >&2; echo "# tests 2, pass 1, fail 1"; exit 1 ;;
esac
"##;

const BUGGY: &str = "function validate(a) {\n  return a > 0;\n}\n";
const FIXED: &str = "function validate(a) {\n  return a > 0 && a < 1000000;\n}\n";
const HALF_FIXED: &str = "function validate(a) {\n  return a > 0 && a < 1e6 + 1;\n}\n";

fn fix_hunk() -> Hunk {
    Hunk {
        start_line: 2,
        old_text: "  return a > 0;\n".into(),
        new_text: "  return a > 0 && a < 1000000;\n".into(),
    }
}

fn config(env: &Env) -> cli2::Config {
    let mut c = cli2::Config::from_env().unwrap();
    c.home = env.home.clone();
    c.path_var = env.path.clone();
    c.policy = Default::default();
    c.resolver = Some(Default::default());
    c
}

struct Rig {
    env: Env,
    project: PathBuf,
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
}

impl Rig {
    fn new() -> Rig {
        let env = Env::new();
        let project = env.work.join("proj");
        std::fs::create_dir_all(project.join("tests")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
        std::fs::write(project.join("tests/run.sh"), TESTS_RUN).unwrap();
        std::fs::write(project.join("src/validation.js"), BUGGY).unwrap();
        std::fs::write(project.join("README.md"), "# invoices\n").unwrap();
        let runtime = Arc::new(Runtime::new(config(&env)));
        let controller = Controller::new(runtime.clone());
        for (name, f) in [
            ("fix-it", fix_it as fn(&ProposalContext<'_>) -> Proposal),
            ("incomplete-fix", incomplete_fix),
            ("raced", raced),
            ("edit-after-verify", edit_after_verify),
        ] {
            controller.register_strategy(
                name,
                Arc::new(FnStrategy(move |_: &CreateTaskParams| {
                    Ok(Box::new(Scripted(f)) as Box<dyn StepProposer>)
                })),
            );
        }
        Rig {
            env,
            project: project.canonicalize().unwrap(),
            runtime,
            controller,
        }
    }

    fn proj(&self) -> String {
        self.project.display().to_string()
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.project.join(p)).unwrap()
    }

    fn plan_edit(&self, path: &str, expected: Option<&str>, hunks: Vec<Hunk>) -> PlanEditParams {
        PlanEditParams {
            project: self.proj(),
            path: path.into(),
            expected_sha256: expected.map(String::from),
            hunks,
            intent: Some("fix the upper bound".into()),
            idempotency_key: None,
            constraints: Default::default(),
        }
    }

    fn wait_execution(&self, id: &str) -> ExecutionSnapshot {
        let t = Instant::now();
        loop {
            let s = self.runtime.execution(id).unwrap();
            if s.lifecycle != Lifecycle::Running && s.lifecycle != Lifecycle::Cancelling {
                return s;
            }
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "execution did not finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Plan, approve by digest and run to completion.
    fn apply(&self, p: PlanEditParams) -> ExecutionSnapshot {
        let plan = self.runtime.plan_edit(p).unwrap().outcome.plan.unwrap();
        let a = self.runtime.approve(&plan.plan_id, &plan.digest).unwrap();
        let s = self
            .runtime
            .start(&plan.plan_id, &a.approval.approval_id)
            .unwrap();
        self.wait_execution(&s.execution_id)
    }

    fn criteria(&self) -> Vec<Criterion> {
        vec![Criterion {
            id: "tests_pass".into(),
            description: "the project's tests pass".into(),
            checks: vec![Check::StdoutContains {
                text: "fail 0".into(),
            }],
            after_changes: true,
            verify_intent: Some("make test".into()),
        }]
    }

    fn task(&self, strategy: &str) -> CreateTaskParams {
        CreateTaskParams {
            project: self.proj(),
            goal: "Add the upper bound to the amount validation; the tests must pass.".into(),
            criteria: self.criteria(),
            strategy: Some(strategy.into()),
            ..Default::default()
        }
    }

    fn wait_for(&self, id: &str, what: &str, pred: impl Fn(&TaskView) -> bool) -> TaskView {
        let t = Instant::now();
        loop {
            let v = self.controller.get(id).unwrap();
            if pred(&v) {
                return v;
            }
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}; state {:?}",
                v.state
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn approve_current(&self, v: &TaskView) {
        let s = v
            .steps
            .iter()
            .find(|s| Some(&s.step_id) == v.current_step.as_ref())
            .unwrap();
        self.controller
            .approve(ApproveStepParams {
                task_id: v.task_id.clone(),
                step_id: s.step_id.clone(),
                plan_digest: s.plan_digest.clone().unwrap(),
            })
            .unwrap();
    }

    /// Approve every operation by its exact digest until the task stops.
    fn drive(&self, id: &str) -> TaskView {
        let t = Instant::now();
        loop {
            let v = self.controller.get(id).unwrap();
            if v.state.is_terminal() || v.state == TaskState::Blocked {
                return v;
            }
            if v.state == TaskState::AwaitingApproval {
                self.approve_current(&v);
            } else {
                std::thread::sleep(Duration::from_millis(25));
            }
            assert!(t.elapsed() < Duration::from_secs(60), "task did not settle");
        }
    }

    fn events(&self, id: &str) -> Vec<DevTaskEvent> {
        self.controller
            .events(&TaskEventsParams {
                task_id: id.into(),
                after_seq: 0,
                wait_ms: 0,
                limit: Some(5000),
            })
            .unwrap()
            .events
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.controller.shutdown(Duration::from_secs(2));
        let _ = &self.env;
    }
}

// --------------------------------------------------------------- strategies

struct Scripted(fn(&ProposalContext<'_>) -> Proposal);

impl StepProposer for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }
    fn propose(&mut self, ctx: &ProposalContext<'_>) -> Result<Proposal, String> {
        Ok((self.0)(ctx))
    }
}

fn verify() -> Proposal {
    Proposal::Step(ProposedStep {
        kind: StepKind::Verify,
        objective: "run the tests".into(),
        intent: "make test".into(),
        selection: None,
        bindings: BTreeMap::new(),
        satisfies: vec!["tests_pass".into()],
        note: None,
    })
}

fn edit(path: &str, expected: &str, hunk: Hunk, why: &str) -> Proposal {
    Proposal::Edit(ProposedEdit {
        objective: why.into(),
        edit: FileEditSpec {
            path: path.into(),
            expected_sha256: Some(expected.into()),
            hunks: vec![hunk],
        },
        note: Some("scripted".into()),
    })
}

fn edits_so_far(ctx: &ProposalContext<'_>) -> usize {
    ctx.history.iter().filter(|s| s.file_edit.is_some()).count()
}

/// Read the file, make the correct edit, then run the tests.
fn fix_it(ctx: &ProposalContext<'_>) -> Proposal {
    if edits_so_far(ctx) == 0 {
        let f = ctx.files.read("src/validation.js").unwrap();
        return edit(&f.path, &f.sha256, fix_hunk(), "add the upper bound");
    }
    if ctx.history.iter().any(|s| s.kind == StepKind::Verify) {
        return Proposal::Done { note: None };
    }
    verify()
}

/// First attempt is wrong (the bound is not what the tests look for). After the
/// tests fail, read the file *again*, look at the failure, and fix it.
fn incomplete_fix(ctx: &ProposalContext<'_>) -> Proposal {
    let n = edits_so_far(ctx);
    let verified = ctx
        .history
        .iter()
        .filter(|s| s.kind == StepKind::Verify)
        .count();
    if n == 0 {
        let f = ctx.files.read("src/validation.js").unwrap();
        return edit(
            &f.path,
            &f.sha256,
            Hunk {
                start_line: 2,
                old_text: "  return a > 0;\n".into(),
                new_text: "  return a > 0 && a < 1e6 + 1;\n".into(),
            },
            "add an upper bound",
        );
    }
    if verified == 0 {
        return verify();
    }
    let last_verify = ctx
        .history
        .iter()
        .rposition(|s| s.kind == StepKind::Verify)
        .unwrap();
    let failed = ctx.history[last_verify]
        .outcome
        .as_ref()
        .is_some_and(|o| o.task_status != Some(Status::Succeeded));
    if !failed {
        return Proposal::Done { note: None };
    }
    if n == 1 {
        // Learn from the observed failure, and from the file as it is now.
        assert!(
            ctx.history[last_verify]
                .stderr_tail
                .contains("upper bound missing"),
            "the failure evidence reaches the proposer: {:?}",
            ctx.history[last_verify].stderr_tail
        );
        let f = ctx.files.read("src/validation.js").unwrap();
        assert_eq!(
            f.content, HALF_FIXED,
            "the proposer sees the file as the first edit left it"
        );
        return edit(
            &f.path,
            &f.sha256,
            Hunk {
                start_line: 2,
                old_text: "  return a > 0 && a < 1e6 + 1;\n".into(),
                new_text: "  return a > 0 && a < 1000000;\n".into(),
            },
            "use the limit the tests check",
        );
    }
    if ctx
        .history
        .iter()
        .rposition(|s| s.file_edit.is_some())
        .unwrap()
        > last_verify
    {
        return verify();
    }
    Proposal::GiveUp {
        reason: "still failing".into(),
    }
}

/// The first call reads the file, then somebody else edits it before the
/// proposal is planned. The proposer must start over from the new version.
fn raced(ctx: &ProposalContext<'_>) -> Proposal {
    let planned_edits = edits_so_far(ctx);
    let was_stale = ctx
        .history
        .iter()
        .any(|s| s.file_edit.is_some() && s.status == StepStatus::NoPlan);
    if planned_edits == 0 {
        let f = ctx.files.read("src/validation.js").unwrap();
        // A concurrent change by a person, after the proposer read the file.
        let p = std::path::Path::new(ctx.project).join("src/validation.js");
        std::fs::write(&p, format!("// reviewed by alice\n{}", f.content)).unwrap();
        return edit(&f.path, &f.sha256, fix_hunk(), "add the upper bound");
    }
    if was_stale && planned_edits == 1 {
        let f = ctx.files.read("src/validation.js").unwrap();
        assert!(f.content.starts_with("// reviewed by alice\n"));
        return edit(
            &f.path,
            &f.sha256,
            Hunk {
                start_line: 3,
                old_text: "  return a > 0;\n".into(),
                new_text: "  return a > 0 && a < 1000000;\n".into(),
            },
            "add the upper bound to the version that exists now",
        );
    }
    if ctx.history.iter().any(|s| s.kind == StepKind::Verify) {
        return Proposal::Done { note: None };
    }
    verify()
}

/// Edit, verify the tests, then edit something else: the test run no longer
/// covers the final state, so it must be redone even though it once passed.
fn edit_after_verify(ctx: &ProposalContext<'_>) -> Proposal {
    let n = edits_so_far(ctx);
    let tests_runs = ctx
        .history
        .iter()
        .filter(|s| s.intent == "make test")
        .count();
    let outcome = |id: &str| ctx.statuses.iter().find(|s| s.id == id).map(|s| s.outcome);
    match n {
        0 => {
            let f = ctx.files.read("src/validation.js").unwrap();
            edit(&f.path, &f.sha256, fix_hunk(), "add the upper bound")
        }
        1 if tests_runs == 0 => verify(),
        1 => {
            let f = ctx.files.read("README.md").unwrap();
            edit(
                &f.path,
                &f.sha256,
                Hunk {
                    start_line: 2,
                    old_text: String::new(),
                    new_text: "Amounts are limited to 1000000.\n".into(),
                },
                "document the limit",
            )
        }
        _ if outcome("validation_present") != Some(CriterionOutcome::Passed) => {
            Proposal::Step(ProposedStep {
                kind: StepKind::Verify,
                objective: "check the validation exists".into(),
                intent: "make check-validation".into(),
                selection: None,
                bindings: BTreeMap::new(),
                satisfies: vec!["validation_present".into()],
                note: None,
            })
        }
        _ if outcome("tests_pass") != Some(CriterionOutcome::Passed) => verify(),
        _ => Proposal::Done { note: None },
    }
}

// ===================================================== the step and its plan

#[test]
fn a_patch_is_planned_as_an_exact_reviewable_diff_and_never_auto_approved() {
    let rig = Rig::new();
    let read = rig
        .runtime
        .read_file(&FileReadParams {
            project: rig.proj(),
            path: "./src//validation.js".into(),
        })
        .unwrap();
    assert_eq!(read.path, "src/validation.js");
    assert_eq!(read.content, BUGGY);
    assert_eq!(read.sha256, content_hash(BUGGY.as_bytes()));

    // Even a request that says "modify files is fine" does not waive review of a patch.
    let mut p = rig.plan_edit("src/validation.js", Some(&read.sha256), vec![fix_hunk()]);
    p.constraints.modify_files = Some(true);
    let planned = rig.runtime.plan_edit(p).unwrap();
    let plan = planned.outcome.plan.clone().unwrap();
    assert_eq!(planned.outcome.status, Status::Planned);
    assert!(
        plan.approval.level >= ApprovalLevel::Confirm,
        "{:?}",
        plan.approval
    );
    assert_ne!(planned.approvability, Approvability::NotRequired);

    // The plan *is* the patch: target, version, hunks and the resulting hash.
    let Step::FileEdit(f) = &plan.steps[0] else {
        panic!("expected a file edit step");
    };
    assert_eq!(f.path, "src/validation.js");
    assert_eq!(f.expected_sha256.as_deref(), Some(read.sha256.as_str()));
    assert_eq!(f.hunks, vec![fix_hunk()]);
    assert_eq!(f.result_sha256, content_hash(FIXED.as_bytes()));
    assert_eq!(f.root, rig.proj());
    assert!(f
        .unified_diff()
        .contains("-  return a > 0;\n+  return a > 0 && a < 1000000;\n"));
    assert!(plan
        .effects
        .classes
        .contains(&cli2_core::EffectClass::ModifiesFiles));

    // Planning wrote nothing.
    assert_eq!(rig.read("src/validation.js"), BUGGY);
}

#[test]
fn nothing_is_written_without_an_approval_bound_to_this_exact_plan() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let plan = rig
        .runtime
        .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]))
        .unwrap()
        .outcome
        .plan
        .unwrap();
    let other = rig
        .runtime
        .plan_edit(rig.plan_edit(
            "src/validation.js",
            Some(&h),
            vec![Hunk {
                start_line: 2,
                old_text: "  return a > 0;\n".into(),
                new_text: "  return true;\n".into(),
            }],
        ))
        .unwrap()
        .outcome
        .plan
        .unwrap();
    assert_ne!(plan.digest, other.digest, "the digest covers the patch");

    // A shortened or foreign digest approves nothing.
    assert!(rig
        .runtime
        .approve(&plan.plan_id, &plan.digest[..19])
        .is_err());
    assert!(rig.runtime.approve(&plan.plan_id, &other.digest).is_err());
    // An approval of the other patch cannot start this one.
    let a_other = rig.runtime.approve(&other.plan_id, &other.digest).unwrap();
    assert_eq!(
        rig.runtime
            .start(&plan.plan_id, &a_other.approval.approval_id)
            .unwrap_err()
            .code,
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(rig.read("src/validation.js"), BUGGY);

    // Rejected plans never run.
    rig.runtime
        .reject(&other.plan_id, Some("no".into()))
        .unwrap();
    assert_eq!(
        rig.runtime
            .start(&other.plan_id, &a_other.approval.approval_id)
            .unwrap_err()
            .code,
        ErrorCode::PlanRejected
    );
    assert_eq!(rig.read("src/validation.js"), BUGGY);
}

#[test]
fn an_approved_patch_is_applied_and_the_evidence_shows_what_actually_changed() {
    let rig = Rig::new();
    std::fs::set_permissions(
        rig.project.join("src/validation.js"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let h = content_hash(BUGGY.as_bytes());
    let done = rig.apply(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]));
    assert_eq!(done.lifecycle, Lifecycle::Finished);
    assert_eq!(done.task_status, Some(Status::Succeeded));
    assert_eq!(rig.read("src/validation.js"), FIXED);
    assert_eq!(
        std::fs::metadata(rig.project.join("src/validation.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    let step = &done.steps[0];
    assert_eq!(step.termination, Termination::Applied);
    let o = step.file_edit.as_ref().unwrap();
    assert_eq!(o.status, FileEditStatus::Applied);
    assert_eq!(o.expected_sha256.as_deref(), Some(h.as_str()));
    assert_eq!(o.before_sha256.as_deref(), Some(h.as_str()));
    assert_eq!(
        o.after_sha256.as_deref(),
        Some(content_hash(FIXED.as_bytes()).as_str())
    );
    assert_eq!(o.planned_sha256, content_hash(FIXED.as_bytes()));

    // The approved diff is retained as evidence of the change.
    let diff = rig
        .runtime
        .evidence(step.stdout_ref.as_ref().unwrap(), None)
        .unwrap();
    assert!(diff.content.contains("+  return a > 0 && a < 1000000;"));

    // Verification says the file reads back as planned and no more than that.
    let r = done.result.unwrap();
    assert_eq!(r.verification.status, VerificationStatus::Passed);
    assert!(r
        .verification
        .checks
        .iter()
        .all(|c| matches!(c.check, Check::FileSha256 { .. })));
    assert!(
        r.message.contains("not that it achieves anything"),
        "{}",
        r.message
    );

    // The same patch cannot be applied twice: the file is no longer that version.
    let again =
        rig.runtime
            .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]));
    assert_eq!(again.unwrap_err().code, ErrorCode::StalePatch);
}

#[test]
fn a_patch_that_does_not_match_the_file_is_refused_and_a_new_file_must_not_exist() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let bad = Hunk {
        start_line: 2,
        old_text: "  return a >= 0;\n".into(),
        new_text: "  return false;\n".into(),
    };
    let e = rig
        .runtime
        .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![bad]))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e}");
    // Creating an existing file is a conflict, not an overwrite.
    let create = |path: &str| {
        rig.runtime.plan_edit(rig.plan_edit(
            path,
            None,
            vec![Hunk {
                start_line: 1,
                old_text: String::new(),
                new_text: "hello\n".into(),
            }],
        ))
    };
    assert_eq!(create("README.md").unwrap_err().code, ErrorCode::Conflict);
    let plan = create("src/new.js").unwrap().outcome.plan.unwrap();
    assert!(
        !rig.project.join("src/new.js").exists(),
        "planning creates nothing"
    );
    let a = rig.runtime.approve(&plan.plan_id, &plan.digest).unwrap();
    let s = rig
        .runtime
        .start(&plan.plan_id, &a.approval.approval_id)
        .unwrap();
    assert_eq!(
        rig.wait_execution(&s.execution_id).task_status,
        Some(Status::Succeeded)
    );
    assert_eq!(rig.read("src/new.js"), "hello\n");
}

// ============================================================== staleness

#[test]
fn a_patch_prepared_against_an_older_version_cannot_overwrite_a_newer_edit() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    // Somebody edits the file after the patch was prepared but before it is planned.
    std::fs::write(
        rig.project.join("src/validation.js"),
        format!("// alice\n{BUGGY}"),
    )
    .unwrap();
    let e = rig
        .runtime
        .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::StalePatch, "{e}");
    assert_eq!(rig.read("src/validation.js"), format!("// alice\n{BUGGY}"));
}

#[test]
fn a_file_changed_between_approval_and_execution_is_left_alone() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let plan = rig
        .runtime
        .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]))
        .unwrap()
        .outcome
        .plan
        .unwrap();
    let a = rig.runtime.approve(&plan.plan_id, &plan.digest).unwrap();
    // The approval was for a patch to *that* version. Now the file changes.
    let theirs = format!("{BUGGY}// concurrent change\n");
    std::fs::write(rig.project.join("src/validation.js"), &theirs).unwrap();
    assert!(
        !rig.runtime
            .plan_get(&plan.plan_id)
            .unwrap()
            .stale_reasons
            .is_empty(),
        "the plan is reported as stale"
    );
    let s = rig
        .runtime
        .start(&plan.plan_id, &a.approval.approval_id)
        .unwrap();
    let done = rig.wait_execution(&s.execution_id);
    assert_ne!(
        done.task_status,
        Some(Status::Succeeded),
        "{:?}",
        done.result
    );
    assert_eq!(
        rig.read("src/validation.js"),
        theirs,
        "nothing was overwritten"
    );
    assert!(done.steps.is_empty() || done.steps[0].termination != Termination::Applied);
}

// ===================================================== path and file safety

#[test]
fn edits_cannot_leave_the_project_or_touch_anything_but_regular_files() {
    let rig = Rig::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "one\n").unwrap();
    symlink(outside.path().join("secret"), rig.project.join("link")).unwrap();
    symlink(outside.path(), rig.project.join("dir")).unwrap();
    let fifo = std::ffi::CString::new(rig.project.join("fifo").to_str().unwrap()).unwrap();
    // SAFETY: valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let h = content_hash(b"one\n");
    let hunk = || Hunk {
        start_line: 1,
        old_text: "one\n".into(),
        new_text: "two\n".into(),
    };
    for path in [
        "../escape",
        "/etc/passwd",
        "src/../../escape",
        "link",
        "dir/secret",
        "fifo",
        "src",
        "nope/nested/file",
    ] {
        let r = rig
            .runtime
            .plan_edit(rig.plan_edit(path, Some(&h), vec![hunk()]));
        assert!(r.is_err(), "{path} must be refused");
        let r = rig.runtime.read_file(&FileReadParams {
            project: rig.proj(),
            path: path.into(),
        });
        assert!(r.is_err(), "{path} must not be readable either");
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret")).unwrap(),
        "one\n"
    );
    assert!(!rig.project.parent().unwrap().join("escape").exists());
}

#[test]
fn a_request_that_forbids_modification_rejects_an_edit() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let mut p = rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]);
    p.constraints.modify_files = Some(false);
    let r = rig.runtime.plan_edit(p).unwrap();
    assert_eq!(r.outcome.status, Status::Rejected);
    assert_eq!(rig.read("src/validation.js"), BUGGY);
}

#[test]
fn planning_the_same_edit_with_the_same_key_returns_the_same_plan_and_a_different_patch_conflicts()
{
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let mut p = rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]);
    p.idempotency_key = Some("k1".into());
    let a = rig.runtime.plan_edit(p.clone()).unwrap();
    let b = rig.runtime.plan_edit(p.clone()).unwrap();
    assert!(b.deduplicated);
    assert_eq!(a.outcome.plan_id, b.outcome.plan_id);
    p.hunks[0].new_text = "  return true;\n".into();
    assert_eq!(
        rig.runtime.plan_edit(p).unwrap_err().code,
        ErrorCode::Conflict
    );
}

// ================================================================= recovery

/// What a crash around the write leaves in the journal: an execution that was
/// started by a runtime that no longer exists, and no result. This is exactly
/// the state `Runtime::start` writes before it runs anything.
fn simulate_crash(rig: &Rig, plan_id: &str, approval_id: &str, request_id: &str) -> String {
    let j = cli2_evidence::Journal::open(&rig.env.home.join("evidence")).unwrap();
    let id = "run_crashed".to_string();
    j.event(
        request_id,
        "execution_started",
        &serde_json::json!({
            "execution_id": id,
            "plan_id": plan_id,
            "request_id": request_id,
            "approval_id": approval_id,
            "instance_id": "a-runtime-that-died",
            "started_at": cli2_core::now_rfc3339(),
        }),
    )
    .unwrap();
    id
}

#[test]
fn an_interrupted_write_is_reported_as_unknown_and_never_applied_again() {
    for committed in [false, true] {
        let rig = Rig::new();
        let h = content_hash(BUGGY.as_bytes());
        let plan = rig
            .runtime
            .plan_edit(rig.plan_edit("src/validation.js", Some(&h), vec![fix_hunk()]))
            .unwrap()
            .outcome;
        let p = plan.plan.unwrap();
        let a = rig.runtime.approve(&p.plan_id, &p.digest).unwrap().approval;
        let id = simulate_crash(&rig, &p.plan_id, &a.approval_id, &p.request_id);
        if committed {
            // The crash happened just after the write landed.
            std::fs::write(rig.project.join("src/validation.js"), FIXED).unwrap();
        }

        // A new runtime reconciles the journal.
        let rt2 = Runtime::new(config(&rig.env));
        assert_eq!(rt2.reconcile().unwrap(), vec![id.clone()]);
        let snap = rt2.execution(&id).unwrap();
        assert_eq!(snap.lifecycle, Lifecycle::InterruptedUnknown);
        assert_ne!(
            snap.task_status,
            Some(Status::Succeeded),
            "no success is claimed"
        );
        let msg = snap
            .result
            .as_ref()
            .map(|r| r.message.clone())
            .unwrap_or_default();
        assert!(msg.contains("unknown"), "{msg}");
        if committed {
            assert!(msg.contains("may have been committed"), "{msg}");
            assert!(msg.contains("will not be applied again"), "{msg}");
        } else {
            assert!(msg.contains("probably not committed"), "{msg}");
        }

        // Starting again with the same approval returns the interrupted
        // execution; it does not apply the patch.
        let again = rt2.start(&p.plan_id, &a.approval_id).unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.execution_id, id);
        std::thread::sleep(Duration::from_millis(100));
        let expected = if committed { FIXED } else { BUGGY };
        assert_eq!(
            std::fs::read_to_string(rig.project.join("src/validation.js")).unwrap(),
            expected,
            "committed={committed}: the patch was not retried"
        );
    }
}

// ============================================================ the controller

#[test]
fn a_task_reads_edits_through_approval_and_completes_only_when_the_tests_pass() {
    let rig = Rig::new();
    let t = rig.controller.create(rig.task("fix-it")).unwrap();
    rig.controller.start(&t.task_id).unwrap();

    // The first thing needing a decision is the edit, and it is never auto-approved.
    let v = rig.wait_for(&t.task_id, "the edit awaiting approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    let s = &v.steps[0];
    assert_eq!(s.kind, StepKind::Change);
    assert!(s.satisfies.is_empty());
    assert_eq!(s.status, StepStatus::AwaitingApproval);
    assert!(s.approval_required.unwrap() >= ApprovalLevel::Confirm);
    let spec = s.file_edit.clone().unwrap();
    assert_eq!(spec.path, "src/validation.js");
    assert_eq!(spec.hunks, vec![fix_hunk()]);
    // A reviewer can see the exact change in the plan the approval will bind to.
    let plan = rig
        .runtime
        .plan_get(s.plan_id.as_ref().unwrap())
        .unwrap()
        .plan;
    assert_eq!(plan.digest, *s.plan_digest.as_ref().unwrap());
    assert!(plan.steps[0].as_file_edit().is_some());
    assert_eq!(
        rig.read("src/validation.js"),
        BUGGY,
        "not written before approval"
    );
    // Not complete: nothing has been verified yet.
    assert_ne!(v.state, TaskState::Completed);

    let done = rig.drive(&t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);
    assert_eq!(rig.read("src/validation.js"), FIXED);
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Verified);
    assert_eq!(r.verified.len(), 1, "{r:?}");
    assert_eq!(
        r.changes.len(),
        1,
        "the edit is reported as a change: {r:?}"
    );
    // What the proposer read is on the record.
    let ev = rig.events(&t.task_id);
    assert!(ev.iter().any(|e| matches!(
        &e.data,
        DevTaskEventData::FileRead { path, sha256, .. }
            if path == "src/validation.js" && *sha256 == content_hash(BUGGY.as_bytes())
    )));
    // The criterion was established by the test run, which came after the edit.
    let order: Vec<_> = done
        .steps
        .iter()
        .map(|s| (s.kind, s.file_edit.is_some()))
        .collect();
    assert_eq!(order, [(StepKind::Change, true), (StepKind::Verify, false)]);
}

#[test]
fn a_failing_check_leads_to_a_follow_up_edit_made_from_the_observed_failure() {
    let rig = Rig::new();
    let t = rig.controller.create(rig.task("incomplete-fix")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);
    assert_eq!(rig.read("src/validation.js"), FIXED);

    let kinds: Vec<_> = done
        .steps
        .iter()
        .map(|s| (s.kind, s.file_edit.is_some()))
        .collect();
    assert_eq!(
        kinds,
        [
            (StepKind::Change, true),
            (StepKind::Verify, false),
            (StepKind::Change, true),
            (StepKind::Verify, false),
        ]
    );
    // The first verification really failed; the second edit was prepared
    // against the file as the first edit left it, not a replay of the first.
    assert_eq!(
        done.steps[1].outcome.as_ref().unwrap().task_status,
        Some(Status::Failed)
    );
    let second = done.steps[2].file_edit.as_ref().unwrap();
    assert_eq!(
        second.expected_sha256,
        Some(content_hash(HALF_FIXED.as_bytes()))
    );
    assert_ne!(done.steps[0].file_edit, done.steps[2].file_edit);
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Verified);
    assert_eq!(r.attempts, 1, "one failed attempt, within the limit");
    assert_eq!(r.changes.len(), 2);
}

#[test]
fn a_stale_proposal_is_recorded_as_a_failed_plan_and_the_next_one_starts_from_the_current_file() {
    let rig = Rig::new();
    let t = rig.controller.create(rig.task("raced")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);
    // The person's edit survived, and the fix was applied on top of it.
    assert_eq!(
        rig.read("src/validation.js"),
        format!("// reviewed by alice\n{FIXED}")
    );
    let ev = rig.events(&t.task_id);
    let failed: Vec<_> = ev
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::PlanFailed {
                status, message, ..
            } => Some((status.clone(), message.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]
            .1
            .contains("not the version the patch was prepared against"),
        "{failed:?}"
    );
    // The stale proposal never produced a plan or an approval.
    assert_eq!(done.steps[0].status, StepStatus::NoPlan);
    assert!(done.steps[0].plan_id.is_none());
}

#[test]
fn a_write_never_substitutes_for_verification_and_invalidates_the_earlier_one() {
    let rig = Rig::new();
    let mut params = rig.task("edit-after-verify");
    params.criteria.push(Criterion {
        id: "validation_present".into(),
        description: "the validation exists in the working tree".into(),
        checks: vec![Check::StdoutContains {
            text: "validation present".into(),
        }],
        after_changes: false,
        verify_intent: Some("make check-validation".into()),
    });
    let t = rig.controller.create(params).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);
    let order: Vec<_> = done
        .steps
        .iter()
        .map(|s| (s.kind, s.file_edit.is_some(), s.intent.clone()))
        .collect();
    // The tests passed once, then a documentation edit landed. The task could
    // not complete on that earlier pass: the tests had to run again.
    let runs = order.iter().filter(|(_, _, i)| i == "make test").count();
    assert_eq!(runs, 2, "{order:?}");
    assert_eq!(order.iter().filter(|(_, e, _)| *e).count(), 2);
    assert_eq!(order.last().unwrap().2, "make test", "{order:?}");
    assert!(rig.read("README.md").contains("Amounts are limited"));

    let ev = rig.events(&t.task_id);
    let mut saw_stale = false;
    for e in &ev {
        if let DevTaskEventData::CriteriaEvaluated { statuses } = &e.data {
            if statuses.iter().any(|s| {
                s.id == "tests_pass"
                    && s.outcome == CriterionOutcome::Unverified
                    && s.reason.contains("stale")
            }) {
                saw_stale = true;
            }
        }
    }
    assert!(
        saw_stale,
        "the earlier passing run was marked stale by the later edit"
    );
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Verified);
    assert_eq!(r.changes.len(), 2);
}

#[test]
fn cancelling_a_task_that_awaits_approval_of_an_edit_means_it_can_never_be_applied() {
    let rig = Rig::new();
    let t = rig.controller.create(rig.task("fix-it")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let v = rig.wait_for(&t.task_id, "the edit awaiting approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    let plan_id = v.steps[0].plan_id.clone().unwrap();
    let digest = v.steps[0].plan_digest.clone().unwrap();
    rig.controller
        .cancel(&t.task_id, Some("changed my mind".into()))
        .unwrap();
    let v = rig.wait_for(&t.task_id, "cancelled", |v| v.state.is_terminal());
    assert_eq!(v.state, TaskState::Cancelled);
    assert_ne!(v.report.as_ref().unwrap().outcome, ReportOutcome::Verified);
    // Even a late approval of that digest cannot revive it.
    let late = rig.runtime.approve(&plan_id, &digest);
    if let Ok(a) = late {
        let e = rig
            .runtime
            .start(&plan_id, &a.approval.approval_id)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::PlanRejected);
    }
    assert_eq!(rig.read("src/validation.js"), BUGGY);
}

#[test]
fn an_outline_edit_cannot_claim_a_criterion_or_pin_a_capability() {
    let rig = Rig::new();
    let h = content_hash(BUGGY.as_bytes());
    let mk = |satisfies: Vec<String>, selection: Option<String>| {
        let mut p = rig.task("outline");
        p.outline = vec![OutlineStep {
            kind: StepKind::Change,
            objective: "add the upper bound".into(),
            intent: "edit".into(),
            selection,
            bindings: Default::default(),
            satisfies,
            file_edit: Some(FileEditSpec {
                path: "src/validation.js".into(),
                expected_sha256: Some(h.clone()),
                hunks: vec![fix_hunk()],
            }),
        }];
        p
    };
    for p in [
        mk(vec!["tests_pass".into()], None),
        mk(vec![], Some("project-task:make:test".into())),
    ] {
        let t = rig.controller.create(p).unwrap();
        rig.controller.start(&t.task_id).unwrap();
        let v = rig.wait_for(&t.task_id, "blocked", |v| {
            v.state == TaskState::Blocked || v.state.is_terminal()
        });
        assert_eq!(v.state, TaskState::Blocked);
        assert!(v.steps.is_empty(), "the offending edit never became a step");
    }
    assert_eq!(rig.read("src/validation.js"), BUGGY);

    // Without the claim, the same edit proceeds to approval.
    let t = rig.controller.create(mk(vec![], None)).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let v = rig.wait_for(&t.task_id, "the edit awaiting approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    assert_eq!(
        v.steps[0].file_edit.as_ref().unwrap().hunks,
        vec![fix_hunk()]
    );
}

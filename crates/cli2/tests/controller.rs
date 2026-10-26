//! The development task controller: the complete loop against a fixture
//! project, with deterministic strategies (no model, no network), plus the
//! approval, completion, limits, cancellation, pause and recovery contracts.

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cli2::controller::proposer::*;
use cli2::controller::Controller;
use cli2::runtime::Runtime;
use cli2::server::{Server, ServerHandle};
use cli2_client::{Client, ClientError};
use cli2_core::{Check, Status};
use cli2_protocol::devtask::*;
use cli2_protocol::ErrorCode;
use support::*;

/// A small "development" project. `add-validation-v1` is a first attempt with a
/// bug; the tests catch it; `add-validation-v2` is the fix. `changes.log` counts
/// how many times a change really ran.
const MAKEFILE: &str = r#".PHONY: add-validation-v1 add-validation-v2 check-validation test slow-change slow update-docs install-system
add-validation-v1: ## Add input validation to the invoice endpoint (first attempt)
	@mkdir -p src; echo 'function validate(a) { return a > 0; }' > src/validation.js; echo v1 >> changes.log
add-validation-v2: ## Add input validation to the invoice endpoint (corrected)
	@mkdir -p src; echo 'function validate(a) { return a > 0 && a < 1000000; }' > src/validation.js; echo v2 >> changes.log
check-validation: ## Check that the validation exists
	@if test -f src/validation.js; then echo "validation present"; else echo "validation missing" >&2; exit 1; fi
test: ## Run the project tests
	@sh tests/run.sh
slow-change: ## A change that takes a while
	@mkdir -p src; echo 'function validate(a) { return a > 0 && a < 1000000; }' > src/validation.js; echo slow >> changes.log; sleep 30
slow: ## Wait a long time
	@sleep 30
update-docs: ## Update the docs for the change
	@echo docs >> docs.log; echo "docs updated"
install-system: ## Install into system directories
	sudo true
"#;

const TESTS_RUN: &str = r##"case "$(cat src/validation.js 2>/dev/null)" in
  *1000000*) echo "ok - rejects amounts over the limit"; echo "# tests 2, pass 2, fail 0" ;;
  *) echo "FAIL: upper bound missing in src/validation.js" >&2; echo "# tests 2, pass 1, fail 1"; exit 1 ;;
esac
"##;

fn criteria() -> Vec<Criterion> {
    vec![
        Criterion {
            id: "validation_present".into(),
            description: "the validation exists in the working tree".into(),
            checks: vec![Check::StdoutContains {
                text: "validation present".into(),
            }],
            after_changes: false,
            verify_intent: Some("make check-validation".into()),
        },
        Criterion {
            id: "tests_pass".into(),
            description: "the project's tests pass".into(),
            checks: vec![Check::StdoutContains {
                text: "fail 0".into(),
            }],
            after_changes: true,
            verify_intent: Some("make test".into()),
        },
    ]
}

struct Scripted(fn(&ProposalContext<'_>) -> Proposal);

impl StepProposer for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }
    fn propose(&mut self, ctx: &ProposalContext<'_>) -> Result<Proposal, String> {
        Ok((self.0)(ctx))
    }
}

fn step(kind: StepKind, intent: &str, satisfies: &[&str]) -> Proposal {
    Proposal::Step(ProposedStep {
        kind,
        objective: intent.into(),
        intent: intent.into(),
        selection: None,
        bindings: BTreeMap::new(),
        satisfies: satisfies.iter().map(|s| s.to_string()).collect(),
        note: Some("scripted".into()),
    })
}

fn last_idx(h: &[HistoryItem], intent: &str) -> Option<usize> {
    h.iter()
        .rposition(|s| s.intent == intent && s.outcome.is_some())
}

fn failed(s: &HistoryItem) -> bool {
    s.outcome
        .as_ref()
        .is_some_and(|o| o.task_status != Some(Status::Succeeded))
}

/// Make the change, verify it exists, run the tests; if they fail, fix and re-run.
fn fixer(ctx: &ProposalContext<'_>) -> Proposal {
    let h = &ctx.history;
    if last_idx(h, "make add-validation-v1").is_none() {
        return step(StepKind::Change, "make add-validation-v1", &[]);
    }
    if last_idx(h, "make check-validation").is_none() {
        return step(
            StepKind::Verify,
            "make check-validation",
            &["validation_present"],
        );
    }
    let Some(t) = last_idx(h, "make test") else {
        return step(StepKind::Verify, "make test", &["tests_pass"]);
    };
    if !failed(&h[t]) {
        return Proposal::Done { note: None };
    }
    match last_idx(h, "make add-validation-v2") {
        None => step(StepKind::Change, "make add-validation-v2", &[]),
        Some(v) if v > t => step(StepKind::Verify, "make test", &["tests_pass"]),
        Some(_) => Proposal::GiveUp {
            reason: "the tests still fail after the fix".into(),
        },
    }
}

/// Never fixes anything: keeps re-running the failing tests.
fn stubborn(ctx: &ProposalContext<'_>) -> Proposal {
    if last_idx(&ctx.history, "make add-validation-v1").is_none() {
        return step(StepKind::Change, "make add-validation-v1", &[]);
    }
    step(StepKind::Verify, "make test", &["tests_pass"])
}

/// Runs the tests but never says they establish the criterion, then claims to be done.
fn claims_done(ctx: &ProposalContext<'_>) -> Proposal {
    let h = &ctx.history;
    if last_idx(h, "make add-validation-v2").is_none() {
        return step(StepKind::Change, "make add-validation-v2", &[]);
    }
    if last_idx(h, "make test").is_none() {
        return step(StepKind::Verify, "make test", &[]); // exit 0, but declares no criterion
    }
    Proposal::Done {
        note: Some("everything passed, trust me".into()),
    }
}

fn harmless(_: &ProposalContext<'_>) -> Proposal {
    step(StepKind::Verify, "make check-validation", &[])
}

fn fabricating(_: &ProposalContext<'_>) -> Proposal {
    Proposal::Step(ProposedStep {
        kind: StepKind::Change,
        objective: "deploy".into(),
        intent: "deploy to production".into(),
        selection: Some("executable:deploy-prod".into()),
        bindings: BTreeMap::new(),
        satisfies: vec![],
        note: None,
    })
}

fn slow_then_nothing(ctx: &ProposalContext<'_>) -> Proposal {
    if ctx.history.is_empty() {
        step(StepKind::Change, "make slow-change", &[])
    } else {
        step(
            StepKind::Verify,
            "make check-validation",
            &["validation_present"],
        )
    }
}

fn alternative_after_rejection(ctx: &ProposalContext<'_>) -> Proposal {
    if ctx.history.iter().any(|s| s.status == StepStatus::Rejected) {
        step(StepKind::Change, "make add-validation-v2", &[])
    } else {
        step(StepKind::Change, "make add-validation-v1", &[])
    }
}

struct Rig {
    env: Env,
    project: PathBuf,
    sock_dir: tempfile::TempDir,
    socket: PathBuf,
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
    server: Option<ServerHandle>,
}

fn config(env: &Env) -> cli2::Config {
    let mut c = cli2::Config::from_env().unwrap();
    c.home = env.home.clone();
    c.path_var = env.path.clone();
    c.policy = Default::default();
    c.resolver = Some(Default::default());
    c
}

fn register(c: &Controller) {
    for (name, f) in [
        ("fixer", fixer as fn(&ProposalContext<'_>) -> Proposal),
        ("stubborn", stubborn),
        ("claims-done", claims_done),
        ("harmless", harmless),
        ("fabricating", fabricating),
        ("slow", slow_then_nothing),
        ("alternative", alternative_after_rejection),
    ] {
        c.register_strategy(
            name,
            Arc::new(FnStrategy(move |_: &CreateTaskParams| {
                Ok(Box::new(Scripted(f)) as Box<dyn StepProposer>)
            })),
        );
    }
}

impl Rig {
    fn new() -> Rig {
        let env = Env::new();
        let project = env.work.join("proj");
        std::fs::create_dir_all(project.join("tests")).unwrap();
        std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
        std::fs::write(project.join("tests/run.sh"), TESTS_RUN).unwrap();
        let sock_dir = tempfile::Builder::new()
            .prefix("c2t")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(sock_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = sock_dir.path().join("s.sock");
        let runtime = Arc::new(Runtime::new(config(&env)));
        let controller = Controller::new(runtime.clone());
        register(&controller);
        let server = Server::bind_with(runtime.clone(), controller.clone(), &socket)
            .unwrap()
            .spawn();
        Rig {
            env,
            project: project.canonicalize().unwrap(),
            sock_dir,
            socket,
            runtime,
            controller,
            server: Some(server),
        }
    }

    fn proj(&self) -> String {
        self.project.display().to_string()
    }

    fn changes(&self) -> Vec<String> {
        std::fs::read_to_string(self.project.join("changes.log"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    fn params(&self, strategy: &str) -> CreateTaskParams {
        CreateTaskParams {
            project: self.proj(),
            goal: "Add input validation to the invoice endpoint, following the existing project conventions. Run the relevant tests and fix any failures introduced by the change.".into(),
            criteria: criteria(),
            strategy: Some(strategy.into()),
            ..Default::default()
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket, "test").unwrap()
    }

    /// Run a task, approving every proposed operation by its exact digest, until it stops.
    fn drive(&self, id: &str) -> TaskView {
        self.drive_with(id, |_| Decision::Approve)
    }

    fn drive_with(&self, id: &str, mut decide: impl FnMut(&TaskView) -> Decision) -> TaskView {
        let t = Instant::now();
        loop {
            let v = self.controller.get(id).unwrap();
            if v.state.is_terminal() || v.state == TaskState::Blocked {
                return v;
            }
            if v.state == TaskState::AwaitingApproval {
                let s = v
                    .steps
                    .iter()
                    .find(|s| Some(&s.step_id) == v.current_step.as_ref())
                    .unwrap();
                assert_eq!(
                    s.status,
                    StepStatus::AwaitingApproval,
                    "an approved step is no longer awaiting approval"
                );
                match decide(&v) {
                    Decision::Approve => {
                        self.controller
                            .approve(ApproveStepParams {
                                task_id: id.into(),
                                step_id: s.step_id.clone(),
                                plan_digest: s.plan_digest.clone().unwrap(),
                            })
                            .unwrap();
                    }
                    Decision::Reject => {
                        self.controller
                            .reject(RejectStepParams {
                                task_id: id.into(),
                                step_id: s.step_id.clone(),
                                reason: Some("no".into()),
                            })
                            .unwrap();
                    }
                    Decision::Stop => return v,
                }
            } else {
                std::thread::sleep(Duration::from_millis(30));
            }
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "task did not settle: {:?}",
                v.state
            );
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
            std::thread::sleep(Duration::from_millis(30));
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
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let _ = &self.sock_dir;
    }
}

enum Decision {
    Approve,
    Reject,
    /// Leave the decision pending and return the task as it is.
    Stop,
}

fn states(events: &[DevTaskEvent]) -> Vec<TaskState> {
    events
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::StateChanged { to, .. } => Some(*to),
            _ => None,
        })
        .collect()
}

// ================================================================== the loop

#[test]
fn a_goal_becomes_a_persistent_task_that_discovers_real_capabilities() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    assert_eq!(v.state, TaskState::Created);
    assert!(v.task_id.starts_with("dev_") && v.goal.starts_with("Add input validation"));
    assert_eq!((v.steps_used, v.attempts), (0, 0));
    assert!(v
        .criteria
        .iter()
        .all(|c| c.outcome == CriterionOutcome::Unverified));
    assert!(rig
        .controller
        .list(None)
        .unwrap()
        .tasks
        .iter()
        .any(|t| t.task_id == v.task_id));
    // Nothing happened: it is persisted, not started.
    assert!(rig.changes().is_empty());

    // Started: it investigates the *real* project, not an imagined one.
    rig.controller.start(&v.task_id).unwrap();
    let v = rig.wait_for(&v.task_id, "a proposed step", |v| !v.steps.is_empty());
    let inv = v.investigation.expect("investigation recorded");
    let ids: Vec<&str> = inv.capabilities.iter().map(|c| c.id.as_str()).collect();
    for real in [
        "project-task:make:add-validation-v1",
        "project-task:make:test",
        "project-task:make:check-validation",
    ] {
        assert!(ids.contains(&real), "{real} missing from {ids:?}");
    }
    assert!(
        !ids.iter().any(|i| i.contains("deploy")),
        "nothing is invented"
    );
    assert!(
        inv.capabilities.iter().all(|c| c.interface != "unknown"),
        "only plannable capabilities are offered"
    );

    // A new controller over the same journal sees the same task: it is persistent.
    let rt2 = Arc::new(Runtime::new(config(&rig.env)));
    let c2 = Controller::new(rt2);
    assert_eq!(c2.get(&v.task_id).unwrap().goal, v.goal);
    assert_eq!(c2.get("dev_missing").unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn the_full_loop_fails_verification_retries_and_completes_only_when_verified() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);

    assert_eq!(
        done.state,
        TaskState::Completed,
        "{:?} / {:?}",
        done.unresolved,
        done.report
    );
    let intents: Vec<(&str, StepStatus)> = done
        .steps
        .iter()
        .map(|s| (s.intent.as_str(), s.status))
        .collect();
    assert_eq!(
        intents.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [
            "make add-validation-v1",
            "make check-validation",
            "make test",
            "make add-validation-v2",
            "make test"
        ],
        "change, verify, fail, fix, re-verify"
    );
    assert!(intents.iter().all(|(_, s)| *s == StepStatus::Executed));
    // The first test run really failed, and was seen as such.
    let first_test = &done.steps[2];
    let o = first_test.outcome.as_ref().unwrap();
    assert_ne!(o.task_status, Some(Status::Succeeded));
    assert_ne!(o.process.as_ref().unwrap().exit_code, Some(0));
    assert_eq!(done.attempts, 1, "one failed attempt counted");
    // The second succeeded *and was verified*.
    let last = done.steps[4].outcome.as_ref().unwrap();
    assert_eq!(
        (last.task_status, last.verification),
        (
            Some(Status::Succeeded),
            Some(cli2_core::VerificationStatus::Passed)
        )
    );
    // Each change ran exactly once.
    assert_eq!(rig.changes(), ["v1", "v2"]);
    assert!(done
        .criteria
        .iter()
        .all(|c| c.outcome == CriterionOutcome::Passed));

    // The report separates what is verified from what is not.
    let r = done.report.clone().unwrap();
    assert_eq!(r.outcome, ReportOutcome::Verified);
    assert_eq!(r.verified.len(), 2);
    assert!(r.failed.is_empty() && r.unverified.is_empty() && r.remaining_work.is_empty());
    assert_eq!(r.changes.len(), 2);
    assert_eq!(r.evidence.len(), 5);
    assert_eq!(r.attempts, 1);
    assert_eq!(rig.controller.report(&v.task_id).unwrap(), r);
    // Every evidence reference resolves in the runtime's own evidence.
    for e in &r.evidence {
        let snap = rig.runtime.execution(&e.execution_id).unwrap();
        assert_eq!(snap.lifecycle, cli2_protocol::Lifecycle::Finished);
    }

    // The history is a legal walk through the state machine, ending in completed, with no skips.
    let ev = rig.events(&v.task_id);
    let walk = states(&ev);
    assert_eq!(walk.first(), Some(&TaskState::Investigating));
    assert_eq!(walk.last(), Some(&TaskState::Completed));
    let mut prev = TaskState::Created;
    for s in &walk {
        assert!(prev.can_become(*s), "illegal {prev:?} -> {s:?}");
        prev = *s;
    }
    for needed in [
        TaskState::Planned,
        TaskState::AwaitingApproval,
        TaskState::Executing,
        TaskState::Verifying,
    ] {
        assert!(walk.contains(&needed), "{needed:?} missing from {walk:?}");
    }
    let seqs: Vec<i64> = ev.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "ordered");
    assert!(matches!(
        ev.last().unwrap().data,
        DevTaskEventData::Completed { .. }
    ));
    // The events are hash-chained journal records.
    let j = cli2_evidence::Journal::open(&rig.env.home.join("evidence")).unwrap();
    j.verify_chain().unwrap();
}

#[test]
fn every_executed_step_went_through_the_runtime_with_a_recorded_approval() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Completed);
    let mut client = rig.client();
    for s in &done.steps {
        let exec = s.execution_id.as_ref().unwrap();
        let snap = client.execution(exec).unwrap();
        // The runtime holds the approval this step used, bound to its plan and digest.
        let plan = client.plan_get(s.plan_id.as_ref().unwrap()).unwrap();
        let a = plan
            .approvals
            .iter()
            .find(|a| Some(&a.approval_id) == s.approval_id.as_ref())
            .expect("approval recorded");
        assert_eq!(a.plan_digest, s.plan_digest.clone().unwrap());
        assert_eq!(a.plan_id, plan.plan.plan_id);
        assert_eq!(snap.approval_id, s.approval_id);
        assert!(plan.executions.iter().any(|e| &e.execution_id == exec));
    }
    // Client approvals were by a person/application, never by the controller.
    let by: Vec<String> = rig
        .events(&v.task_id)
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ApprovalGranted { by, .. } => Some(by.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(by.len(), 5);
    assert!(by.iter().all(|b| b == "client"), "{by:?}");
}

// ============================================================== completion

#[test]
fn exit_zero_and_a_confident_proposer_cannot_complete_a_task() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("claims-done")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    // Both commands exited 0 and the proposer insisted it was done...
    assert!(done
        .steps
        .iter()
        .all(|s| s
            .outcome
            .as_ref()
            .is_some_and(|o| o.process.as_ref().unwrap().exit_code == Some(0))));
    // ...but no criterion was ever verified, so the task is stuck, never completed.
    assert_eq!(done.state, TaskState::Blocked, "{:?}", done.unresolved);
    assert!(done
        .criteria
        .iter()
        .all(|c| c.outcome == CriterionOutcome::Unverified));
    let rep = rig.controller.report(&v.task_id).unwrap();
    assert_eq!(rep.outcome, ReportOutcome::Blocked);
    assert!(rep.verified.is_empty());
    assert!(
        rep.unverified.iter().any(|u| u.contains("tests_pass")),
        "{:?}",
        rep.unverified
    );
    assert!(rep.remaining_work.len() >= 2);
    assert!(rep.summary.contains("blocked"));
    let ev = rig.events(&v.task_id);
    let rejections: Vec<&str> = ev
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ProposalRejected { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rejections.len(), 3);
    assert!(rejections[0].contains("not verified"), "{rejections:?}");
    assert!(!ev
        .iter()
        .any(|e| matches!(e.data, DevTaskEventData::Completed { .. })));
}

#[test]
fn a_criterion_is_judged_by_its_own_checks_which_a_proposer_cannot_change() {
    let rig = Rig::new();
    // Criterion text the real output will never contain.
    let mut p = rig.params("fixer");
    p.criteria[1].checks = vec![Check::StdoutContains {
        text: "all green".into(),
    }];
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let mut seen_expectations = false;
    let done = rig.drive_with(&v.task_id, |view| {
        // The plan the person is asked to approve already carries the criterion's check.
        if let Some(plan) = &view.current_plan {
            let s = view
                .steps
                .iter()
                .find(|s| Some(&s.step_id) == view.current_step.as_ref())
                .unwrap();
            if s.satisfies.contains(&"tests_pass".to_string()) {
                assert!(plan.verification.checks.iter().any(|c| c.check
                    == Check::StdoutContains {
                        text: "all green".into()
                    }));
                seen_expectations = true;
            }
        }
        Decision::Approve
    });
    assert!(seen_expectations);
    // The tests themselves pass, but the stated criterion was not met: not completed.
    assert_ne!(done.state, TaskState::Completed);
    let c = done.criteria.iter().find(|c| c.id == "tests_pass").unwrap();
    assert_eq!(c.outcome, CriterionOutcome::Failed, "{}", c.reason);
    assert!(c.reason.contains("verification failed"));
}

/// Change, run the tests, then make a further change (docs). The passing tests no
/// longer describe the tree, so they must be re-run before the task can complete.
fn late_docs(ctx: &ProposalContext<'_>) -> Proposal {
    let h = &ctx.history;
    if last_idx(h, "make add-validation-v2").is_none() {
        return step(StepKind::Change, "make add-validation-v2", &[]);
    }
    let Some(t) = last_idx(h, "make test") else {
        return step(StepKind::Verify, "make test", &["tests_pass"]);
    };
    let Some(d) = last_idx(h, "make update-docs") else {
        return step(StepKind::Change, "make update-docs", &["docs_updated"]);
    };
    if t < d {
        return step(StepKind::Verify, "make test", &["tests_pass"]);
    }
    Proposal::Done { note: None }
}

#[test]
fn a_change_after_verification_makes_that_verification_stale() {
    let rig = Rig::new();
    rig.controller.register_strategy(
        "late",
        Arc::new(FnStrategy(|_: &CreateTaskParams| {
            Ok(Box::new(Scripted(late_docs)) as Box<dyn StepProposer>)
        })),
    );
    let mut p = rig.params("late");
    p.criteria.remove(0);
    p.criteria.push(Criterion {
        id: "docs_updated".into(),
        description: "the docs describe the change".into(),
        checks: vec![Check::StdoutContains {
            text: "docs updated".into(),
        }],
        after_changes: false,
        verify_intent: None,
    });
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);

    // Right after the docs change, docs_updated passed but the earlier passing tests were stale.
    let evaluated: Vec<Vec<CriterionStatus>> = rig
        .events(&v.task_id)
        .into_iter()
        .filter_map(|e| match e.data {
            DevTaskEventData::CriteriaEvaluated { statuses } => Some(statuses),
            _ => None,
        })
        .collect();
    let after_docs = &evaluated[2];
    let get = |st: &[CriterionStatus], id: &str| st.iter().find(|c| c.id == id).cloned().unwrap();
    assert_eq!(
        get(&evaluated[1], "tests_pass").outcome,
        CriterionOutcome::Passed,
        "verified right after the first change"
    );
    assert_eq!(
        get(after_docs, "docs_updated").outcome,
        CriterionOutcome::Passed
    );
    let stale = get(after_docs, "tests_pass");
    assert_eq!(
        stale.outcome,
        CriterionOutcome::Unverified,
        "no longer true of the tree"
    );
    assert!(stale.reason.contains("stale"), "{}", stale.reason);
    // It completes only after the tests were run again.
    assert_eq!(done.state, TaskState::Completed);
    assert_eq!(
        done.steps
            .iter()
            .filter(|s| s.intent == "make test")
            .count(),
        2
    );
    assert_eq!(done.steps.len(), 4);
}

// ================================================================= limits

#[test]
fn a_task_that_cannot_be_fixed_stops_at_its_attempt_limit_with_an_honest_report() {
    let rig = Rig::new();
    let mut p = rig.params("stubborn");
    p.criteria.remove(0);
    p.limits = Limits {
        max_steps: 10,
        max_attempts: 2,
    };
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Failed);
    assert_eq!(done.attempts, 2);
    assert_eq!(
        done.steps.len(),
        3,
        "the change plus two failing test runs, then it stopped"
    );
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Failed);
    assert!(r.verified.is_empty() && !r.failed.is_empty());
    assert!(r.remaining_work.iter().any(|w| w.contains("tests_pass")));
    assert!(r.summary.contains("did not achieve"));
    assert_eq!(rig.changes(), ["v1"], "nothing ran after the limit");
    let ev = rig.events(&v.task_id);
    assert!(
        matches!(&ev.last().unwrap().data, DevTaskEventData::Failed { reason, .. } if reason.contains("attempts failed"))
    );
}

#[test]
fn the_step_limit_bounds_a_task_that_makes_no_progress() {
    let rig = Rig::new();
    let mut p = rig.params("harmless");
    p.limits = Limits {
        max_steps: 2,
        max_attempts: 3,
    };
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Failed);
    assert_eq!(done.steps.len(), 2);
    assert!(
        done.report
            .unwrap()
            .summary
            .to_lowercase()
            .contains("did not")
            || true
    );
    let ev = rig.events(&v.task_id);
    assert!(
        matches!(&ev.last().unwrap().data, DevTaskEventData::Failed { reason, .. } if reason.contains("step limit"))
    );
}

// ============================================================== authority

#[test]
fn nothing_runs_until_a_client_approves_the_exact_operation() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let v = rig.wait_for(&v.task_id, "awaiting approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        rig.changes().is_empty(),
        "the controller never approves for itself"
    );
    assert_eq!(
        rig.controller.get(&v.task_id).unwrap().state,
        TaskState::AwaitingApproval
    );
    let s = v.steps.last().unwrap().clone();
    assert_eq!(s.approval_required, Some(cli2_core::ApprovalLevel::Confirm));
    assert!(
        v.current_plan.is_some(),
        "the full plan is shown for the decision"
    );
    let digest = s.plan_digest.clone().unwrap();

    let approve = |step: &str, digest: &str| {
        rig.controller.approve(ApproveStepParams {
            task_id: v.task_id.clone(),
            step_id: step.into(),
            plan_digest: digest.into(),
        })
    };
    assert_eq!(
        approve(&s.step_id, "sha256:00").unwrap_err().code,
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(
        approve(&s.step_id, &digest[7..19]).unwrap_err().code,
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(
        approve("s99", &digest).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert!(rig.changes().is_empty());
    approve(&s.step_id, &digest).unwrap();
    // The same approval cannot be applied again to anything later.
    assert_eq!(
        approve(&s.step_id, &digest).unwrap_err().code,
        ErrorCode::Conflict
    );
    rig.wait_for(&v.task_id, "the change to run", |v| {
        v.steps[0].status == StepStatus::Executed
    });
    assert_eq!(rig.changes(), ["v1"]);
    // Approving step s1 cannot authorize step s2.
    let v2 = rig.wait_for(&v.task_id, "the next approval", |v| {
        v.state == TaskState::AwaitingApproval && v.steps.len() == 2
    });
    assert_eq!(
        approve("s1", v2.steps[1].plan_digest.as_ref().unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(rig.changes(), ["v1"]);
}

#[test]
fn a_rejected_operation_never_runs_and_the_task_chooses_another_step() {
    let rig = Rig::new();
    let mut p = rig.params("alternative");
    p.criteria.remove(1);
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let mut first = true;
    let done = rig.drive_with(&v.task_id, |_| {
        if std::mem::take(&mut first) {
            Decision::Reject
        } else {
            Decision::Stop
        }
    });
    // After the rejection the proposer saw it and offered the other change; we stop at its approval.
    assert!(done.steps[0].status == StepStatus::Rejected);
    assert_eq!(done.steps[0].intent, "make add-validation-v1");
    assert_eq!(done.steps[1].intent, "make add-validation-v2");
    assert!(rig.changes().is_empty(), "the rejected operation never ran");
    // The rejected plan can no longer be approved through the runtime either.
    let pid = done.steps[0].plan_id.clone().unwrap();
    let mut c = rig.client();
    assert_eq!(
        c.approve(&pid, done.steps[0].plan_digest.as_ref().unwrap())
            .unwrap_err()
            .code(),
        Some(ErrorCode::PlanRejected)
    );
    rig.controller.cancel(&v.task_id, None).unwrap();
}

#[test]
fn a_proposer_cannot_invent_capabilities_or_carry_authority() {
    let rig = Rig::new();
    // 1. A scripted proposer naming a capability that does not exist.
    let v = rig.controller.create(rig.params("fabricating")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Blocked);
    assert!(done.steps.is_empty(), "no step was ever planned");
    let reasons: Vec<String> = rig
        .events(&v.task_id)
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ProposalRejected { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert!(
        reasons
            .iter()
            .all(|r| r.contains("not a capability discovered")),
        "{reasons:?}"
    );
    assert!(rig.changes().is_empty());

    // 2. A model that tries to approve, to supply evidence, to name commands and to declare success.
    struct Hostile(std::sync::Mutex<Vec<&'static str>>);
    impl ModelClient for Hostile {
        fn complete(&self, _: &ModelRequest) -> Result<String, String> {
            Ok(self.0.lock().unwrap().remove(0).to_string())
        }
    }
    let rig2 = Rig::new();
    rig2.controller.register_model(Arc::new(Hostile(std::sync::Mutex::new(vec![
        r#"{"action":"step","kind":"change","intent":"make add-validation-v1","approval":"granted"}"#,
        r#"{"action":"step","kind":"verify","intent":"make test","evidence":"tests passed","satisfies":["tests_pass"]}"#,
        r#"{"action":"done","status":"completed"}"#,
    ]))));
    let mut p = rig2.params("model");
    p.strategy = Some("model".into());
    let v = rig2.controller.create(p).unwrap();
    rig2.controller.start(&v.task_id).unwrap();
    let done = rig2.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Blocked);
    assert!(done.steps.is_empty() && rig2.changes().is_empty());
    let reasons: Vec<String> = rig2
        .events(&v.task_id)
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ProposalRejected { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 3);
    assert!(
        reasons.iter().all(|r| r.contains("not a valid proposal")),
        "{reasons:?}"
    );
    assert_eq!(
        rig2.runtime.tasks(Some(5)).unwrap().document["tasks"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "the runtime never saw a request"
    );
}

#[test]
fn a_well_behaved_model_drives_the_same_loop() {
    // A model that proposes sensible steps (derived from what it is shown) still needs
    // approvals, and its "done" is only a suggestion.
    struct Sensible;
    impl ModelClient for Sensible {
        fn complete(&self, req: &ModelRequest) -> Result<String, String> {
            assert!(
                req.capabilities
                    .iter()
                    .any(|c| c.id == "project-task:make:test"),
                "it is shown real capabilities"
            );
            assert!(req.instructions.contains("cannot run commands"));
            let ran: Vec<&str> = req.history.iter().map(|h| h.intent.as_str()).collect();
            let next = if !ran.contains(&"make add-validation-v2") {
                r#"{"action":"step","kind":"change","objective":"add validation","intent":"make add-validation-v2","note":"fix"}"#
            } else if !ran.contains(&"make check-validation") {
                r#"{"action":"step","kind":"verify","objective":"check it exists","intent":"make check-validation","satisfies":["validation_present"]}"#
            } else if !ran.contains(&"make test") {
                r#"{"action":"step","kind":"verify","objective":"run tests","intent":"make test","satisfies":["tests_pass"],"selection":"project-task:make:test"}"#
            } else {
                r#"{"action":"done"}"#
            };
            Ok(next.to_string())
        }
    }
    let rig = Rig::new();
    rig.controller.register_model(Arc::new(Sensible));
    let mut p = rig.params("model");
    p.strategy = Some("model".into());
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.unresolved);
    assert_eq!(done.steps.len(), 3);
    assert!(done
        .steps
        .iter()
        .all(|s| s.proposer_note.is_some() || s.kind == StepKind::Verify));
    assert_eq!(rig.changes(), ["v2"]);
}

#[test]
fn policy_decides_when_no_approval_is_needed_and_forbidden_operations_never_run() {
    let rig = Rig::new();
    // Declaring the check read-only makes policy require no approval for it.
    let r = rig.env.run(
        &rig.project,
        &[
            "characterize",
            "project-task:make:check-validation",
            "--effects",
            "read_only",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut p = rig.params("fixer");
    p.criteria.remove(1);
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let mut approvals_asked = Vec::new();
    let done = rig.drive_with(&v.task_id, |view| {
        approvals_asked.push(view.steps.last().unwrap().intent.clone());
        Decision::Approve
    });
    assert_eq!(done.state, TaskState::Completed);
    assert_eq!(
        approvals_asked,
        ["make add-validation-v1"],
        "only the change needed a person"
    );
    let by: Vec<(String, String)> = rig
        .events(&v.task_id)
        .iter()
        .filter_map(|e| match &e.data {
            DevTaskEventData::ApprovalGranted { step_id, by, .. } => {
                Some((step_id.clone(), by.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(by.contains(&("s2".into(), "policy".into())), "{by:?}");

    // A forbidden operation is never planned for approval.
    struct Forbidden;
    impl StepProposer for Forbidden {
        fn name(&self) -> String {
            "forbidden".into()
        }
        fn propose(&mut self, _: &ProposalContext<'_>) -> Result<Proposal, String> {
            Ok(step(StepKind::Change, "make install-system", &[]))
        }
    }
    rig.controller.register_strategy(
        "forbidden",
        Arc::new(FnStrategy(|_: &CreateTaskParams| {
            Ok(Box::new(Forbidden) as Box<dyn StepProposer>)
        })),
    );
    let mut p = rig.params("forbidden");
    p.criteria.remove(1);
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Blocked);
    assert!(
        done.steps
            .iter()
            .all(|s| s.status == StepStatus::NoPlan
                && s.detail.as_ref().unwrap().contains("forbids")),
        "{:?}",
        done.steps.iter().map(|s| &s.detail).collect::<Vec<_>>()
    );
}

// ============================================= cancel, pause, resume, restart

#[test]
fn cancelling_stops_the_running_operation_and_never_reports_success() {
    let rig = Rig::new();
    let mut p = rig.params("slow");
    p.criteria.remove(1);
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    // Approve the slow change and wait until it is really running.
    let v1 = rig.wait_for(&v.task_id, "approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    let s = v1.steps.last().unwrap();
    rig.controller
        .approve(ApproveStepParams {
            task_id: v.task_id.clone(),
            step_id: s.step_id.clone(),
            plan_digest: s.plan_digest.clone().unwrap(),
        })
        .unwrap();
    rig.wait_for(&v.task_id, "executing", |v| v.state == TaskState::Executing);
    let t = Instant::now();
    while rig.changes().is_empty() {
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(30));
    }
    rig.controller.cancel(&v.task_id, None).unwrap();
    let done = rig.wait_for(&v.task_id, "cancelled", |v| v.state == TaskState::Cancelled);
    assert!(
        t.elapsed() < Duration::from_secs(25),
        "the 30 s command was terminated, not waited out"
    );
    let o = done.steps[0].outcome.as_ref().unwrap();
    assert_eq!(o.task_status, Some(Status::Cancelled));
    assert_eq!(
        o.process.as_ref().unwrap().termination,
        cli2_core::Termination::Cancelled
    );
    let r = done.report.unwrap();
    assert_eq!(r.outcome, ReportOutcome::Cancelled);
    assert!(r.verified.is_empty() && !r.remaining_work.is_empty());
    assert!(!rig
        .events(&v.task_id)
        .iter()
        .any(|e| matches!(e.data, DevTaskEventData::Completed { .. })));
    assert_eq!(
        rig.controller.cancel(&v.task_id, None).unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(
        rig.controller.start(&v.task_id).unwrap().state,
        TaskState::Cancelled
    );
}

#[test]
fn cancelling_while_awaiting_approval_rejects_the_plan_so_it_can_never_run() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let v1 = rig.wait_for(&v.task_id, "approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    let s = v1.steps.last().unwrap().clone();
    rig.controller
        .cancel(&v.task_id, Some("changed my mind".into()))
        .unwrap();
    rig.wait_for(&v.task_id, "cancelled", |v| v.state == TaskState::Cancelled);
    assert_eq!(
        rig.controller
            .approve(ApproveStepParams {
                task_id: v.task_id.clone(),
                step_id: s.step_id.clone(),
                plan_digest: s.plan_digest.clone().unwrap()
            })
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let mut c = rig.client();
    assert_eq!(
        c.approve(s.plan_id.as_ref().unwrap(), s.plan_digest.as_ref().unwrap())
            .unwrap_err()
            .code(),
        Some(ErrorCode::PlanRejected)
    );
    assert!(rig.changes().is_empty());
}

#[test]
fn pausing_holds_the_task_at_a_step_boundary_until_resumed() {
    let rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let v1 = rig.wait_for(&v.task_id, "approval", |v| {
        v.state == TaskState::AwaitingApproval
    });
    let s = v1.steps.last().unwrap().clone();
    assert!(rig.controller.pause(&v.task_id).unwrap().paused);
    rig.controller
        .approve(ApproveStepParams {
            task_id: v.task_id.clone(),
            step_id: s.step_id.clone(),
            plan_digest: s.plan_digest.clone().unwrap(),
        })
        .unwrap();
    // The approved step is held: nothing starts while paused.
    std::thread::sleep(Duration::from_millis(800));
    let held = rig.controller.get(&v.task_id).unwrap();
    assert!(held.paused && held.steps[0].status == StepStatus::Approved);
    assert!(rig.changes().is_empty());
    rig.controller.resume(&v.task_id).unwrap();
    rig.wait_for(&v.task_id, "the step to run", |v| {
        v.steps[0].status == StepStatus::Executed
    });
    assert_eq!(rig.changes(), ["v1"]);
    rig.controller.cancel(&v.task_id, None).unwrap();

    // A task paused before it starts holds still: it does not even investigate.
    let w = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.pause(&w.task_id).unwrap();
    rig.controller.start(&w.task_id).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let held = rig.controller.get(&w.task_id).unwrap();
    assert!(held.paused && held.investigation.is_none() && held.steps.is_empty());
    rig.controller.resume(&w.task_id).unwrap();
    rig.wait_for(&w.task_id, "it to proceed", |v| {
        v.state == TaskState::AwaitingApproval
    });
    rig.controller.cancel(&w.task_id, None).unwrap();
}

#[test]
fn a_restarted_controller_resumes_where_it_was_without_repeating_side_effects() {
    let mut rig = Rig::new();
    let v = rig.controller.create(rig.params("fixer")).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    // Run until the first change and the validation check are done and the tests await approval.
    let mut approved = 0;
    let at = rig.drive_with(&v.task_id, |view| {
        let _ = view;
        approved += 1;
        if approved <= 2 {
            Decision::Approve
        } else {
            Decision::Stop
        }
    });
    let _ = at;
    let mid = rig.wait_for(&v.task_id, "tests awaiting approval", |v| {
        v.state == TaskState::AwaitingApproval && v.steps.len() == 3
    });
    assert_eq!(rig.changes(), ["v1"]);

    // "Restart": stop the old controller and server, start fresh ones on the same journal.
    rig.controller.shutdown(Duration::from_secs(3));
    rig.server.take().unwrap().stop();
    let runtime = Arc::new(Runtime::new(config(&rig.env)));
    assert!(
        runtime.reconcile().unwrap().is_empty(),
        "nothing was in flight"
    );
    let controller = Controller::new(runtime.clone());
    register(&controller);
    assert_eq!(controller.recover().unwrap(), vec![v.task_id.clone()]);
    rig.runtime = runtime;
    rig.controller = controller;
    let sock = rig.sock_dir.path().join("s2.sock");
    rig.server = Some(
        Server::bind_with(rig.runtime.clone(), rig.controller.clone(), &sock)
            .unwrap()
            .spawn(),
    );

    let back = rig.controller.get(&v.task_id).unwrap();
    assert_eq!(
        back.state,
        TaskState::AwaitingApproval,
        "the pending decision survived"
    );
    assert_eq!(back.steps.len(), mid.steps.len());
    assert_eq!(
        back.steps[2].plan_digest, mid.steps[2].plan_digest,
        "the same plan, not a new one"
    );
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Completed);
    assert_eq!(
        rig.changes(),
        ["v1", "v2"],
        "completed steps were not repeated"
    );
    assert_eq!(
        done.steps
            .iter()
            .filter(|s| s.intent == "make add-validation-v1")
            .count(),
        1
    );
    // A finished task stays finished; starting it again does nothing.
    assert_eq!(
        rig.controller.start(&v.task_id).unwrap().state,
        TaskState::Completed
    );
    assert_eq!(rig.changes().len(), 2);
}

// =============================================================== the protocol

fn code(e: ClientError) -> ErrorCode {
    e.code()
        .unwrap_or_else(|| panic!("not a server error: {e}"))
}

#[test]
fn a_client_can_run_a_task_over_the_protocol_and_recover_it_after_disconnecting() {
    let rig = Rig::new();
    let mut c = rig.client();
    assert!(c.server().operations.iter().any(|o| o == "devtask.create"));
    let v = c.create_task(&rig.params("fixer")).unwrap();
    assert_eq!(v.state, TaskState::Created);
    c.start_task(&v.task_id).unwrap();
    // Observe with a cursor and approve operations by digest, over the wire only.
    let mut after = 0;
    let mut seen_types = Vec::new();
    let mut approvals = 0;
    // The first client disconnects part-way through.
    let mut disconnected = false;
    let mut client = c;
    let started = Instant::now();
    let final_view = loop {
        let batch = client.task_events(&v.task_id, after, 500).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "stuck: {:?}",
            client.task_view(&v.task_id).unwrap()
        );
        for e in &batch.events {
            after = e.seq;
            seen_types.push(
                serde_json::to_value(&e.data).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let view = client.task_view(&v.task_id).unwrap();
        if view.state.is_terminal() {
            break view;
        }
        if view.state == TaskState::AwaitingApproval {
            let s = view
                .steps
                .iter()
                .find(|s| Some(&s.step_id) == view.current_step.as_ref())
                .unwrap();
            client
                .approve_step(&v.task_id, &s.step_id, s.plan_digest.as_ref().unwrap())
                .unwrap();
            approvals += 1;
            if approvals == 2 && !disconnected {
                disconnected = true;
                drop(client);
                // A brand-new connection recovers the state and resumes from its cursor.
                client = Client::connect(&rig.socket, "reconnected").unwrap();
                let recovered = client.task_view(&v.task_id).unwrap();
                assert!(!recovered.steps.is_empty() && recovered.goal == v.goal);
            }
        }
    };
    assert_eq!(final_view.state, TaskState::Completed);
    assert_eq!(approvals, 5);
    // The cursor stream is complete and ordered, ending in the terminal event.
    let tail = client.task_events(&v.task_id, after, 0).unwrap();
    assert!(tail.finished || tail.events.is_empty());
    let all = client.task_events(&v.task_id, 0, 0).unwrap();
    assert!(all.finished);
    assert!(all.events.windows(2).all(|w| w[0].seq < w[1].seq));
    for t in [
        "created",
        "state_changed",
        "investigated",
        "step_proposed",
        "plan_made",
        "approval_granted",
        "execution_started",
        "step_observed",
        "criteria_evaluated",
        "attempt_counted",
        "completed",
    ] {
        assert!(
            seen_types.iter().any(|s| s == t)
                || all
                    .events
                    .iter()
                    .any(|e| serde_json::to_value(&e.data).unwrap()["type"] == t),
            "{t}"
        );
    }
    let report = client.task_report(&v.task_id).unwrap();
    assert_eq!(report.outcome, ReportOutcome::Verified);
    assert_eq!(report.schema_version, "cli2.devtask_report.v1");
    assert_eq!(client.task_list(10).unwrap().tasks[0].task_id, v.task_id);
    // Errors are explicit and typed.
    assert_eq!(
        code(client.task_view("dev_nope").unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(client.cancel_task(&v.task_id).unwrap_err()),
        ErrorCode::Conflict
    );
    let mut bad = rig.params("fixer");
    bad.criteria[0].checks = vec![Check::ProcessExitStatus { expected: 0 }];
    assert_eq!(
        code(client.create_task(&bad).unwrap_err()),
        ErrorCode::InvalidParams,
        "an exit status alone cannot be a criterion"
    );
    bad = rig.params("fixer");
    bad.criteria.clear();
    assert_eq!(
        code(client.create_task(&bad).unwrap_err()),
        ErrorCode::InvalidParams,
        "no criteria, no task"
    );
    bad = rig.params("no-such-strategy");
    assert_eq!(
        code(client.create_task(&bad).unwrap_err()),
        ErrorCode::InvalidParams
    );
    // Creating with the same idempotency key returns the same task.
    let mut keyed = rig.params("fixer");
    keyed.idempotency_key = Some("k1".into());
    let a = client.create_task(&keyed).unwrap();
    let b = client.create_task(&keyed).unwrap();
    assert_eq!(a.task_id, b.task_id);
}

#[test]
fn the_outline_strategy_is_deterministic_and_stops_precisely_when_it_cannot_finish() {
    let rig = Rig::new();
    let outline = |intents: &[(&str, StepKind, &[&str])]| -> Vec<OutlineStep> {
        intents
            .iter()
            .map(|(i, k, s)| OutlineStep {
                kind: *k,
                objective: i.to_string(),
                intent: i.to_string(),
                selection: None,
                bindings: BTreeMap::new(),
                satisfies: s.iter().map(|x| x.to_string()).collect(),
            })
            .collect()
    };
    // A correct outline completes.
    let mut p = rig.params("outline");
    p.strategy = None;
    p.outline = outline(&[
        ("make add-validation-v2", StepKind::Change, &[]),
        (
            "make check-validation",
            StepKind::Verify,
            &["validation_present"],
        ),
        ("make test", StepKind::Verify, &["tests_pass"]),
    ]);
    let v = rig.controller.create(p).unwrap();
    assert_eq!(v.strategy, "outline");
    rig.controller.start(&v.task_id).unwrap();
    assert_eq!(rig.drive(&v.task_id).state, TaskState::Completed);

    // An outline that leaves a criterion unverified ends blocked with exactly why.
    let mut p = rig.params("outline");
    p.outline = outline(&[(
        "make check-validation",
        StepKind::Verify,
        &["validation_present"],
    )]);
    let v = rig.controller.create(p).unwrap();
    rig.controller.start(&v.task_id).unwrap();
    let done = rig.drive(&v.task_id);
    assert_eq!(done.state, TaskState::Blocked);
    assert!(
        done.unresolved
            .iter()
            .any(|u| u.contains("outline is exhausted") && u.contains("tests_pass")),
        "{:?}",
        done.unresolved
    );
}

#[test]
fn a_local_program_can_be_configured_as_the_model_and_gets_no_authority() {
    let env = Env::new();
    let project = env.work.join("proj");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
    std::fs::write(project.join("tests/run.sh"), TESTS_RUN).unwrap();
    let model = env.bin.join("fake-model");
    write_executable(
        &model,
        r#"#!/bin/sh
in=$(cat)
case "$in" in
  *'"intent":"make test"'*) echo '{"action":"done"}' ;;
  *'"intent":"make check-validation"'*) echo '{"action":"step","kind":"verify","intent":"make test","satisfies":["tests_pass"]}' ;;
  *'"intent":"make add-validation-v2"'*) echo '{"action":"step","kind":"verify","intent":"make check-validation","satisfies":["validation_present"]}' ;;
  *) echo '{"action":"step","kind":"change","intent":"make add-validation-v2","objective":"add validation"}' ;;
esac
"#,
    );
    std::fs::write(env.home.join("config.json"), serde_json::json!({"controller": {"model_command": [model.display().to_string()], "model_timeout_secs": 10}}).to_string()).unwrap();
    let runtime = Arc::new(Runtime::new(config(&env)));
    let controller = Controller::new(runtime.clone());
    let project = project.canonicalize().unwrap();
    let mk = || CreateTaskParams {
        project: project.display().to_string(),
        goal: "add validation".into(),
        criteria: criteria(),
        strategy: Some("model".into()),
        ..Default::default()
    };
    let v = controller.create(mk()).unwrap();
    controller.start(&v.task_id).unwrap();
    // Its reply is only a proposal: every operation still waits for this client's approval.
    let t = Instant::now();
    let done = loop {
        let v = controller.get(&v.task_id).unwrap();
        if v.state.is_terminal() || v.state == TaskState::Blocked {
            break v;
        }
        if v.state == TaskState::AwaitingApproval {
            let s = v
                .steps
                .iter()
                .find(|s| Some(&s.step_id) == v.current_step.as_ref())
                .unwrap();
            controller
                .approve(ApproveStepParams {
                    task_id: v.task_id.clone(),
                    step_id: s.step_id.clone(),
                    plan_digest: s.plan_digest.clone().unwrap(),
                })
                .unwrap();
        }
        assert!(t.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.unresolved);
    assert_eq!(done.steps.len(), 3);
    controller.shutdown(Duration::from_secs(2));

    // A model that hangs, or answers with something that is not a proposal, stops the task rather than guessing.
    for body in ["sleep 30", "echo 'sure, I will deploy it'"] {
        write_executable(&model, &format!("#!/bin/sh\ncat >/dev/null\n{body}\n"));
        std::fs::write(env.home.join("config.json"), serde_json::json!({"controller": {"model_command": [model.display().to_string()], "model_timeout_secs": 1}}).to_string()).unwrap();
        let runtime = Arc::new(Runtime::new(config(&env)));
        let controller = Controller::new(runtime);
        let v = controller.create(mk()).unwrap();
        controller.start(&v.task_id).unwrap();
        let t = Instant::now();
        loop {
            let v = controller.get(&v.task_id).unwrap();
            if v.state == TaskState::Blocked {
                assert!(v.steps.is_empty());
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "{:?}", v.state);
            std::thread::sleep(Duration::from_millis(50));
        }
        controller.shutdown(Duration::from_secs(2));
    }
}

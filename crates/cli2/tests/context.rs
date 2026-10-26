//! The context a proposer is given, exercised against real projects (with a
//! real `git` where one is needed), through the controller. No model, no
//! network: strategies are scripted and record the context they were handed.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cli2::controller::context::*;
use cli2::controller::proposer::*;
use cli2::controller::Controller;
use cli2::runtime::Runtime;
use cli2_core::{Check, Hunk};
use cli2_protocol::devtask::*;
use support::*;

const MAKEFILE: &str = r##".PHONY: test soft-test check-validation slow-change
test: ## Run the project tests
	@sh tests/run.sh
soft-test: ## Report a failure but exit 0
	@echo "# tests 2, pass 1, fail 1"
check-validation: ## Check that the validation exists
	@if test -f src/validation.js; then echo "validation present"; else echo "validation missing" >&2; exit 1; fi
slow-change: ## A change that takes a while
	@mkdir -p src; echo '// slow' >> src/validation.js; sleep 30
"##;

const TESTS_RUN: &str = r##"case "$(cat src/validation.js 2>/dev/null)" in
  *1000000*) echo "ok - rejects amounts over the limit"; echo "# tests 2, pass 2, fail 0" ;;
  *) echo "FAIL: upper bound missing in src/validation.js:2" >&2; echo "# tests 2, pass 1, fail 1"; exit 1 ;;
esac
"##;

const BUGGY: &str = "function validate(a) {\n  return a > 0;\n}\n";

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

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new(which("git").expect("git"))
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Rig {
    env: Env,
    project: PathBuf,
    runtime: Arc<Runtime>,
    controller: Arc<Controller>,
}

fn register(c: &Controller) {
    for (name, f) in [
        (
            "tracked-fix",
            tracked_fix as fn(&ProposalContext<'_>) -> Proposal,
        ),
        ("failures", failures),
        ("slow-then-verify", slow_then_verify),
        ("observer", observer),
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
    /// `with_git`: make `git` available on the controlled PATH, put the project
    /// under version control and commit it.
    fn new(with_git: bool) -> Option<Rig> {
        let env = Env::new();
        if with_git {
            let Some(g) = which("git") else {
                eprintln!("skipping: git is not installed");
                return None;
            };
            let _ = std::os::unix::fs::symlink(&g, env.bin.join("git"));
        }
        let project = env.work.join("proj");
        std::fs::create_dir_all(project.join("tests")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
        std::fs::write(project.join("tests/run.sh"), TESTS_RUN).unwrap();
        std::fs::write(project.join("src/validation.js"), BUGGY).unwrap();
        std::fs::write(project.join("README.md"), "# invoices\n").unwrap();
        if with_git {
            git(&project, &["init", "-q"]);
            git(&project, &["add", "-A"]);
            git(&project, &["commit", "-q", "-m", "init"]);
        }
        let runtime = Arc::new(Runtime::new(config(&env)));
        let controller = Controller::new(runtime.clone());
        register(&controller);
        Some(Rig {
            env,
            project: project.canonicalize().unwrap(),
            runtime,
            controller,
        })
    }

    fn proj(&self) -> String {
        self.project.display().to_string()
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.project.join(p)).unwrap()
    }

    fn task(&self, strategy: &str) -> CreateTaskParams {
        CreateTaskParams {
            project: self.proj(),
            goal: "Add the upper bound to the amount validation in src/validation.js; the tests must pass.".into(),
            criteria: vec![
                Criterion {
                    id: "tests_pass".into(),
                    description: "the project's tests pass".into(),
                    checks: vec![Check::StdoutContains { text: "fail 0".into() }],
                    after_changes: true,
                    verify_intent: Some("make test".into()),
                },
                Criterion {
                    id: "validation_present".into(),
                    description: "the validation exists".into(),
                    checks: vec![Check::StdoutContains { text: "validation present".into() }],
                    after_changes: false,
                    verify_intent: Some("make check-validation".into()),
                },
            ],
            strategy: Some(strategy.into()),
            ..Default::default()
        }
    }

    fn drive(&self, c: &Arc<Controller>, id: &str) -> TaskView {
        let t = Instant::now();
        loop {
            let v = c.get(id).unwrap();
            if v.state.is_terminal() || v.state == TaskState::Blocked {
                return v;
            }
            if v.state == TaskState::AwaitingApproval {
                let s = v
                    .steps
                    .iter()
                    .find(|s| Some(&s.step_id) == v.current_step.as_ref())
                    .unwrap();
                c.approve(ApproveStepParams {
                    task_id: id.into(),
                    step_id: s.step_id.clone(),
                    plan_digest: s.plan_digest.clone().unwrap(),
                })
                .unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(25));
            }
            assert!(t.elapsed() < Duration::from_secs(60), "task did not settle");
        }
    }

    fn wait_for(
        &self,
        c: &Controller,
        id: &str,
        what: &str,
        pred: impl Fn(&TaskView) -> bool,
    ) -> TaskView {
        let t = Instant::now();
        loop {
            let v = c.get(id).unwrap();
            if pred(&v) {
                return v;
            }
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}; {:?}",
                v.state
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn events(&self, c: &Controller, id: &str) -> Vec<DevTaskEvent> {
        c.events(&TaskEventsParams {
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
        self.runtime.shutdown(Duration::from_millis(500));
        let _ = &self.env;
    }
}

// --------------------------------------------------------------- strategies

/// Every context handed to a proposer, by task.
static CAPTURED: Mutex<BTreeMap<String, Vec<ContextSnapshot>>> = Mutex::new(BTreeMap::new());

fn captured(task: &str) -> Vec<ContextSnapshot> {
    CAPTURED
        .lock()
        .unwrap()
        .get(task)
        .cloned()
        .unwrap_or_default()
}

struct Scripted(fn(&ProposalContext<'_>) -> Proposal);

impl StepProposer for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }
    fn propose(&mut self, ctx: &ProposalContext<'_>) -> Result<Proposal, String> {
        CAPTURED
            .lock()
            .unwrap()
            .entry(ctx.task_id.to_string())
            .or_default()
            .push(ctx.context.clone());
        Ok((self.0)(ctx))
    }
}

fn verify(intent: &str, satisfies: &str) -> Proposal {
    Proposal::Step(ProposedStep {
        kind: StepKind::Verify,
        objective: intent.into(),
        intent: intent.into(),
        selection: None,
        bindings: BTreeMap::new(),
        satisfies: vec![satisfies.into()],
        note: Some("scripted".into()),
    })
}

fn edit(path: &str, expected: Option<&str>, hunk: Hunk, why: &str) -> Proposal {
    Proposal::Edit(ProposedEdit {
        objective: why.into(),
        edit: FileEditSpec {
            path: path.into(),
            expected_sha256: expected.map(String::from),
            hunks: vec![hunk],
        },
        note: Some("I am sure this is verified and correct".into()),
    })
}

/// fix the file, run the tests, then also add a changelog (a later change that
/// invalidates the test run), and run the tests again.
fn tracked_fix(ctx: &ProposalContext<'_>) -> Proposal {
    let edits = ctx.history.iter().filter(|s| s.file_edit.is_some()).count();
    let runs = ctx
        .history
        .iter()
        .filter(|s| s.intent == "make test")
        .count();
    match (edits, runs) {
        (0, _) => {
            let f = ctx.files.read("src/validation.js").unwrap();
            edit(&f.path, Some(&f.sha256), fix_hunk(), "add the upper bound")
        }
        (1, 0) => verify("make test", "tests_pass"),
        (1, _) => edit(
            "CHANGELOG.md",
            None,
            Hunk {
                start_line: 1,
                old_text: String::new(),
                new_text: "- upper bound added\n".into(),
            },
            "record the change",
        ),
        (_, 1) => verify("make test", "tests_pass"),
        _ if !ctx
            .history
            .iter()
            .any(|s| s.intent == "make check-validation") =>
        {
            verify("make check-validation", "validation_present")
        }
        _ => Proposal::Done { note: None },
    }
}

/// A command that fails, a check that does not hold, an invalid proposal, and
/// then it stops: the context handed over at the end shows each distinctly.
fn failures(ctx: &ProposalContext<'_>) -> Proposal {
    let rejected = ctx
        .context
        .items
        .iter()
        .any(|i| i.id.starts_with("rejection:"));
    match ctx.history.len() {
        0 => verify("make test", "tests_pass"),
        1 => verify("make soft-test", "tests_pass"),
        _ if !rejected => Proposal::Step(ProposedStep {
            kind: StepKind::Change,
            objective: "use a tool that does not exist".into(),
            intent: "make nope".into(),
            selection: Some("project-task:make:nope".into()),
            bindings: BTreeMap::new(),
            satisfies: vec![],
            note: None,
        }),
        _ => Proposal::GiveUp {
            reason: "done demonstrating".into(),
        },
    }
}

fn slow_then_verify(ctx: &ProposalContext<'_>) -> Proposal {
    if ctx.history.is_empty() {
        Proposal::Step(ProposedStep {
            kind: StepKind::Change,
            objective: "slow change".into(),
            intent: "make slow-change".into(),
            selection: None,
            bindings: BTreeMap::new(),
            satisfies: vec![],
            note: None,
        })
    } else {
        verify("make check-validation", "validation_present")
    }
}

/// Does nothing but record the context, then proposes the harmless check once.
fn observer(ctx: &ProposalContext<'_>) -> Proposal {
    if ctx.history.is_empty() {
        verify("make check-validation", "validation_present")
    } else {
        Proposal::GiveUp {
            reason: "observed".into(),
        }
    }
}

// ----------------------------------------------------------------- helpers

fn tree(c: &ContextSnapshot) -> (bool, Vec<TreeChange>) {
    match &c.item("working_tree").expect("working_tree").content {
        Content::WorkingTree {
            available, entries, ..
        } => (*available, entries.clone()),
        _ => unreachable!(),
    }
}

fn attempt(c: &ContextSnapshot, step: &str) -> Attempt {
    match &c
        .item(&format!("attempt:{step}"))
        .unwrap_or_else(|| panic!("no attempt {step}"))
        .content
    {
        Content::Attempt(a) => (**a).clone(),
        _ => unreachable!(),
    }
}

fn failure_of(a: &Attempt) -> Option<FailureKind> {
    match &a.result {
        AttemptResult::Failed { failure } => Some(failure.kind),
        _ => None,
    }
}

fn crit(c: &ContextSnapshot, id: &str) -> (String, Freshness) {
    let it = c.item(&format!("criterion:{id}")).unwrap();
    match &it.content {
        Content::Criterion { status, .. } => (status.clone(), it.freshness.clone()),
        _ => unreachable!(),
    }
}

// ============================================================ working tree

#[test]
fn a_dirty_working_tree_stays_visible_and_is_never_assumed_to_be_the_agents() {
    let Some(rig) = Rig::new(true) else { return };
    // The user's work in progress, before the task exists.
    std::fs::write(
        rig.project.join("README.md"),
        "# invoices\n\nWIP: ask finance about limits\n",
    )
    .unwrap();
    std::fs::write(rig.project.join("notes.txt"), "scratch\n").unwrap();
    let readme_before = rig.read("README.md");

    let t = rig.controller.create(rig.task("tracked-fix")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&rig.controller, &t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);

    let ctxs = captured(&t.task_id);
    assert!(ctxs.len() >= 5, "{}", ctxs.len());

    // From the very first proposal the user's changes are there, as theirs.
    let (available, first) = tree(&ctxs[0]);
    assert!(available);
    let attr = |entries: &[TreeChange]| {
        entries
            .iter()
            .map(|e| (e.path.clone(), (e.status.clone(), e.attribution.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    let first = attr(&first);
    assert_eq!(first["README.md"], (" M".into(), "pre_existing".into()));
    assert_eq!(first["notes.txt"], ("??".into(), "pre_existing".into()));
    assert_eq!(first.len(), 2);

    // The user's modified file is offered as source, labelled as theirs.
    let Content::Source {
        selected_because,
        text,
        ..
    } = &ctxs[0].item("source:README.md").unwrap().content
    else {
        panic!()
    };
    assert!(
        selected_because
            .iter()
            .any(|r| r.contains("before the task began")),
        "{selected_because:?}"
    );
    assert!(text.contains("WIP: ask finance"));

    // After the task edited src/validation.js and created CHANGELOG.md, those
    // are attributed to the task's edits; the user's files are still the user's.
    let (_, last) = tree(ctxs.last().unwrap());
    let last = attr(&last);
    assert_eq!(last["README.md"].1, "pre_existing");
    assert_eq!(last["notes.txt"].1, "pre_existing");
    assert_eq!(last["src/validation.js"].1, "task_edit");
    assert_eq!(last["CHANGELOG.md"].1, "task_edit");
    assert_eq!(last.values().filter(|v| v.1 == "task_edit").count(), 2);
    assert_eq!(
        rig.read("README.md"),
        readme_before,
        "the user's change was not touched"
    );
    assert_eq!(rig.read("notes.txt"), "scratch\n");

    // The baseline is a task event, from the existing journal.
    assert!(rig
        .events(&rig.controller, &t.task_id)
        .iter()
        .any(|e| matches!(&e.data, DevTaskEventData::WorkingTreeBaseline { tree } if tree.entries.len() == 2)));
}

#[test]
fn a_tree_that_cannot_be_inspected_is_reported_as_such_not_as_clean() {
    let Some(rig) = Rig::new(false) else { return };
    let t = rig.controller.create(rig.task("observer")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    rig.drive(&rig.controller, &t.task_id);
    let ctxs = captured(&t.task_id);
    let (available, entries) = tree(&ctxs[0]);
    assert!(!available);
    assert!(entries.is_empty());
    let Content::WorkingTree {
        unavailable_reason, ..
    } = &ctxs[0].item("working_tree").unwrap().content
    else {
        panic!()
    };
    assert!(
        unavailable_reason.as_deref().unwrap().contains("git"),
        "{unavailable_reason:?}"
    );
}

// ================================================== rebuilt after each change

#[test]
fn the_context_is_rebuilt_after_every_state_change_and_old_verification_goes_stale() {
    let Some(rig) = Rig::new(true) else { return };
    let t = rig.controller.create(rig.task("tracked-fix")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&rig.controller, &t.task_id);
    assert_eq!(done.state, TaskState::Completed, "{:?}", done.report);
    let ctxs = captured(&t.task_id);

    // One context per proposal; each reflects strictly newer task state.
    assert_eq!(
        ctxs.len(),
        done.steps.len(),
        "one per proposal; none once every criterion is verified"
    );
    for w in ctxs.windows(2) {
        assert!(
            w[0].as_of_seq < w[1].as_of_seq,
            "{} !< {}",
            w[0].as_of_seq,
            w[1].as_of_seq
        );
        assert_ne!(w[0].digest, w[1].digest);
    }
    // They are all within budget and say what they are.
    for c in &ctxs {
        assert!(c.size() <= c.max_bytes);
        assert_eq!(c.schema, CONTEXT_SCHEMA);
        assert_eq!(c.task_id, t.task_id);
    }

    // After the edit: the file the proposer will see next has changed hash.
    let src = |c: &ContextSnapshot| match &c.item("source:src/validation.js").unwrap().content {
        Content::Source { sha256, text, .. } => (sha256.clone(), text.clone()),
        _ => unreachable!(),
    };
    let (h0, text0) = src(&ctxs[0]);
    let (h1, text1) = src(&ctxs[1]);
    assert_ne!(h0, h1);
    assert!(!text0.contains("1000000") && text1.contains("1000000"));

    // ctx[2] follows the test run: the criterion passed and is current.
    assert_eq!(
        crit(&ctxs[2], "tests_pass"),
        ("passed".into(), Freshness::Current)
    );
    let a = attempt(&ctxs[2], "s2");
    assert_eq!(a.kind, StepKind::Verify);
    assert_eq!(
        ctxs[2].item("attempt:s2").unwrap().freshness,
        Freshness::Current
    );

    // ctx[3] follows the CHANGELOG edit: the earlier pass no longer counts, and
    // the context says why, while keeping the run for explanation.
    let (status, fresh) = crit(&ctxs[3], "tests_pass");
    assert_eq!(status, "unverified");
    assert!(
        matches!(&fresh, Freshness::Stale { reason } if reason.contains("stale")),
        "{fresh:?}"
    );
    assert!(matches!(
        ctxs[3].item("attempt:s2").unwrap().freshness,
        Freshness::Stale { .. }
    ));
    assert_eq!(
        attempt(&ctxs[3], "s2").evidence.len(),
        2,
        "its evidence is still referenced"
    );

    // The final state is verified again, after the last change.
    let last = ctxs.last().unwrap();
    assert_eq!(
        crit(last, "tests_pass"),
        ("passed".into(), Freshness::Current)
    );

    // A proposer's confident note never became evidence.
    for c in &ctxs {
        for it in &c.items {
            if let Content::Suggestion { .. } = it.content {
                assert_eq!(it.provenance, Provenance::ModelSuggestion);
            }
            if it.provenance == Provenance::Verified {
                assert!(matches!(it.content, Content::Criterion { .. }));
            }
        }
    }
    // Only digests and coverage were journaled; the content itself was not stored again.
    let built: Vec<_> = rig
        .events(&rig.controller, &t.task_id)
        .into_iter()
        .filter_map(|e| match e.data {
            DevTaskEventData::ContextBuilt { digest, bytes, .. } => Some((digest, bytes)),
            _ => None,
        })
        .collect();
    assert_eq!(built.len(), ctxs.len());
    for ((d, bytes), c) in built.iter().zip(&ctxs) {
        assert_eq!(d, &c.digest);
        assert_eq!(*bytes as usize, c.size());
    }
}

// ================================================================== failures

#[test]
fn each_kind_of_failure_reaches_the_next_proposal_distinctly() {
    let Some(rig) = Rig::new(true) else { return };
    let t = rig.controller.create(rig.task("failures")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    let done = rig.drive(&rig.controller, &t.task_id);
    assert_eq!(done.state, TaskState::Blocked, "{:?}", done.unresolved);
    let c = captured(&t.task_id).pop().unwrap();

    // The command that ran and failed, with its real output.
    let s1 = attempt(&c, "s1");
    assert_eq!(failure_of(&s1), Some(FailureKind::CommandFailure));
    assert_eq!(
        s1.exit.as_deref(),
        Some("exit status 2"),
        "make reports 2 when a recipe fails"
    );
    assert!(s1
        .output
        .as_ref()
        .unwrap()
        .stderr
        .contains("upper bound missing"));
    // The check that ran to completion but did not hold.
    let s2 = attempt(&c, "s2");
    assert_eq!(
        failure_of(&s2),
        Some(FailureKind::VerificationFailure),
        "{:?}",
        s2.result
    );
    assert_eq!(s2.exit.as_deref(), Some("exit status 0"));
    assert!(s2.output.as_ref().unwrap().stdout.contains("fail 1"));
    // The proposal the controller refused before anything was planned.
    let rej = c
        .items
        .iter()
        .find(|i| i.id.starts_with("rejection:"))
        .unwrap();
    let Content::RefusedProposal {
        failure,
        reason,
        proposed,
        ..
    } = &rej.content
    else {
        panic!()
    };
    assert_eq!(failure.kind, FailureKind::InvalidProposal);
    assert!(reason.contains("project-task:make:nope"));
    assert!(proposed.as_ref().unwrap().contains("make nope"));
    // Three different reasons, three different representations.
    let kinds = [failure_of(&s1), failure_of(&s2), Some(failure.kind)];
    assert_eq!(
        kinds
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    // The failed test names a file; the context offers it, centered on that line.
    let Content::Source {
        selected_because, ..
    } = &c.item("source:src/validation.js").unwrap().content
    else {
        panic!()
    };
    assert!(
        selected_because
            .iter()
            .any(|r| r.contains("named in the output")),
        "{selected_because:?}"
    );
}

// ============================================================ unknown outcome

#[test]
fn an_unknown_outcome_stays_unknown_until_fresh_evidence_and_is_never_shown_as_a_failure() {
    let Some(rig) = Rig::new(true) else { return };
    let t = rig.controller.create(rig.task("slow-then-verify")).unwrap();
    rig.controller.start(&t.task_id).unwrap();
    // The change starts and takes a while.
    let v = rig.wait_for(
        &rig.controller,
        &t.task_id,
        "the change awaiting approval",
        |v| v.state == TaskState::AwaitingApproval,
    );
    let s = &v.steps[0];
    rig.controller
        .approve(ApproveStepParams {
            task_id: t.task_id.clone(),
            step_id: s.step_id.clone(),
            plan_digest: s.plan_digest.clone().unwrap(),
        })
        .unwrap();
    rig.wait_for(&rig.controller, &t.task_id, "the change executing", |v| {
        v.steps
            .first()
            .is_some_and(|s| s.status == StepStatus::Executing)
    });
    // It touched the tree already (it appends before sleeping).
    let t0 = Instant::now();
    while !rig.read("src/validation.js").contains("// slow") {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }

    // Another runtime takes over the journal and finds an execution that never finished.
    rig.controller.shutdown(Duration::from_secs(1));
    let rt2 = Arc::new(Runtime::new(config(&rig.env)));
    assert_eq!(rt2.reconcile().unwrap().len(), 1);
    let c2 = Controller::new(rt2.clone());
    register(&c2);
    assert_eq!(c2.recover().unwrap(), vec![t.task_id.clone()]);
    let blocked = rig.wait_for(&c2, &t.task_id, "blocked on the unknown outcome", |v| {
        v.state == TaskState::Blocked
    });
    assert_eq!(blocked.steps[0].status, StepStatus::Unknown);

    // What the next proposer would see.
    let ctx = c2.context(&t.task_id).unwrap();
    let a = attempt(&ctx, "s1");
    assert_eq!(
        failure_of(&a),
        Some(FailureKind::UnknownOutcome),
        "{:?}",
        a.result
    );
    assert_ne!(failure_of(&a), Some(FailureKind::CommandFailure));
    let AttemptResult::Failed { failure } = &a.result else {
        panic!()
    };
    assert!(failure.unknown.contains("NOT known to have failed"));
    assert!(a.output.is_none(), "there is no outcome to show");
    assert!(
        a.probe.as_ref().unwrap().recorded.contains("unknown"),
        "{:?}",
        a.probe
    );
    // The tree shows the change that may have come from it, unattributed to anyone.
    let (_, entries) = tree(&ctx);
    let e = entries
        .iter()
        .find(|e| e.path == "src/validation.js")
        .unwrap();
    assert_eq!(e.attribution, "since_baseline");

    // Resuming asks for fresh evidence; a passing check afterwards does not
    // retroactively decide what the interrupted change did.
    c2.resume(&t.task_id).unwrap();
    let done = rig.drive(&c2, &t.task_id);
    assert!(done.steps.len() >= 2);
    let after = c2.context(&t.task_id).unwrap();
    let a = attempt(&after, "s1");
    assert_eq!(
        failure_of(&a),
        Some(FailureKind::UnknownOutcome),
        "still unresolved"
    );
    assert!(matches!(
        attempt(&after, "s2").result,
        AttemptResult::Succeeded { .. }
    ));

    c2.shutdown(Duration::from_secs(1));
    rt2.shutdown(Duration::from_millis(500));
}

// ================================================================= the budget

#[test]
fn the_budget_holds_on_a_real_project_and_what_was_cut_is_reported() {
    let Some(rig) = Rig::new(true) else { return };
    // Make the project large: many source files named in the goal, big files.
    let mut names = Vec::new();
    for i in 0..12 {
        let name = format!("src/module_{i:02}.js");
        std::fs::write(
            rig.project.join(&name),
            "// module code\nfunction f() { return 1; }\n".repeat(400),
        )
        .unwrap();
        names.push(name);
    }
    for i in 0..80 {
        std::fs::write(rig.project.join(format!("scratch_{i:02}.tmp")), "x").unwrap();
    }
    let mut p = rig.task("observer");
    p.goal = format!("Review {} and src/validation.js", names.join(" "));
    let t = rig.controller.create(p).unwrap();

    for max in [4096usize, 8192, 20_000, 64 * 1024] {
        rig.controller
            .set_context_budget(ContextBudget::with_max_bytes(max))
            .unwrap();
        let c = rig.controller.context(&t.task_id).unwrap();
        assert!(c.size() <= max, "{} > {max}", c.size());
        assert!(c.item("task").is_some() && c.item("criterion:tests_pass").is_some());
        if max <= 8192 {
            assert!(!c.manifest.complete, "{max}");
            let cov = &c.manifest.coverage;
            assert!(
                cov.source_files[0] < cov.source_files[1]
                    || cov.tree_entries[0] < cov.tree_entries[1],
                "{cov:?}"
            );
            assert!(
                !c.manifest.cut.is_empty()
                    || !c.manifest.omitted.is_empty()
                    || c.manifest.omitted_more > 0
            );
        }
        // Big files are excerpts and the item says so.
        if let Some(it) = c.item("source:src/module_00.js") {
            let Content::Source {
                total_bytes,
                text,
                sha256,
                ..
            } = &it.content
            else {
                panic!()
            };
            assert!(text.len() < *total_bytes);
            assert!(it
                .partial
                .as_ref()
                .is_some_and(|p| p.kept == text.len() && p.total == *total_bytes));
            assert_eq!(
                sha256,
                &cli2_core::content_hash(
                    &std::fs::read(rig.project.join("src/module_00.js")).unwrap()
                )
            );
        }
    }
    assert!(rig
        .controller
        .set_context_budget(ContextBudget::with_max_bytes(100))
        .is_err());
}

#[test]
fn the_budget_can_be_set_in_the_config_file() {
    let Some(rig) = Rig::new(false) else { return };
    std::fs::write(
        rig.env.home.join("config.json"),
        r#"{"controller":{"context_max_bytes":6000}}"#,
    )
    .unwrap();
    let controller = Controller::new(rig.runtime.clone());
    register(&controller);
    let t = controller.create(rig.task("observer")).unwrap();
    let c = controller.context(&t.task_id).unwrap();
    assert_eq!(c.max_bytes, 6000);
    assert!(c.size() <= 6000);
}

// ======================================================= determinism, safety

#[test]
fn the_same_state_gives_the_same_context_and_building_it_changes_nothing() {
    let Some(rig) = Rig::new(true) else { return };
    let t = rig.controller.create(rig.task("observer")).unwrap();
    let before_events = rig.events(&rig.controller, &t.task_id);
    let tree_before = (rig.read("src/validation.js"), rig.read("README.md"));

    let a = rig.controller.context(&t.task_id).unwrap();
    let b = rig.controller.context(&t.task_id).unwrap();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
    assert_eq!(a.digest, b.digest);

    // No authority: nothing was planned, approved, started or recorded, and no
    // project file changed.
    assert_eq!(rig.events(&rig.controller, &t.task_id), before_events);
    assert_eq!(
        (rig.read("src/validation.js"), rig.read("README.md")),
        tree_before
    );
    let v = rig.controller.get(&t.task_id).unwrap();
    assert_eq!(v.state, TaskState::Created);
    assert!(v.steps.is_empty());

    // A new project state is a different context.
    std::fs::write(rig.project.join("src/validation.js"), "// changed\n").unwrap();
    let c = rig.controller.context(&t.task_id).unwrap();
    assert_ne!(a.digest, c.digest);
}

// ============================================================ git is read-only

#[test]
fn inspecting_the_working_tree_cannot_be_made_to_run_code_from_the_repository() {
    let Some(rig) = Rig::new(true) else { return };
    let marker = rig.env.work.join("fsmonitor-ran");
    let hook = rig.env.work.join("hook.sh");
    write_executable(
        &hook,
        &format!("#!/bin/sh\ntouch {}\nprintf '\\0'\n", marker.display()),
    );
    git(
        &rig.project,
        &["config", "core.fsmonitor", hook.to_str().unwrap()],
    );
    // The attack is real: an ordinary `git status` runs the configured program.
    git(&rig.project, &["status", "--porcelain"]);
    let raw_ran = marker.exists();
    let _ = std::fs::remove_file(&marker);

    let t = rig.controller.create(rig.task("observer")).unwrap();
    let c = rig.controller.context(&t.task_id).unwrap();
    let (available, _) = tree(&c);
    assert!(available, "git status still works");
    assert!(
        !marker.exists(),
        "the repository's configuration executed code"
    );
    // The investigation records a baseline the same way.
    rig.controller.start(&t.task_id).unwrap();
    rig.drive(&rig.controller, &t.task_id);
    assert!(
        !marker.exists(),
        "the repository's configuration executed code"
    );
    assert!(
        raw_ran,
        "(sanity) plain `git status` was expected to run the fsmonitor hook on this git"
    );
}

//! The evaluation harness itself: determinism, failure-stage classification,
//! process-versus-task outcomes, side-effect detection, stale/ineligible
//! handling, retrieval quality, and the absence of side effects on the repo.

mod support;

use std::path::PathBuf;

use cli2::eval::{run_suite, CaseResult, Stage, SuiteOptions};
use serde_json::{json, Value};
use support::*;

fn suite_lines() -> Vec<Value> {
    std::fs::read_to_string(repo_root().join("evals/workflows-v1.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn case(id: &str) -> Value {
    suite_lines()
        .into_iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
}

/// Write a suite file and run it. The sandboxes are temporary and removed.
fn run(cases: Vec<Value>) -> Vec<CaseResult> {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("suite.jsonl");
    std::fs::write(
        &path,
        cases
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    run_suite(&repo_root(), &path, &SuiteOptions::default())
        .unwrap()
        .cases
}

fn run_report(cases: Vec<Value>) -> cli2::eval::EvalReport {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("suite.jsonl");
    std::fs::write(
        &path,
        cases
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    run_suite(&repo_root(), &path, &SuiteOptions::default()).unwrap()
}

fn with(mut c: Value, edits: Value) -> Value {
    for (k, v) in edits.as_object().unwrap() {
        c[k] = v.clone();
    }
    c
}

macro_rules! need_make {
    () => {
        if which("make").is_none() {
            eprintln!("SKIPPED: make is not installed");
            return;
        }
    };
}

fn stage(r: &CaseResult) -> Option<Stage> {
    r.first_failure.as_ref().map(|f| f.stage)
}

#[test]
fn results_are_deterministic_and_preserved_per_case() {
    need_make!();
    let ids = [
        "run-tests",
        "lint-checks",
        "failing-test-diagnostics",
        "reuse-after-recipe-change",
        "reuse-after-target-removed",
    ];
    let cases: Vec<Value> = ids.iter().map(|i| case(i)).collect();
    let a = run_report(cases.clone());
    let b = run_report(cases);
    assert_eq!(
        a.results_fingerprint, b.results_fingerprint,
        "same suite, same outcomes"
    );
    assert_eq!(
        a.cases.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ids,
        "per-case results in suite order"
    );
    assert!(a
        .cases
        .iter()
        .all(|c| c.attempt.is_some() && c.skipped.is_none()));
    assert!(
        a.cases.iter().all(|c| c.passed),
        "{:#?}",
        a.cases
            .iter()
            .filter(|c| !c.passed)
            .map(|c| &c.first_failure)
            .collect::<Vec<_>>()
    );
    // Metrics keep their own denominators; there is no combined score.
    let v = serde_json::to_value(&a).unwrap();
    assert!(v["metrics"]["plan_validity"]["of"].as_u64().unwrap() >= 4);
    assert!(v.get("score").is_none() && v["metrics"].get("overall").is_none());
    // Process failure is an *expected* outcome for the failing-test case, not a harness error.
    let f = a
        .cases
        .iter()
        .find(|c| c.id == "failing-test-diagnostics")
        .unwrap()
        .attempt
        .as_ref()
        .unwrap();
    assert_eq!(
        (f.process_exit_zero, f.process_as_expected, f.exit_code),
        (Some(false), Some(true), Some(2))
    );
    assert!(f.verification_status.as_deref() != Some("passed"));
}

#[test]
fn the_earliest_failing_stage_is_identified_for_each_kind_of_failure() {
    need_make!();
    let base = case("run-tests");
    let cases = vec![
        with(
            base.clone(),
            json!({"id": "resolution", "capabilities": ["project-task:make:lint"], "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "discovery", "capabilities": ["project-task:make:does-not-exist"], "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "planning", "plan": {"argv": ["make", "lint"], "approval": "confirm"}, "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "authority", "plan": {"argv": ["make", "test"], "approval": "none"}, "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "execution", "expect": {"process": "failed", "exit_code": 3, "task": "verified", "output_contains": []}, "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "verification", "args": [], "variants": []}),
        ),
        with(
            base.clone(),
            json!({"id": "diagnostics", "expect": {"process": "succeeded", "task": "verified", "output_contains": ["never printed"]}, "variants": []}),
        ),
        // The tool is named exactly but its interface was never captured.
        json!({"id": "interface", "category": "search", "request": "fsearch", "fixture": "dev-project", "tools": ["fsearch"],
               "capabilities": ["executable:fsearch"], "plan": {"approval": "confirm"}, "execute": false,
               "expect": {"process": "not_run", "task": "not_run", "output_contains": []}}),
    ];
    let got: std::collections::BTreeMap<String, (bool, Option<Stage>, String)> = run(cases)
        .into_iter()
        .map(|c| {
            (
                c.id.clone(),
                (
                    c.passed,
                    stage(&c),
                    c.first_failure.map(|f| f.detail).unwrap_or_default(),
                ),
            )
        })
        .collect();
    for (id, want) in [
        ("resolution", Stage::Resolution),
        ("discovery", Stage::Discovery),
        ("planning", Stage::Planning),
        ("authority", Stage::Authority),
        ("execution", Stage::Execution),
        ("verification", Stage::Verification),
        ("diagnostics", Stage::Verification),
        ("interface", Stage::Interface),
    ] {
        let (passed, st, detail) = &got[id];
        assert!(!passed, "{id} should fail");
        assert_eq!(*st, Some(want), "{id}: {detail}");
        assert!(!detail.is_empty(), "{id}: a concise diagnostic is recorded");
    }
}

#[test]
fn exit_zero_without_task_verification_is_not_a_pass() {
    need_make!();
    // `make test` exits 0, but with no expectation nothing verifies the task.
    let c = with(
        case("run-tests"),
        json!({"id": "unverified", "args": [], "variants": []}),
    );
    let r = run(vec![c]).remove(0);
    assert!(!r.passed);
    let a = r.attempt.unwrap();
    assert_eq!(a.process_exit_zero, Some(true), "the process did succeed");
    assert_eq!(a.verification, Some(false), "but the task was not verified");
    assert_eq!(a.verification_status.as_deref(), Some("unverified"));
    assert_eq!(r.first_failure.unwrap().stage, Stage::Verification);
}

#[test]
fn unintended_side_effects_are_detected_and_planning_changes_nothing() {
    need_make!();
    // `make build` creates out/: allowed only when the case says so.
    let mut strict = case("build-check");
    strict["id"] = json!("strict");
    strict["side_effects_allowed"] = json!([]);
    strict["variants"] = json!([]);
    let mut ok = case("build-check");
    ok["variants"] = json!([]);
    let rs = run(vec![strict, ok]);
    let a = rs[0].attempt.as_ref().unwrap();
    assert!(!rs[0].passed);
    assert!(
        a.unintended_side_effects
            .iter()
            .any(|p| p.starts_with("out")),
        "{:?}",
        a.unintended_side_effects
    );
    assert_eq!(
        rs[0].first_failure.as_ref().unwrap().stage,
        Stage::Execution
    );
    assert!(rs[1].passed, "{:?}", rs[1].first_failure);
    assert!(rs[1]
        .attempt
        .as_ref()
        .unwrap()
        .unintended_side_effects
        .is_empty());
    // Plan-only and unapproved attempts leave the project exactly as it was.
    let plan_only = with(
        case("run-tests"),
        json!({"id": "plan-only", "execute": false, "variants": [], "expect": {"process": "not_run", "task": "not_run", "output_contains": []}}),
    );
    let r = run(vec![plan_only]).remove(0);
    assert!(r.passed, "{:?}", r.first_failure);
    let a = r.attempt.unwrap();
    assert!(!a.executed && a.unintended_side_effects.is_empty() && a.unauthorized_executions == 0);
    assert_eq!(
        a.approval,
        Some(true),
        "approval was exercised: unapproved execution was refused"
    );
}

#[test]
fn stale_and_ineligible_workflows_are_handled_not_promoted() {
    need_make!();
    let rs = run(vec![
        case("reuse-after-unrelated-change"),
        case("reuse-after-recipe-change"),
        case("reuse-after-target-removed"),
        case("reuse-in-another-project"),
        // History that never was verified must not be reused at all.
        with(
            case("reuse-after-unrelated-change"),
            json!({"id": "unverified-prior", "args": [], "variants": []}),
        ),
    ]);
    let by = |id: &str| rs.iter().find(|c| c.id == id).unwrap();
    let a = by("reuse-after-unrelated-change").attempt.as_ref().unwrap();
    assert_eq!(
        (
            by("reuse-after-unrelated-change").passed,
            a.validity.as_deref()
        ),
        (true, Some("validated"))
    );
    assert!(
        a.executed && a.verification == Some(true),
        "validated history led to a fresh, approved, verified run"
    );

    for id in ["reuse-after-recipe-change", "reuse-in-another-project"] {
        let a = by(id).attempt.as_ref().unwrap();
        assert!(by(id).passed, "{id}: {:?}", by(id).first_failure);
        assert_eq!(a.validity.as_deref(), Some("stale"), "{id}");
        assert_eq!(
            a.stale_handling,
            Some(true),
            "{id}: stale, yet re-planned from the current state"
        );
        assert!(
            a.executed,
            "{id}: the *fresh* plan ran after fresh approval, not the stale workflow"
        );
    }
    let removed = by("reuse-after-target-removed").attempt.as_ref().unwrap();
    assert!(by("reuse-after-target-removed").passed);
    assert_eq!(
        (
            removed.validity.as_deref(),
            removed.stale_handling,
            removed.executed
        ),
        (Some("stale"), Some(true), false)
    );
    assert_ne!(
        removed.plan_status, "planned",
        "no plan from a workflow whose task is gone"
    );

    let u = by("unverified-prior");
    assert!(!u.passed);
    let f = u.first_failure.as_ref().unwrap();
    assert_eq!(f.stage, Stage::History);
    assert!(f.detail.contains("no verified prior"), "{}", f.detail);
}

#[test]
fn history_never_costs_safety_in_the_comparison() {
    need_make!();
    let mut c = case("run-tests");
    c["variants"] = json!([
        "execute the test suite",
        "kick off the specs",
        "make sure the tests pass"
    ]);
    let r = run_report(vec![c]);
    let v = &r.comparison;
    assert_eq!(v.fresh_deterministic.pairs, 3);
    for a in [&v.fresh_deterministic, &v.history_replan] {
        assert_eq!(
            (
                a.invalid_plans,
                a.unauthorized_executions,
                a.incorrect_verification,
                a.unintended_side_effects
            ),
            (0, 0, 0, 0)
        );
    }
    assert!(
        v.model.is_none(),
        "the model is only compared when asked for and ready"
    );
    assert!(v.model_status.starts_with("not run"));
    // Retrieval misses are reported as misses (stage history), not hidden or papered over.
    let missed: Vec<_> = r.cases[0]
        .comparisons
        .iter()
        .filter(|p| p.history_replan.retrieval == Some(false))
        .collect();
    for p in missed {
        assert_eq!(
            p.history_replan.failure.as_ref().unwrap().stage,
            Stage::History
        );
    }
}

#[test]
fn matching_improves_on_the_baseline_without_letting_similarity_decide_safety() {
    let tuned =
        cli2::eval::matching::run_matching(&repo_root().join("evals/matching-v1.jsonl")).unwrap();
    let again =
        cli2::eval::matching::run_matching(&repo_root().join("evals/matching-v1.jsonl")).unwrap();
    assert_eq!(tuned.fingerprint, again.fingerprint, "deterministic");
    let (b, c) = (&tuned.baseline, &tuned.current);
    assert!(
        c.recall.passed > b.recall.passed,
        "{:?} vs {:?}",
        c.recall,
        b.recall
    );
    assert!(c.false_positives.passed < b.false_positives.passed);
    assert_eq!(
        c.false_positives.passed, 0,
        "no different-action or unsafe request is matched on the tuned set"
    );
    // Specific, named behaviours.
    let get = |id: &str| c.cases.iter().find(|x| x.id == id).unwrap();
    assert!(get("para-search-1").missed.is_empty() && get("para-search-2").missed.is_empty());
    for id in [
        "act-delete",
        "act-write",
        "act-unpublish",
        "unsafe-1",
        "unsafe-2",
        "unsafe-3",
        "none-1",
        "none-2",
    ] {
        assert!(
            get(id).retrieved.is_empty(),
            "{id}: {:?}",
            get(id).retrieved
        );
    }
    // Context and staleness are for validation: retrieval still surfaces them.
    assert!(get("ctx-same-words").missed.is_empty() && get("ctx-other-project").missed.is_empty());

    // Held-out wording: improvement is smaller, and the remaining gaps are reported, not hidden.
    let held =
        cli2::eval::matching::run_matching(&repo_root().join("evals/matching-heldout-v1.jsonl"))
            .unwrap();
    assert!(held.current.recall.passed >= held.baseline.recall.passed);
    assert!(held.current.false_positives.passed <= held.baseline.false_positives.passed);
    assert!(
        held.current.cases.iter().any(|c| !c.passed),
        "the held-out set documents real remaining gaps"
    );
}

#[test]
fn running_the_suite_leaves_the_repository_and_fixtures_untouched() {
    need_make!();
    let before = fixtures_state();
    let tmp = tempfile::tempdir().unwrap();
    let report = run_suite(
        &repo_root(),
        &repo_root().join("evals/workflows-v1.jsonl"),
        &SuiteOptions {
            only: Some("build-check".into()),
            out_dir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        report.cases[0].passed,
        "{:?}",
        report.cases[0].first_failure
    );
    // The side effect (out/) happened in the sandbox, which is retained as evidence.
    let sandbox = tmp.path().join("cases/build-check/main");
    assert!(sandbox.join("project/out/artifact.txt").exists());
    assert!(
        sandbox.join("home/evidence/journal.db").exists(),
        "evidence is retained"
    );
    assert!(!repo_root().join("fixtures/dev-project/out").exists());
    assert_eq!(
        before,
        fixtures_state(),
        "fixtures are exactly as they were"
    );
}

fn fixtures_state() -> Vec<(String, u64)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push((
                    p.display().to_string(),
                    std::fs::metadata(&p).unwrap().len(),
                ));
            }
        }
    }
    let mut v = Vec::new();
    walk(&repo_root().join("fixtures"), &mut v);
    v.sort();
    v
}

#[test]
fn the_documented_command_writes_a_reproducible_report() {
    need_make!();
    let env = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    let out: PathBuf = tmp.path().join("eval");
    let proj = env.work.clone();
    let run_once = |dir: &PathBuf| {
        let r = env.run(
            &proj,
            &[
                "--json",
                "eval",
                "run",
                "--repo",
                repo_root().to_str().unwrap(),
                "--only",
                "run-tests",
                "--matching",
                "none",
                "--out",
                dir.to_str().unwrap(),
            ],
        );
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.json()
    };
    let a = run_once(&out);
    let b = run_once(&tmp.path().join("eval2"));
    assert_eq!(a["results_fingerprint"], b["results_fingerprint"]);
    let saved: Value =
        serde_json::from_slice(&std::fs::read(out.join("report.json")).unwrap()).unwrap();
    assert_eq!(saved["results_fingerprint"], a["results_fingerprint"]);
    assert_eq!(saved["schema_version"], "cli2.eval_report.v1");
    assert_eq!(saved["cases"][0]["id"], "run-tests");
    assert!(saved["cases"][0]["evidence_dir"].is_string());
    let human = env
        .run(
            &proj,
            &[
                "eval",
                "run",
                "--repo",
                repo_root().to_str().unwrap(),
                "--only",
                "run-tests",
                "--matching",
                "none",
            ],
        )
        .stdout;
    assert!(
        human.contains("no combined score") && human.contains("plan validity"),
        "{human}"
    );
}

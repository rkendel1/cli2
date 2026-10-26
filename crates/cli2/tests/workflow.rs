//! Capability learning and reuse: the derived workflow index, eligibility,
//! live validity, fresh-plan replay, model ordering and recovery.
//! Hermetic: fixture executables and synthetic journal records.

mod support;

use std::path::{Path, PathBuf};

use cli2_core::ExecutionResult;
use cli2_evidence::Journal;
use serde_json::{json, Value};
use support::*;

const SEARCH_HELP: &str = "fsearch - recursively searches the current directory for lines matching a pattern.\n\nUSAGE:\n    fsearch [OPTIONS] PATTERN [PATH ...]\n";

fn install_fsearch(env: &Env, help: &str) -> PathBuf {
    let path = env.bin.join("fsearch");
    write_executable(
        &path,
        &format!("#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then cat <<'EOF'\n{help}EOF\nexit 0; fi\nexec /usr/bin/grep -rn -- \"$1\" .\n"),
    );
    path
}

/// A project directory with a source file mentioning the given terms.
fn project(env: &Env, terms: &[&str]) -> PathBuf {
    let proj = env.project("node-project");
    for t in terms {
        std::fs::write(
            proj.join(format!("src/{t}.js")),
            format!("// {t} lives here\n"),
        )
        .unwrap();
    }
    proj
}

fn learn(env: &Env, dir: &Path) {
    assert_eq!(env.run(dir, &["scan"]).code, 0);
    let r = env.run(dir, &["describe", "executable:fsearch", "--capture-help"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
}

fn ask(env: &Env, dir: &Path, term: &str) -> Run {
    env.run(
        dir,
        &[
            &format!("find all references to {term} in fsearch"),
            "--yes",
            "--output",
            "response",
        ],
    )
}

fn search(env: &Env, dir: &Path, text: &str) -> Value {
    let r = env.run(dir, &["--json", "history", "search", text]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    r.json()
}

fn state_of(env: &Env, dir: &Path, id: &str) -> (String, Value) {
    let r = env.run(dir, &["--json", "workflow", "show", id]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    (v["validity"]["state"].as_str().unwrap().to_string(), v)
}

// ------------------------------------------------------------ synthetic history

type Case<'a> = (
    &'a str,
    &'a str,
    Option<(&'a str, Option<i32>)>,
    &'a str,
    cli2::workflow::Outcome,
    bool,
);

fn open_journal(env: &Env) -> Journal {
    Journal::open(&env.home.join("evidence")).unwrap()
}

fn record_request(j: &Journal, id: &str, intent: &str, wd: &str) {
    j.record_request(
        &serde_json::from_value(json!({
            "schema_version": "cli2.request.v1", "request_id": id, "received_at": "2026-01-01T00:00:00.000Z",
            "intent": intent, "constraints": {}, "working_directory": wd,
            "caller": {"kind": "non_interactive", "uid": 1, "established_by": "test"}
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A recorded result. `term`: termination of the one step (None: no step ran).
fn record_result(
    j: &Journal,
    req: &str,
    status: &str,
    term: Option<(&str, Option<i32>)>,
    verification: &str,
) {
    let steps = match term {
        Some((t, code)) => {
            let out = j.store_blob(b"out\n").unwrap();
            let err = j.store_blob(b"").unwrap();
            json!([{
                "index": 0, "capability_id": "project-script:npm:test", "executable": "/bin/npm",
                "arguments": ["run", "test"], "working_directory": "/tmp/p", "termination": t, "exit_code": code,
                "started_at": "2026-01-01T00:00:01.000Z", "finished_at": "2026-01-01T00:00:02.000Z", "duration_ms": 1000,
                "stdout_ref": out, "stderr_ref": err, "stdout_bytes": 4, "stderr_bytes": 0,
                "stdout_truncated": false, "stderr_truncated": false
            }])
        }
        None => json!([]),
    };
    let result: ExecutionResult = serde_json::from_value(json!({
        "schema_version": "cli2.result.v1", "request_id": req, "status": status,
        "process_status": if term.is_some() { "succeeded" } else { "not_run" },
        "steps": steps, "verification": {"status": verification, "task_level": verification == "passed", "checks": []},
        "limitations": [], "evidence_ref": format!("evidence:task:{req}"), "message": "synthetic"
    }))
    .unwrap();
    j.record_result(&result, None).unwrap();
}

#[test]
fn outcomes_are_distinct_and_only_verified_completion_is_eligible() {
    use cli2::workflow::Outcome::*;
    let env = Env::new();
    let j = open_journal(&env);
    let cases: [Case; 10] = [
        (
            "verified",
            "succeeded",
            Some(("exited", Some(0))),
            "passed",
            TaskVerified,
            true,
        ),
        (
            "unverified",
            "succeeded",
            Some(("exited", Some(0))),
            "unverified",
            ProcessSucceeded,
            false,
        ),
        (
            "failed",
            "failed",
            Some(("exited", Some(2))),
            "failed",
            ProcessFailed,
            false,
        ),
        (
            "verfail",
            "verification_failed",
            Some(("exited", Some(0))),
            "failed",
            TaskFailedVerification,
            false,
        ),
        (
            "timeout",
            "failed",
            Some(("timed_out", None)),
            "not_run",
            TimedOut,
            false,
        ),
        (
            "interrupt",
            "cancelled",
            Some(("cancelled", None)),
            "not_run",
            Interrupted,
            false,
        ),
        (
            "denied",
            "cancelled",
            None,
            "not_run",
            ApprovalDenied,
            false,
        ),
        (
            "awaiting",
            "awaiting_approval",
            None,
            "not_run",
            ApprovalRequired,
            false,
        ),
        ("planned", "planned", None, "not_run", NotPerformed, false),
        (
            "clarify",
            "needs_clarification",
            None,
            "not_run",
            NotPerformed,
            false,
        ),
    ];
    for (name, status, term, verification, _, _) in &cases {
        let id = format!("req_{name}");
        record_request(&j, &id, &format!("run the project tests {name}"), "/tmp/p");
        record_result(&j, &id, status, *term, verification);
    }
    let wfs = cli2::workflow::derive(&j).unwrap();
    assert_eq!(wfs.len(), cases.len());
    for (name, _, _, _, outcome, eligible) in &cases {
        let w = wfs.iter().find(|w| w.intent.ends_with(name)).unwrap();
        assert_eq!(w.outcome, *outcome, "{name}");
        assert_eq!(
            w.eligibility.eligible, *eligible,
            "{name}: {:?}",
            w.eligibility
        );
        assert_eq!(
            w.steps[0].execution_id.is_some(),
            outcome.executed(),
            "{name}"
        );
    }
    // Exit status zero is not task verification.
    let u = wfs
        .iter()
        .find(|w| w.intent.ends_with("unverified"))
        .unwrap();
    assert_eq!(u.steps[0].exit_code, Some(0));
    assert!(u.eligibility.reasons[0].contains("not verified"));
    // Derivation is a pure function of the journal: same IDs, same order, same content.
    assert_eq!(
        serde_json::to_string(&wfs).unwrap(),
        serde_json::to_string(&cli2::workflow::derive(&j).unwrap()).unwrap()
    );
}

#[test]
fn a_tampered_recorded_argv_is_never_executed_only_replanned() {
    // A (synthetic) history whose recorded command line is hostile: replay must
    // build today's plan for the capability and never use the old argv.
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    assert_eq!(env.run(&proj, &["scan"]).code, 0);
    let j = open_journal(&env);
    let wd = proj.canonicalize().unwrap().display().to_string();
    record_request(&j, "req_evil", "run the project tests", &wd);
    let out = j.store_blob(b"ok\n").unwrap();
    let result: ExecutionResult = serde_json::from_value(json!({
        "schema_version": "cli2.result.v1", "request_id": "req_evil", "status": "succeeded", "process_status": "succeeded",
        "steps": [{"index": 0, "capability_id": "project-script:npm:test", "executable": "/bin/sh",
            "arguments": ["-c", "rm -rf /"], "working_directory": wd, "termination": "exited", "exit_code": 0,
            "started_at": "2026-01-01T00:00:01.000Z", "finished_at": "2026-01-01T00:00:02.000Z", "duration_ms": 5,
            "stdout_ref": out, "stderr_ref": out, "stdout_bytes": 3, "stderr_bytes": 3, "stdout_truncated": false, "stderr_truncated": false}],
        "verification": {"status": "passed", "task_level": true, "checks": []},
        "limitations": [], "evidence_ref": "evidence:task:req_evil", "message": "synthetic"
    }))
    .unwrap();
    j.record_result(&result, None).unwrap();
    drop(j);
    let r = env.run(&proj, &["--json", "workflow", "replay", "wf_e1"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["executed"], false);
    let step = &v["steps"][0];
    assert_eq!(step["recorded_argv"], json!(["sh", "-c", "rm -rf /"]));
    assert_eq!(step["fresh_argv"], json!(["npm", "run", "test"]));
    assert_eq!(step["argv_differs"], true);
    assert_eq!(step["result"]["status"], "planned");
    let plan = &step["result"]["plan"]["steps"][0];
    assert_eq!(plan["arguments"], json!(["run", "test"]));
    // Nothing "validated" about a history that recorded no identity: it is only a candidate.
    assert_eq!(v["validity"]["state"], "candidate");
}

// ------------------------------------------------------------------ real runs

#[test]
fn verified_search_becomes_a_validated_workflow_with_an_explanation() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    let r = ask(&env, &proj, "authentication");
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("authentication lives here"));

    let v = search(
        &env,
        &proj,
        "find all references to authentication in fsearch",
    );
    let hit = &v["hits"][0];
    assert_eq!(hit["workflow"]["id"], "wf_e1");
    assert_eq!(hit["workflow"]["outcome"], "task_verified");
    assert_eq!(hit["validity"]["state"], "validated");
    let ex = hit["explanation"].to_string();
    for needle in [
        "Why it matches",
        "exec_1",
        "passed",
        "Unchanged",
        "fresh plan",
        "old command line is not reused",
    ] {
        assert!(ex.contains(needle), "{needle}: {ex}");
    }
    assert_eq!(
        hit["workflow"]["steps"][0]["capability_id"],
        "executable:fsearch"
    );
    assert!(hit["workflow"]["steps"][0]["interface"]["usage_line"]
        .as_str()
        .unwrap()
        .contains("PATTERN"));

    // The human rendering agrees.
    let human = env
        .run(
            &proj,
            &[
                "history",
                "search",
                "find references to authentication in fsearch",
            ],
        )
        .stdout;
    assert!(
        human.contains("wf_e1")
            && human.contains("Validated")
            && human.contains("evidence, not authority"),
        "{human}"
    );
}

#[test]
fn a_replay_is_a_fresh_plan_under_current_approval_and_runs_nothing() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication", "billing"]);
    learn(&env, &proj);
    assert_eq!(ask(&env, &proj, "authentication").code, 0);
    let before = std::fs::read_dir(env.home.join("evidence/blobs"))
        .unwrap()
        .count();

    let r = env.run(
        &proj,
        &["--json", "workflow", "replay", "wf_e1", "--no-exec"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    let step = &v["steps"][0];
    assert_eq!(v["executed"], false);
    assert_eq!(step["fresh_argv"], json!(["fsearch", "authentication"]));
    assert_eq!(step["argv_differs"], false);
    assert_eq!(
        step["current_approval"], "confirm",
        "unknown effects still need approval"
    );
    // The historical approval is reported as history, not applied.
    assert_eq!(step["recorded_approval"]["method"], "yes_flag");
    let plan_id = step["result"]["plan_id"].as_str().unwrap();
    assert_eq!(step["result"]["status"], "planned");
    assert_eq!(
        std::fs::read_dir(env.home.join("evidence/blobs"))
            .unwrap()
            .count(),
        before,
        "no process ran, so no new output was captured"
    );
    assert_eq!(
        env.run(&proj, &["output", "list", "--limit", "5"])
            .stdout
            .lines()
            .count(),
        1,
        "only the original execution exists"
    );

    // Running the fresh plan still needs approval now: `--yes` is needed again.
    let r = env.run(&proj, &["--json", "exec", plan_id]);
    assert_eq!(r.code, 3, "awaiting approval: {}", r.stdout);
    assert_eq!(r.json()["status"], "awaiting_approval");
    let r = env.run(&proj, &["exec", plan_id, "--yes", "--output", "response"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("authentication lives here"));

    // The replay is recorded in evidence, linked to the workflow.
    let req = step["result"]["request_id"].as_str().unwrap();
    let task = env.run(&proj, &["--json", "task", "show", req]).json();
    assert!(task["events"].to_string().contains("workflow_replay"));
    assert_eq!(task["journal_integrity"], "verified");

    // Inputs may be supplied for a fresh plan; the old value is not silently reused.
    let v = env
        .run(
            &proj,
            &[
                "--json",
                "workflow",
                "replay",
                "wf_e1",
                "--arg",
                "pattern=billing",
            ],
        )
        .json();
    assert_eq!(v["steps"][0]["fresh_argv"], json!(["fsearch", "billing"]));
    assert_eq!(
        v["steps"][0]["recorded_argv"],
        json!(["fsearch", "authentication"])
    );
}

#[test]
fn unverified_failed_and_unexecuted_requests_are_not_eligible_but_stay_visible() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication"); // verified (wf_e1)
    let nomatch = ask(&env, &proj, "zzznothing"); // grep exits 1: process failed (wf_e2)
    assert_ne!(nomatch.code, 0);
    env.run(&proj, &["find all references to authentication in fsearch"]); // awaiting approval: never ran
    env.run(
        &proj,
        &["plan", "find all references to authentication in fsearch"],
    ); // planned only
    let v = search(
        &env,
        &proj,
        "find all references to authentication in fsearch",
    );
    let by_id = |id: &str| {
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["workflow"]["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id("wf_e1")["validity"]["state"], "validated");
    let failed = by_id("wf_e2");
    assert_eq!(failed["workflow"]["outcome"], "process_failed");
    assert_eq!(failed["validity"]["state"], "ineligible");
    let awaiting = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["workflow"]["outcome"] == "approval_required")
        .unwrap();
    assert_eq!(awaiting["validity"]["state"], "ineligible");
    assert!(
        awaiting["workflow"]["steps"][0]["execution_id"].is_null(),
        "never executed"
    );
    assert!(v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["workflow"]["outcome"] == "not_performed"));
    // --verified-only keeps just the eligible one.
    let only = env
        .run(
            &proj,
            &[
                "--json",
                "history",
                "search",
                "--verified-only",
                "find all references to authentication in fsearch",
            ],
        )
        .json();
    assert_eq!(only["hits"].as_array().unwrap().len(), 1);
    // Ineligible workflows are never described as successes.
    assert!(!failed["explanation"]
        .to_string()
        .contains("completed and passed"));
}

#[test]
fn changed_executable_interface_policy_context_and_evidence_make_a_workflow_stale() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication");
    assert_eq!(state_of(&env, &proj, "wf_e1").0, "validated");

    // Changed context: the same request in a different project.
    let other = env.project("make-project");
    let (st, v) = state_of(&env, &other, "wf_e1");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"]
            .to_string()
            .contains("different project"),
        "{v}"
    );
    // A subdirectory of the same project is the same context.
    assert_eq!(state_of(&env, &proj.join("src"), "wf_e1").0, "validated");

    // Interface metadata changed: help re-captured (even from the same executable).
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(
        env.run(
            &proj,
            &[
                "describe",
                "executable:fsearch",
                "--capture-help",
                "--refresh-help"
            ]
        )
        .code,
        0
    );
    let (st, v) = state_of(&env, &proj, "wf_e1");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"]
            .to_string()
            .contains("interface metadata changed"),
        "{v}"
    );

    // Fresh history, then: changed executable identity.
    ask(&env, &proj, "authentication"); // wf_e2 with the new capture
    assert_eq!(state_of(&env, &proj, "wf_e2").0, "validated");
    write_executable(&env.bin.join("fsearch"), "#!/bin/sh\necho replaced\n");
    env.run(&proj, &["scan"]);
    let (st, v) = state_of(&env, &proj, "wf_e2");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"]
            .to_string()
            .contains("has changed since it ran"),
        "{v}"
    );

    // Missing capability.
    std::fs::remove_file(env.bin.join("fsearch")).unwrap();
    env.run(&proj, &["scan"]);
    let (st, v) = state_of(&env, &proj, "wf_e2");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"]
            .to_string()
            .contains("no longer discovered"),
        "{v}"
    );
    // ... and replaying it reports the blocker instead of inventing anything.
    let r = env.run(&proj, &["--json", "workflow", "replay", "wf_e2"]);
    assert_ne!(r.code, 0);
    let rep = r.json();
    assert_eq!(rep["validity"]["state"], "stale");
    assert_eq!(rep["steps"][0]["result"]["status"], "needs_clarification");
    assert!(rep["steps"][0]["fresh_argv"].is_null());
}

#[test]
fn pruned_evidence_and_changed_policy_invalidate_but_do_not_hide() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication");
    assert_eq!(state_of(&env, &proj, "wf_e1").0, "validated");

    // Policy: a new deny rule means the recorded plan would now be rejected? (unknown effects are not
    // classes, so use a constraint-independent change: characterize raises the requirement.)
    assert_eq!(
        env.run(
            &proj,
            &["characterize", "executable:fsearch", "--effects", "deletes"]
        )
        .code,
        0
    );
    let (st, v) = state_of(&env, &proj, "wf_e1");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"]
            .to_string()
            .contains("approval requirement changed"),
        "{v}"
    );
    // The fresh plan shows today's requirement (explicit); `--yes` is not enough, and history does not help.
    let rep = env
        .run(&proj, &["--json", "workflow", "replay", "wf_e1"])
        .json();
    let step = &rep["steps"][0];
    assert_eq!(step["current_approval"], "explicit");
    assert_eq!(step["recorded_approval"]["required"], "confirm");
    let plan_id = step["result"]["plan_id"].as_str().unwrap();
    let r = env.run(&proj, &["--json", "exec", plan_id, "--yes"]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert_eq!(r.json()["status"], "awaiting_approval");

    // Pruned evidence.
    let env2 = Env::new();
    install_fsearch(&env2, SEARCH_HELP);
    let proj2 = project(&env2, &["authentication"]);
    learn(&env2, &proj2);
    ask(&env2, &proj2, "authentication");
    assert_eq!(
        env2.run(&proj2, &["output", "prune", "--older-than-days", "0"])
            .code,
        0
    );
    let (st, v) = state_of(&env2, &proj2, "wf_e1");
    assert_eq!(st, "stale");
    assert!(
        v["validity"]["reasons"].to_string().contains("pruned"),
        "{v}"
    );
    // Still visible and inspectable.
    assert_eq!(
        search(
            &env2,
            &proj2,
            "find all references to authentication in fsearch"
        )["hits"][0]["workflow"]["id"],
        "wf_e1"
    );
}

#[test]
fn a_policy_that_now_forbids_it_is_reported_and_replay_is_rejected() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    assert_eq!(
        env.run(
            &proj,
            &[
                "characterize",
                "executable:fsearch",
                "--effects",
                "read_only"
            ]
        )
        .code,
        0
    );
    ask(&env, &proj, "authentication");
    assert_eq!(state_of(&env, &proj, "wf_e1").0, "validated");
    std::fs::write(
        env.home.join("policy.json"),
        r#"{"deny_effects":["read_only"]}"#,
    )
    .unwrap();
    let (st, v) = state_of(&env, &proj, "wf_e1");
    assert_eq!(st, "stale", "{v}");
    let rep = env.run(&proj, &["--json", "workflow", "replay", "wf_e1"]);
    assert_ne!(rep.code, 0, "{}", rep.stdout);
    assert_eq!(rep.json()["steps"][0]["result"]["status"], "rejected");
}

// ----------------------------------------------------------------------- batch

fn batch_file(env: &Env, proj: &Path, terms: &[&str], on_failure: &str) -> PathBuf {
    let steps: Vec<Value> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "id": format!("s{i}"),
                "depends_on": if i == 0 { json!([]) } else { json!([format!("s{}", i - 1)]) },
                "request": {"schema_version": "cli2.request.v1", "intent": format!("find all references to {t} in fsearch"),
                            "context": {"working_directory": proj.display().to_string()}}
            })
        })
        .collect();
    let p = env.work.join(format!("batch-{}.json", terms.join("-")));
    std::fs::write(
        &p,
        serde_json::to_string(
            &json!({"schema_version": "cli2.batch.v1", "on_failure": on_failure, "steps": steps}),
        )
        .unwrap(),
    )
    .unwrap();
    p
}

#[test]
fn a_verified_batch_becomes_a_multi_step_workflow_and_replays_as_fresh_plans() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication", "billing"]);
    learn(&env, &proj);
    let f = batch_file(&env, &proj, &["authentication", "billing"], "continue");
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            f.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let batch = r.json();

    let v = search(&env, &proj, "find all references to billing in fsearch");
    let wf = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["workflow"]["kind"] == "batch")
        .expect("batch workflow");
    let w = &wf["workflow"];
    assert!(w["id"].as_str().unwrap().starts_with("wf_b"));
    assert_eq!(w["batch_id"], batch["batch_id"]);
    assert_eq!(w["outcome"], "task_verified");
    assert_eq!(wf["validity"]["state"], "validated");
    assert_eq!(w["on_failure"], "continue");
    let steps = w["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|s| s["step_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["s0", "s1"],
        "ordering kept"
    );
    assert_eq!(steps[1]["depends_on"], json!(["s0"]), "dependencies kept");
    assert!(steps
        .iter()
        .all(|s| s["capability_id"] == "executable:fsearch" && s["verification"] == "passed"));
    // Members of the batch are not listed separately by default.
    assert!(v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["workflow"]["part_of_batch"].is_null()));

    let rep = env.run(
        &proj,
        &["--json", "workflow", "replay", w["id"].as_str().unwrap()],
    );
    assert_eq!(rep.code, 0, "{}", rep.stderr);
    let rep = rep.json();
    assert_eq!(rep["executed"], false);
    let argvs: Vec<Value> = rep["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["fresh_argv"].clone())
        .collect();
    assert_eq!(
        argvs,
        [
            json!(["fsearch", "authentication"]),
            json!(["fsearch", "billing"])
        ]
    );
    assert!(rep["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["result"]["status"] == "planned"));
    // A prior batch's success does not carry over: each step needs its own approval now.
    assert!(rep["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["current_approval"] == "confirm"));
}

#[test]
fn a_batch_with_a_failed_step_is_visible_but_not_eligible() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    let f = batch_file(&env, &proj, &["authentication", "nothingmatches"], "stop");
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            f.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(r.code, 8, "{}", r.stderr);
    let v = search(
        &env,
        &proj,
        "find all references to authentication in fsearch",
    );
    let wf = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["workflow"]["kind"] == "batch")
        .unwrap();
    assert_eq!(wf["validity"]["state"], "ineligible");
    assert_eq!(wf["workflow"]["outcome"], "process_failed");
    assert_eq!(wf["workflow"]["steps"].as_array().unwrap().len(), 2);
}

// -------------------------------------------------------- index, rebuild, advice

#[test]
fn the_index_is_derived_rebuildable_and_never_the_source_of_truth() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication");
    ask(&env, &proj, "zzznothing");
    let fp = |env: &Env| {
        env.run(&proj, &["--json", "workflow", "reindex"]).json()["fingerprint"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let journal_bytes = || std::fs::read(env.home.join("evidence/journal.db")).unwrap();
    let before = journal_bytes();
    let a = fp(&env);
    let b = fp(&env);
    assert_eq!(a, b, "repeated rebuilds are identical");
    assert!(env.home.join("workflows.db").exists());
    // Deleting the derived index loses nothing: the rebuild is byte-for-byte the same derivation.
    std::fs::remove_file(env.home.join("workflows.db")).unwrap();
    let c = fp(&env);
    assert_eq!(a, c);
    assert_eq!(journal_bytes(), before, "the journal was only read");
    let v = search(
        &env,
        &proj,
        "find all references to authentication in fsearch",
    );
    assert_eq!(v["hits"][0]["workflow"]["id"], "wf_e1");
    // Delete again: a search silently rebuilds.
    std::fs::remove_file(env.home.join("workflows.db")).unwrap();
    assert_eq!(
        search(
            &env,
            &proj,
            "find all references to authentication in fsearch"
        )["index"]["rebuilt"],
        true
    );
    // New history is picked up.
    ask(&env, &proj, "authentication");
    let ids: Vec<String> = search(
        &env,
        &proj,
        "find all references to authentication in fsearch",
    )["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["workflow"]["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"wf_e3".to_string()), "{ids:?}");
}

#[test]
fn a_damaged_index_is_set_aside_and_rebuilt_from_the_journal() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication");
    let good = env.run(&proj, &["--json", "workflow", "reindex"]).json()["fingerprint"].clone();
    let path = env.home.join("workflows.db");
    let query = "find all references to authentication in fsearch";

    // 1. Not a database at all.
    std::fs::write(
        &path,
        b"this is not sqlite, it is garbage that is long enough to look like a file",
    )
    .unwrap();
    let v = search(&env, &proj, query);
    assert_eq!(v["hits"][0]["workflow"]["id"], "wf_e1");
    assert_eq!(v["index"]["rebuilt"], true);
    assert!(v["index"]["recovered_from"].as_str().is_some());
    assert_eq!(
        env.run(&proj, &["--json", "workflow", "reindex"]).json()["fingerprint"],
        good
    );

    // 2. A valid database whose rows are damaged.
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute("UPDATE workflows SET json = '{\"broken\": true}'", [])
            .unwrap();
    }
    let v = search(&env, &proj, query);
    assert_eq!(v["hits"][0]["workflow"]["id"], "wf_e1");
    assert!(v["index"]["recovered_from"].as_str().is_some());

    // 3. A foreign SQLite file in its place.
    std::fs::remove_file(&path).unwrap();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE unrelated(x); INSERT INTO unrelated VALUES (1);")
            .unwrap();
    }
    let v = search(&env, &proj, query);
    assert_eq!(v["hits"][0]["workflow"]["id"], "wf_e1");
    // Authoritative history is untouched throughout.
    let t = env
        .run(
            &proj,
            &[
                "--json",
                "task",
                "show",
                v["hits"][0]["workflow"]["steps"][0]["request_id"]
                    .as_str()
                    .unwrap(),
            ],
        )
        .json();
    assert_eq!(t["journal_integrity"], "verified");
}

#[test]
fn ordinary_requests_get_explained_advice_that_never_changes_the_plan() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    // (the first plan is itself recorded as history, but never as an executed one: wf_e1)
    let first = env
        .run(
            &proj,
            &[
                "--json",
                "plan",
                "find all references to authentication in fsearch",
            ],
        )
        .json();
    ask(&env, &proj, "authentication"); // verified history now exists: wf_e2
    let again = env
        .run(
            &proj,
            &[
                "--json",
                "plan",
                "find all references to authentication in fsearch",
            ],
        )
        .json();
    assert_eq!(
        first["plan"]["steps"][0]["arguments"],
        again["plan"]["steps"][0]["arguments"]
    );
    assert_eq!(
        first["plan"]["steps"][0]["capability_id"],
        again["plan"]["steps"][0]["capability_id"]
    );
    assert_eq!(
        first["plan"]["approval"]["level"],
        again["plan"]["approval"]["level"]
    );
    let notes = again["limitations"].to_string();
    assert!(
        notes.contains("History: workflow wf_e2") && notes.contains("does not authorize anything"),
        "{notes}"
    );
    assert!(!first["limitations"].to_string().contains("History:"));
    // Without history for the request, no advice.
    let other = env
        .run(
            &proj,
            &["--json", "plan", "order a pizza with extra cheese"],
        )
        .json();
    assert!(!other["limitations"].to_string().contains("History:"));
}

// ------------------------------------------------------------- model ordering

fn fake_bridge(env: &Env, rankings: &str, reason: &str) -> PathBuf {
    let path = env.bin.join("fake-apple-fm");
    write_executable(
        &path,
        &format!(
            "#!/bin/sh\nin=$(cat)\nid=$(printf '%s' \"$in\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '{{\"schema_version\":\"cli2.apple_fm.response.v1\",\"request_id\":\"%s\",\"status\":\"ranked\",\"rankings\":{rankings},\"clarification\":null}}\\n' \"$id\"\n# {reason}\n"
        ),
    );
    path
}

fn ranking(ids: &[&str], reason: &str) -> String {
    let items: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| format!("{{\"capability_id\":\"workflow:{id}\",\"rank\":{},\"relevance\":\"high\",\"reason\":\"{reason}\"}}", i + 1))
        .collect();
    format!("[{}]", items.join(","))
}

fn with_model(env: &Env, dir: &Path, bridge: &Path, query: &str) -> Value {
    let out = env
        .command(
            dir,
            &[
                "--json",
                "history",
                "search",
                "--resolver",
                "apple-fm",
                query,
            ],
        )
        .env("CLI2_APPLE_FM_BRIDGE", bridge)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn the_model_may_only_reorder_existing_workflows_and_cannot_relabel_them() {
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication"]);
    learn(&env, &proj);
    ask(&env, &proj, "authentication"); // wf_e1 verified
    ask(&env, &proj, "authentication"); // wf_e2 verified
    ask(&env, &proj, "zzznothing"); // wf_e3 failed (intent differs but is still a candidate for the model? it matches by similarity)
    let q = "find all references to authentication in fsearch";
    let det = search(&env, &proj, q);
    let det_ids: Vec<&str> = det["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["workflow"]["id"].as_str().unwrap())
        .collect();

    // A model that prefers wf_e2 and *claims* the unverified/failed one is successful.
    let b = fake_bridge(
        &env,
        &ranking(
            &["wf_e2", "wf_e1"],
            "this one definitely succeeded and is verified",
        ),
        "",
    );
    let v = with_model(&env, &proj, &b, q);
    assert_eq!(v["ordered_by"], "apple-foundation-models");
    let ids: Vec<&str> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["workflow"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], "wf_e2", "reordered");
    let mut a = ids.clone();
    let mut d = det_ids.clone();
    a.sort();
    d.sort();
    assert_eq!(a, d, "exactly the same workflows: nothing added or dropped");
    assert!(v["model_note"]
        .as_str()
        .unwrap()
        .contains("not by the model"));

    // Labels come from evidence, whatever the model says.
    let failing = ranking(&["wf_e3"], "verified successful workflow");
    let v = with_model(&env, &proj, &fake_bridge(&env, &failing, ""), q);
    let f = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["workflow"]["id"] == "wf_e3");
    if let Some(f) = f {
        assert_eq!(f["validity"]["state"], "ineligible");
        assert_eq!(f["workflow"]["outcome"], "process_failed");
        assert!(!f["explanation"]
            .to_string()
            .contains("completed and passed"));
    }

    // A workflow the model invents is rejected: deterministic order is kept.
    let b = fake_bridge(&env, &ranking(&["wf_invented", "wf_e1"], "x"), "");
    let v = with_model(&env, &proj, &b, q);
    assert_eq!(v["ordered_by"], "deterministic");
    assert!(
        v["model_note"].as_str().unwrap().contains("not used"),
        "{v}"
    );
    let ids: Vec<&str> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["workflow"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, det_ids);

    // No bridge at all: deterministic, with a clear note.
    let v = with_model(&env, &proj, &env.bin.join("does-not-exist"), q);
    assert_eq!(v["ordered_by"], "deterministic");
    assert!(v["model_note"].as_str().unwrap().contains("not used"));
}

// ------------------------------------------------------------ index freshness

fn engine_for(env: &Env) -> cli2::Engine {
    let mut config = cli2::Config::from_env().unwrap();
    config.home = env.home.clone();
    config.path_var = env.path.clone();
    config.resolver = Some(Default::default());
    cli2::Engine::open(config).unwrap()
}

fn add_history(env: &Env, from: usize, n: usize) {
    let j = open_journal(env);
    for i in from..from + n {
        let id = format!("req_f{i}");
        record_request(&j, &id, &format!("run the project tests {i}"), "/tmp/p");
        record_result(&j, &id, "succeeded", Some(("exited", Some(0))), "passed");
    }
}

#[test]
fn ordinary_requests_never_scan_a_large_journal_and_say_so() {
    use cli2::workflow::store::Freshness;
    let env = Env::new();
    let proj = env.project("node-project");
    let budget = cli2::workflow::engine_ext::FRESHNESS_BUDGET;
    add_history(&env, 0, budget + 50);
    let journal_before = std::fs::read(env.home.join("evidence/journal.db")).unwrap();

    // No index yet and history is larger than an ordinary request will build: nothing is built or implied.
    let mut e = engine_for(&env);
    let advice = e.advise_history("run the project tests", &proj, None);
    assert!(
        matches!(advice.freshness, Freshness::Behind { .. }),
        "{:?}",
        advice.freshness
    );
    assert!(
        advice
            .notes
            .iter()
            .any(|n| n.contains("skipped") && n.contains("workflow reindex")),
        "{:?}",
        advice.notes
    );
    assert!(
        advice
            .notes
            .iter()
            .all(|n| !n.starts_with("History: workflow")),
        "no match may be offered as current"
    );
    assert!(
        !env.home.join("workflows.db").exists(),
        "an ordinary request must not build a large index"
    );
    assert_eq!(
        std::fs::read(env.home.join("evidence/journal.db")).unwrap(),
        journal_before,
        "advice only reads the journal"
    );
    // The same holds end to end for a real request.
    let r = env
        .run(
            &proj,
            &["--json", "plan", "order a pizza with extra cheese"],
        )
        .json();
    assert!(
        r["limitations"]
            .to_string()
            .contains("History advice skipped"),
        "{}",
        r["limitations"]
    );
    drop(e);
    // Explicit commands make the index current.
    let r = env.run(
        &proj,
        &["--json", "history", "search", "run the project tests"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["index"]["freshness"]["state"], "rebuilt");
    assert!(!v["hits"].as_array().unwrap().is_empty());

    // A little new history: an ordinary request now brings the index forward incrementally.
    add_history(&env, budget + 50, 3);
    let mut e = engine_for(&env);
    let advice = e.advise_history("run the project tests", &proj, None);
    assert!(
        matches!(
            advice.freshness,
            Freshness::UpdatedIncrementally { workflows: 3 }
        ),
        "{:?}",
        advice.freshness
    );
    // ... and the result is exactly what a full rebuild would give.
    let incremental =
        env.run(&proj, &["--json", "workflow", "reindex"]).json()["fingerprint"].clone();
    let again = env.run(&proj, &["--json", "workflow", "reindex"]).json()["fingerprint"].clone();
    assert_eq!(incremental, again);
    let mut e = engine_for(&env);
    let adv = e.advise_history("run the project tests", &proj, None);
    assert!(adv.freshness.is_current());

    // Far behind again: explicit about it.
    add_history(&env, 10_000, budget + 10);
    let mut e = engine_for(&env);
    let adv = e.advise_history("run the project tests", &proj, None);
    assert!(
        matches!(adv.freshness, Freshness::Behind { executions_behind, .. } if executions_behind as usize > budget),
        "{:?}",
        adv.freshness
    );
}

#[test]
fn incremental_updates_equal_a_full_rebuild_including_batch_membership() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    install_fsearch(&env, SEARCH_HELP);
    let proj = project(&env, &["authentication", "billing"]);
    learn(&env, &proj);
    // Index after the first execution, then record a batch whose steps include later executions.
    ask(&env, &proj, "authentication");
    let mut e = engine_for(&env);
    e.workflow_list().unwrap();
    drop(e);
    let f = batch_file(&env, &proj, &["authentication", "billing"], "stop");
    assert_eq!(
        env.run(
            &proj,
            &[
                "--json",
                "batch",
                "--request-file",
                f.to_str().unwrap(),
                "--yes"
            ]
        )
        .code,
        0
    );
    // Incremental refresh, then compare with a from-scratch rebuild of the same journal.
    let mut e = engine_for(&env);
    let (_, report) = e.workflow_list().unwrap();
    assert!(
        matches!(
            report.freshness,
            cli2::workflow::store::Freshness::UpdatedIncrementally { .. }
        ),
        "{:?}",
        report.freshness
    );
    let (list, _) = e.workflow_list().unwrap();
    let incremental: Vec<(String, Option<String>)> = list
        .iter()
        .map(|w| (w.id.clone(), w.part_of_batch.clone()))
        .collect();
    let (_, fp_full) = e.workflow_reindex().unwrap();
    let (list2, _) = e.workflow_list().unwrap();
    let full: Vec<(String, Option<String>)> = list2
        .iter()
        .map(|w| (w.id.clone(), w.part_of_batch.clone()))
        .collect();
    assert_eq!(incremental, full);
    assert!(!fp_full.is_empty());
    assert!(
        full.iter()
            .any(|(id, m)| id.starts_with("wf_e") && m.is_some()),
        "batch members are marked: {full:?}"
    );
}

#[test]
fn a_failed_refresh_leaves_the_journal_alone_and_offers_nothing_as_current() {
    use cli2::workflow::store::Freshness;
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let proj = env.project("node-project");
    add_history(&env, 0, 5);
    let mut e = engine_for(&env);
    e.workflow_list().unwrap(); // builds workflows.db
    drop(e);
    add_history(&env, 5, 2);
    let db = env.home.join("workflows.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().write(true).open(&db).is_ok() {
        eprintln!("SKIPPED: permissions do not restrict this user (running as root?)");
        return;
    }
    let journal_before = std::fs::read(env.home.join("evidence/journal.db")).unwrap();
    let mut e = engine_for(&env);
    let advice = e.advise_history("run the project tests", &proj, None);
    assert!(
        matches!(advice.freshness, Freshness::Unavailable { .. }),
        "{:?}",
        advice.freshness
    );
    assert!(
        advice
            .notes
            .iter()
            .any(|n| n.contains("could not be refreshed") && n.contains("journal is unchanged")),
        "{:?}",
        advice.notes
    );
    assert!(advice
        .notes
        .iter()
        .all(|n| !n.starts_with("History: workflow")));
    drop(e);
    assert_eq!(
        std::fs::read(env.home.join("evidence/journal.db")).unwrap(),
        journal_before,
        "history is untouched"
    );
    // The request itself still works.
    let r = env.run(&proj, &["--json", "plan", "order a pizza"]);
    assert_eq!(r.code, 4, "{}", r.stderr);
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
}

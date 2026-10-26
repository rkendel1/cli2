//! First-slice acceptance tests (spec §11). Every test drives the real `cli2`
//! binary against fixture projects copied into temporary directories, with an
//! isolated data directory and a controlled PATH, executing real subprocesses.

mod support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::*;

fn step0(v: &Value) -> &Value {
    &v["steps"][0]
}

fn plan_step(v: &Value) -> &Value {
    &v["plan"]["steps"][0]
}

// --- Discovery ---------------------------------------------------------------

#[test]
fn discover_a_known_executable() {
    let env = Env::new();
    let tool = env.bin.join("fixturetool");
    write_executable(&tool, "#!/bin/sh\necho fixture\n");
    let (code, scan) = env.json(&env.work, &["scan"]);
    assert_eq!(code, 0);
    assert!(scan["environment"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "executable:fixturetool"));

    let (_, cap) = env.json(&env.work, &["describe", "executable:fixturetool"]);
    assert_eq!(cap["id"], "executable:fixturetool");
    assert_eq!(cap["kind"], "executable");
    assert_eq!(cap["location"]["path"], tool.display().to_string());
    assert!(cap["fingerprint"].as_str().unwrap().starts_with("file:v1:"));
    assert_eq!(cap["enrichment"], "discovered");
    // Unknown effects are recorded as unknown, never as read-only.
    assert_eq!(cap["effects"]["certainty"], "unknown");
    assert_valid("capability", &cap);
}

#[test]
fn discover_a_node_project() {
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, _) = env.json(&proj, &["scan"]);
    assert_eq!(code, 0);
    let (_, inv) = env.json(&proj, &["inventory", "--kind", "project_task"]);
    let caps = inv["capabilities"].as_array().unwrap();
    let ids: Vec<&str> = caps.iter().map(|c| c["id"].as_str().unwrap()).collect();
    for id in [
        "project-script:npm:test",
        "project-script:npm:build",
        "project-script:npm:lint",
        "project-script:npm:publish-package",
    ] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }
    let test = caps
        .iter()
        .find(|c| c["id"] == "project-script:npm:test")
        .unwrap();
    assert_eq!(test["kind"], "project_task");
    assert_eq!(test["interface"]["executable"], "npm");
    assert_eq!(
        test["interface"]["arguments"],
        serde_json::json!(["run", "test"])
    );
    assert_eq!(test["purpose"]["source"], "package_manifest");
    assert_eq!(test["location"]["project_root"], proj.display().to_string());
    for c in caps {
        assert_valid("capability", c);
    }
}

#[test]
fn discover_make_and_cargo_projects() {
    let env = Env::new();
    let make = env.project("make-project");
    let ids = env.inventory_ids(&make);
    for id in [
        "project-task:make:build",
        "project-task:make:test",
        "project-task:make:clean",
    ] {
        assert!(ids.contains(&id.to_string()), "{id} missing");
    }
    assert!(!ids.iter().any(|i| i.contains(".PHONY")));
    let rust = env.project("rust-project");
    let ids = env.inventory_ids(&rust);
    for id in [
        "project-task:cargo:build",
        "project-task:cargo:test",
        "project-task:cargo:run",
    ] {
        assert!(ids.contains(&id.to_string()), "{id} missing");
    }
}

#[test]
fn discover_a_new_executable_on_rescan() {
    let env = Env::new();
    env.json(&env.work, &["scan"]);
    assert!(!env
        .inventory_ids(&env.work)
        .contains(&"executable:brandnewtool".to_string()));

    write_executable(&env.bin.join("brandnewtool"), "#!/bin/sh\necho new\n");
    let (_, scan) = env.json(&env.work, &["scan"]);
    assert_eq!(
        scan["environment"]["added"],
        serde_json::json!(["executable:brandnewtool"])
    );
    assert!(env
        .inventory_ids(&env.work)
        .contains(&"executable:brandnewtool".to_string()));

    // Removal is reconciled too.
    std::fs::remove_file(env.bin.join("brandnewtool")).unwrap();
    let (_, scan) = env.json(&env.work, &["scan"]);
    assert_eq!(
        scan["environment"]["removed"],
        serde_json::json!(["executable:brandnewtool"])
    );
}

#[test]
fn changed_project_manifest_is_invalidated_and_reconciled() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    env.json(&proj, &["scan"]);
    let (_, before) = env.json(&proj, &["describe", "project-script:npm:test"]);

    // A plan made before the change...
    let (code, planned) = env.json(&proj, &["plan", "run the project tests"]);
    assert_eq!(code, 0);
    let old_plan_id = planned["plan_id"].as_str().unwrap().to_string();

    // ...then the manifest changes.
    let pj = proj.join("package.json");
    let text = std::fs::read_to_string(&pj).unwrap();
    std::fs::write(
        &pj,
        text.replace(
            "\"test\": \"node test.js\"",
            "\"test\": \"node test.js --verbose\"",
        ),
    )
    .unwrap();

    // Without an explicit rescan, the next request detects and reconciles it.
    let (_, after) = env.json(&proj, &["describe", "project-script:npm:test"]);
    assert_ne!(before["fingerprint"], after["fingerprint"]);
    assert!(after["purpose"]["value"]
        .as_str()
        .unwrap()
        .contains("--verbose"));

    // The stale plan can no longer run, even with approval.
    let digest = planned["plan"]["digest"].as_str().unwrap().to_string();
    let (code, v) = env.json(&proj, &["exec", &old_plan_id, "--approve", &digest]);
    assert_eq!(v["status"], "rejected", "{v:#}");
    assert_eq!(code, 6);
    assert!(v["rejection"].to_string().contains("redefined"));

    // An explicit scan reports the change and what was invalidated.
    std::fs::write(&pj, text).unwrap();
    let (_, scan) = env.json(&proj, &["scan"]);
    let changed = scan["project"]["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["id"], "project-script:npm:test");
}

// --- Resolution --------------------------------------------------------------

#[test]
fn resolve_a_clear_intent() {
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, v) = env.json(&proj, &["plan", "run the project tests"]);
    if !has_node() {
        // Without npm on PATH the task is not viable: the system must say so.
        assert_eq!(v["status"], "needs_clarification");
        return;
    }
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["status"], "planned");
    let step = plan_step(&v);
    assert_eq!(step["capability_id"], "project-script:npm:test");
    assert_eq!(step["executable"], "npm");
    assert_eq!(step["arguments"], serde_json::json!(["run", "test"]));
    assert_eq!(step["working_directory"], proj.display().to_string());
    // The plan references a capability that actually exists in the index.
    assert!(env
        .inventory_ids(&proj)
        .contains(&"project-script:npm:test".to_string()));
    // Candidates are ranked; the selection is explained.
    assert_eq!(
        v["plan"]["candidates"][0]["capability_id"],
        "project-script:npm:test"
    );
    assert!(!v["plan"]["selection_reason"].as_str().unwrap().is_empty());
    assert!(v["plan"]["digest"].as_str().unwrap().starts_with("sha256:"));
}

#[test]
fn resolve_an_ambiguous_intent() {
    require!(has_node() && has_make(), "node/npm and make");
    let env = Env::new();
    let proj = env.project("node-project");
    // The same project also has a Makefile with a `test` target.
    std::fs::write(proj.join("Makefile"), "test:\n\t@echo make tests passed\n").unwrap();

    let (code, v) = env.json(&proj, &["plan", "run the tests"]);
    assert_eq!(v["status"], "needs_clarification", "{v:#}");
    assert_eq!(code, 4);
    let options: Vec<&str> = v["clarification"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["capability_id"].as_str().unwrap())
        .collect();
    assert!(options.contains(&"project-script:npm:test"), "{options:?}");
    assert!(options.contains(&"project-task:make:test"), "{options:?}");
    assert!(
        v["plan_id"].is_null(),
        "no plan is made for an ambiguous intent"
    );

    // Answering the question resolves it.
    let (code, v) = env.json(
        &proj,
        &["plan", "run the tests", "--pick", "project-task:make:test"],
    );
    assert_eq!(code, 0);
    assert_eq!(plan_step(&v)["capability_id"], "project-task:make:test");

    // Naming the runner is also enough to disambiguate.
    let (_, v) = env.json(&proj, &["plan", "run the tests with make"]);
    assert_eq!(
        plan_step(&v)["capability_id"],
        "project-task:make:test",
        "{v:#}"
    );
}

#[test]
fn unknown_interfaces_are_not_guessed() {
    let env = Env::new();
    write_executable(&env.bin.join("frobnicate"), "#!/bin/sh\ntouch frobbed\n");
    let (code, v) = env.json(&env.work, &["frobnicate the widgets"]);
    assert_eq!(code, 4, "{v:#}");
    assert_eq!(v["status"], "needs_clarification");
    assert!(v["clarification"]["question"]
        .as_str()
        .unwrap()
        .contains("--capture-help"));
    assert!(!env.work.join("frobbed").exists(), "nothing may run");
}

// --- Plans, approval and policy -------------------------------------------------

#[test]
fn plan_only_request_does_not_execute() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    for args in [
        vec!["plan", "build the bundle"],
        vec!["build the bundle", "--no-exec", "--yes"],
    ] {
        let (code, v) = env.json(&proj, &args);
        assert_eq!(code, 0);
        assert_eq!(v["status"], "planned");
        assert_eq!(v["process_status"], "not_run");
        assert!(v["steps"].as_array().unwrap().is_empty());
        assert_eq!(plan_step(&v)["capability_id"], "project-script:npm:build");
        assert!(!proj.join("dist").exists(), "the build must not have run");
        let (_, task) = env.json(&proj, &["task", "show", v["request_id"].as_str().unwrap()]);
        assert!(!task["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "step_started"));
    }
}

#[test]
fn unapproved_risky_operation_is_blocked() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");

    // Non-interactive with no approval: blocked.
    let (code, v) = env.json(&proj, &["publish the package"]);
    assert_eq!(code, 3, "{v:#}");
    assert_eq!(v["status"], "awaiting_approval");
    assert_eq!(v["plan"]["approval"]["level"], "explicit");
    assert!(!proj.join("PUBLISHED").exists());

    // --yes is not explicit approval for a publishing operation.
    let (code, v) = env.json(&proj, &["publish the package", "--yes"]);
    assert_eq!(code, 3);
    assert_eq!(v["status"], "awaiting_approval");
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("--yes does not approve"));
    assert!(!proj.join("PUBLISHED").exists());

    // Explicit approval of the exact plan presented allows it.
    let plan_id = v["plan_id"].as_str().unwrap();
    let digest = v["plan"]["digest"].as_str().unwrap();
    let (code, v) = env.json(&proj, &["exec", plan_id, "--approve", digest]);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["approval"]["method"], "digest_token");
    assert!(proj.join("PUBLISHED").exists());
}

#[test]
fn hard_restrictions_cannot_be_approved() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let (code, v) = env.json(&proj, &["install-system", "--yes"]);
    assert_eq!(code, 6, "{v:#}");
    assert_eq!(v["status"], "rejected");
    assert_eq!(v["plan"]["approval"]["level"], "forbidden");
    let plan_id = v["plan_id"].as_str().unwrap();
    let digest = v["plan"]["digest"].as_str().unwrap();
    let (code, v) = env.json(&proj, &["exec", plan_id, "--approve", digest]);
    assert_eq!(code, 6);
    assert_eq!(v["status"], "rejected");
}

#[test]
fn constraints_reject_noncompliant_plans() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, v) = env.json(&proj, &["build the bundle", "--no-modify", "--yes"]);
    assert_eq!(code, 6);
    assert_eq!(v["status"], "rejected");
    assert!(!proj.join("dist").exists());

    // The same constraint via a cli2.request.v1 JSON request.
    let req = serde_json::json!({
        "schema_version": "cli2.request.v1",
        "intent": "publish the package",
        "constraints": {"network": "deny"},
        "context": {"working_directory": "."}
    });
    assert_valid("request", &req);
    let f = env.work.join("req.json");
    std::fs::write(&f, req.to_string()).unwrap();
    let (code, v) = env.json(&proj, &["--request", f.to_str().unwrap()]);
    assert_eq!(code, 6, "{v:#}");
    assert!(v["rejection"].to_string().contains("network"));
}

#[test]
fn modified_plan_cannot_use_old_approval() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let (_, v) = env.json(&proj, &["plan", "publish the package"]);
    let plan_id = v["plan_id"].as_str().unwrap().to_string();
    let digest = v["plan"]["digest"].as_str().unwrap().to_string();

    // Tamper with the recorded plan: change the invocation behind the
    // approval's back.
    let db = rusqlite::Connection::open(env.home.join("evidence/journal.db")).unwrap();
    let json: String = db
        .query_row(
            "SELECT json FROM plans WHERE plan_id = ?1",
            [&plan_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut plan: Value = serde_json::from_str(&json).unwrap();
    plan["steps"][0]["arguments"] = serde_json::json!(["run", "build"]);
    db.execute(
        "UPDATE plans SET json = ?1 WHERE plan_id = ?2",
        [plan.to_string(), plan_id.clone()],
    )
    .unwrap();
    drop(db);

    let (code, v) = env.json(&proj, &["exec", &plan_id, "--approve", &digest]);
    assert_eq!(code, 6, "{v:#}");
    assert_eq!(v["status"], "rejected");
    assert!(!proj.join("PUBLISHED").exists());
    assert!(!proj.join("dist").exists());

    // A re-planned operation gets a new digest; the old approval token does not apply.
    let (_, v2) = env.json(&proj, &["plan", "publish the package"]);
    let plan2 = v2["plan_id"].as_str().unwrap();
    assert_ne!(v2["plan"]["digest"].as_str().unwrap(), digest);
    let (code, v) = env.json(&proj, &["exec", plan2, "--approve", &digest]);
    assert_eq!(code, 3, "{v:#}");
    assert_eq!(v["status"], "awaiting_approval");
    assert!(v["message"].as_str().unwrap().contains("does not match"));
    assert!(!proj.join("PUBLISHED").exists());
}

// --- Execution and evidence ------------------------------------------------------

#[test]
fn run_a_successful_process() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, v) = env.json(&proj, &["run the project tests", "--yes"]);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["status"], "succeeded");
    assert_eq!(v["process_status"], "succeeded");
    let s = step0(&v);
    assert_eq!(s["termination"], "exited");
    assert_eq!(s["exit_code"], 0);
    assert_eq!(s["capability_id"], "project-script:npm:test");
    // Output was captured as evidence and can be read back.
    let out = env.run(&proj, &["evidence", s["stdout_ref"].as_str().unwrap()]);
    assert!(out.stdout.contains("pass 2, fail 0"), "{}", out.stdout);
    // A zero exit status alone is not reported as verified task success.
    assert_eq!(v["verification"]["status"], "unverified");
    assert_eq!(v["verification"]["task_level"], false);
}

#[test]
fn run_a_failing_process() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, v) = env.json(&proj, &["npm run test:fail", "--yes"]);
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(v["status"], "failed");
    assert_eq!(v["process_status"], "failed");
    assert_eq!(step0(&v)["exit_code"], 1);
    assert_eq!(v["verification"]["status"], "failed");
    let out = env.run(
        &proj,
        &["evidence", step0(&v)["stdout_ref"].as_str().unwrap()],
    );
    assert!(out.stdout.contains("not ok - deliberate failure"));

    // The failure is preserved in the evidence record.
    let (_, task) = env.json(&proj, &["task", "show", v["request_id"].as_str().unwrap()]);
    assert_eq!(task["results"][0]["status"], "failed");
    assert_eq!(task["journal_integrity"], "verified");
    let kinds: Vec<&str> = task["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for k in [
        "request_received",
        "plan_created",
        "approval_decision",
        "step_started",
        "step_finished",
        "verification_completed",
        "result_recorded",
    ] {
        assert!(kinds.contains(&k), "missing event {k} in {kinds:?}");
    }
}

#[test]
fn verify_a_task_postcondition() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let (code, v) = env.json(
        &proj,
        &[
            "build the bundle",
            "--yes",
            "--expect-file",
            "dist/bundle.js",
        ],
    );
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["status"], "succeeded");
    assert_eq!(v["verification"]["status"], "passed");
    assert_eq!(v["verification"]["task_level"], true);

    // Process success with a failed postcondition is reported distinctly.
    let (code, v) = env.json(
        &proj,
        &[
            "build the bundle",
            "--yes",
            "--expect-file",
            "dist/missing.js",
        ],
    );
    assert_eq!(code, 5, "{v:#}");
    assert_eq!(v["status"], "verification_failed");
    assert_eq!(v["process_status"], "succeeded");
    assert_eq!(step0(&v)["exit_code"], 0);
}

#[test]
fn manifest_derived_postcondition_for_cargo_build() {
    let env = Env::new();
    require!(std::env::var_os("CARGO").is_some(), "cargo");
    let proj = env.project("rust-project");
    let (code, v) = env.json(&proj, &["build the crate", "--yes", "--timeout", "300"]);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(plan_step(&v)["capability_id"], "project-task:cargo:build");
    assert_eq!(v["verification"]["status"], "passed");
    assert!(v["verification"]["checks"]
        .to_string()
        .contains("path_exists"));
}

#[test]
fn json_output_conforms_to_contracts() {
    require!(has_node(), "node/npm");
    let env = Env::new();
    let proj = env.project("node-project");
    let mut outputs = vec![
        env.json(&proj, &["run the project tests", "--yes"]).1,
        env.json(&proj, &["plan", "build the bundle"]).1,
        env.json(&proj, &["publish the package"]).1,
        env.json(&proj, &["the quick brown fox jumps"]).1,
        env.json(&proj, &["build the bundle", "--no-modify"]).1,
    ];
    std::fs::write(proj.join("Makefile"), "test:\n\t@echo hi\n").unwrap();
    outputs.push(env.json(&proj, &["plan", "run the tests"]).1);
    let statuses: Vec<&str> = outputs
        .iter()
        .map(|o| o["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        vec![
            "succeeded",
            "planned",
            "awaiting_approval",
            "needs_clarification",
            "rejected",
            "needs_clarification"
        ]
    );
    for o in &outputs {
        assert_eq!(o["schema_version"], "cli2.result.v1");
        assert_valid("result", o);
        if !o["plan"].is_null() {
            assert_valid("plan", &o["plan"]);
        }
    }
    let (_, task) = env.json(
        &proj,
        &["task", "show", outputs[0]["request_id"].as_str().unwrap()],
    );
    for a in task["approvals"].as_array().unwrap() {
        assert_valid("approval", a);
    }
    // Errors are machine-readable too.
    let (code, e) = env.json(&proj, &["task", "show", "req_doesnotexist"]);
    assert_eq!(code, 2);
    assert_eq!(e["error"]["code"], "not_found");
}

#[test]
fn corrupt_index_is_reported_and_rebuilt() {
    let env = Env::new();
    write_executable(&env.bin.join("fixturetool"), "#!/bin/sh\n");
    env.json(&env.work, &["scan"]);
    let db = env.home.join("index.db");

    // A record altered behind the index's back fails its checksum.
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "UPDATE capabilities SET json = replace(json, 'fixturetool', 'evil') WHERE id = 'executable:fixturetool'",
            [],
        )
        .unwrap();
    }
    let (code, v) = env.json(&env.work, &["inventory"]);
    assert_eq!(code, 9, "{v:#}");
    assert_eq!(v["error"]["code"], "index_corrupt");
    assert!(v["error"]["remedy"].as_str().unwrap().contains("--rebuild"));
    // Intents refuse to run on corrupt state as well.
    let (code, _) = env.json(&env.work, &["run fixturetool"]);
    assert_eq!(code, 9);

    // Garbage bytes are detected too.
    std::fs::write(&db, vec![0x42u8; 4096]).unwrap();
    let (code, _) = env.json(&env.work, &["inventory"]);
    assert_eq!(code, 9);

    let (code, v) = env.json(&env.work, &["scan", "--rebuild"]);
    assert_eq!(code, 0, "{v:#}");
    assert!(v["rebuilt_from"].as_str().unwrap().contains("corrupt"));
    assert!(env
        .inventory_ids(&env.work)
        .contains(&"executable:fixturetool".to_string()));
}

#[test]
fn timeout_is_recorded() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let started = Instant::now();
    let (code, v) = env.json(&proj, &["make sleepy", "--yes", "--timeout", "1"]);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(v["status"], "failed");
    assert_eq!(step0(&v)["termination"], "timed_out");
    assert!(v["message"].as_str().unwrap().contains("timed out"));
}

#[test]
fn cancellation_is_recorded() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    env.json(&proj, &["scan"]);
    let child = env
        .command(&proj, &["--json", "make sleepy", "--yes"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the process has actually started (step_started recorded).
    let started = Instant::now();
    let journal = env.home.join("evidence/journal.db");
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let started_step = rusqlite::Connection::open(&journal)
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM events WHERE kind = 'step_started'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap_or(0);
        if started_step > 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "step never started"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: sending SIGINT to the child process we spawned.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(25));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(7), "{v:#}");
    assert_eq!(v["status"], "cancelled");
    assert_eq!(step0(&v)["termination"], "cancelled");
    let (_, task) = env.json(&proj, &["task", "show", v["request_id"].as_str().unwrap()]);
    assert_eq!(task["results"][0]["status"], "cancelled");
}

#[test]
fn deterministic_resolution_without_inference() {
    // No inference provider is configured anywhere in these tests; this one
    // makes the point explicitly with an exact invocation and a lexical match.
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let (code, v) = env.json(&proj, &["make test", "--yes"]);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["plan"]["candidates"][0]["score"], 100.0);
    let (code, v) = env.json(
        &proj,
        &[
            "run the checks",
            "--yes",
            "--expect-stdout",
            "checks passed",
        ],
    );
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(plan_step(&v)["capability_id"], "project-task:make:test");
    assert_eq!(v["verification"]["status"], "passed");
}

// --- The end-to-end demonstration (spec §12) -----------------------------------

#[test]
fn end_to_end_unfamiliar_executable() {
    let env = Env::new();
    let proj = env.project("make-project");
    // 1. An executable cli2 has never seen.
    let tool = env.bin.join("mkreport");
    std::fs::copy(fixture("tools/mkreport"), &tool).unwrap();

    // 2–3. Scan discovers it and the project's tasks, with no custom definition.
    let (_, scan) = env.json(&proj, &["scan"]);
    assert!(scan["environment"]["added"]
        .to_string()
        .contains("executable:mkreport"));
    assert!(scan["project"]["added"]
        .to_string()
        .contains("project-task:make:build"));

    // 4. Ask for something the executable can do. Its interface is unknown,
    //    so cli2 asks rather than guessing...
    let intent = "write a summary report to out/report.txt";
    let (code, v) = env.json(&proj, &[intent]);
    assert_eq!(code, 4, "{v:#}");
    assert!(!proj.join("out/report.txt").exists());

    // ...and with consent it learns the interface from bounded help output.
    // 5–6. Candidates and the exact invocation are shown; policy requires
    //      approval because the effects are unknown.
    let (code, v) = env.json(&proj, &[intent, "--capture-help"]);
    assert_eq!(code, 3, "{v:#}");
    assert_eq!(v["status"], "awaiting_approval");
    assert_eq!(plan_step(&v)["capability_id"], "executable:mkreport");
    assert_eq!(
        plan_step(&v)["resolved_executable"],
        tool.display().to_string()
    );
    assert_eq!(
        plan_step(&v)["arguments"],
        serde_json::json!(["out/report.txt"])
    );
    assert!(v["plan"]["candidates"].as_array().unwrap().len() > 1);
    assert!(!proj.join("out/report.txt").exists());

    // 7–8. Approve the exact plan; a real process runs; the postcondition is verified.
    let (code, v) = env.json(
        &proj,
        &[
            "exec",
            v["plan_id"].as_str().unwrap(),
            "--approve",
            v["plan"]["digest"].as_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["status"], "succeeded");
    assert_eq!(v["verification"]["status"], "passed");
    let report = std::fs::read_to_string(proj.join("out/report.txt")).unwrap();
    assert!(report.contains("files:"));

    // 9. JSON and evidence record.
    assert_valid("result", &v);
    let (_, task) = env.json(&proj, &["task", "show", v["request_id"].as_str().unwrap()]);
    assert_eq!(task["journal_integrity"], "verified");
    assert_eq!(task["approvals"][0]["method"], "digest_token");
    let (_, cap) = env.json(&proj, &["describe", "executable:mkreport"]);
    assert_eq!(cap["enrichment"], "observed");

    // 10. Change the environment: the executable is replaced. Its old
    //     characterization is invalidated, and a stale plan cannot run.
    let (_, stale) = env.json(&proj, &["plan", intent]);
    std::fs::write(
        &tool,
        std::fs::read_to_string(fixture("tools/mkreport")).unwrap() + "\n# v2\n",
    )
    .unwrap();
    let (code, v) = env.json(
        &proj,
        &[
            "exec",
            stale["plan_id"].as_str().unwrap(),
            "--approve",
            stale["plan"]["digest"].as_str().unwrap(),
        ],
    );
    assert_eq!(code, 6, "{v:#}");
    let (_, scan) = env.json(&proj, &["scan"]);
    let changed = &scan["environment"]["changed"][0];
    assert_eq!(changed["id"], "executable:mkreport");
    assert!(changed["invalidated"].to_string().contains("help"));
    let (_, cap) = env.json(&proj, &["describe", "executable:mkreport"]);
    assert_eq!(cap["enrichment"], "discovered");
}

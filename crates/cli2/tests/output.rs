//! Output capture and reuse: formats, retrieval, truncation/retention,
//! clipboard and batches. All processes are fixture executables written by the
//! tests, on the hermetic PATH from `support::Env`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::*;

/// An executable that documents itself (`--help`), then does `body`.
fn tool(env: &Env, name: &str, body: &str) -> PathBuf {
    let path = env.bin.join(name);
    write_executable(
        &path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then cat <<'EOF'\n{name} - fixture tool for output tests\n\nUsage: {name} [OPTIONS]\nEOF\nexit 0; fi\n{body}\n"
        ),
    );
    path
}

/// A tool that takes three positional arguments and prints each in brackets.
fn argecho(env: &Env) -> PathBuf {
    let path = env.bin.join("argecho");
    write_executable(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then cat <<'EOF'\nargecho - print arguments\n\nUsage: argecho <FIRST> <SECOND> <THIRD>\nEOF\nexit 0; fi\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\n",
    );
    path
}

fn learn(env: &Env, dir: &Path, names: &[&str]) {
    assert_eq!(env.run(dir, &["scan"]).code, 0);
    for n in names {
        let r = env.run(
            dir,
            &["describe", &format!("executable:{n}"), "--capture-help"],
        );
        assert_eq!(r.code, 0, "{n}: {}", r.stderr);
    }
}

fn project(env: &Env) -> PathBuf {
    env.project("node-project")
}

fn show(env: &Env, dir: &Path, id: &str) -> Value {
    let r = env.run(dir, &["--json", "output", "show", id]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    r.json()
}

#[test]
fn response_only_prints_just_stdout_and_forwards_stderr() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT; echo ERR >&2");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    let r = env.run(&proj, &["emit", "--yes", "--output", "response"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout, "OUT\n",
        "no presentation, no command, no narration"
    );
    assert_eq!(
        r.stderr, "ERR\n",
        "the process's own stderr is not swallowed or mixed into stdout"
    );
}

#[test]
fn command_and_response_shows_the_command_then_the_streams() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT; echo ERR >&2");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    let r = env.run(
        &proj,
        &["emit", "--yes", "--output", "command-and-response"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "$ emit\nOUT\n[stderr]\nERR\n");
}

#[test]
fn json_output_is_structured_and_separates_process_task_and_verification() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT; echo ERR >&2");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    let r = env.run(&proj, &["emit", "--yes", "--output", "json"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["schema_version"], "cli2.output_result.v1");
    assert_eq!(v["executed"], true);
    assert_eq!(v["status"], "succeeded");
    let o = &v["outputs"][0];
    assert_eq!(o["schema_version"], "cli2.output.v1");
    assert_eq!(o["output_id"], "out_1");
    assert_eq!(o["execution_id"], "exec_1");
    assert!(o["request_id"].as_str().unwrap().starts_with("req_"));
    assert!(o["plan_id"].as_str().unwrap().starts_with("plan_"));
    assert_eq!(
        o["command"]["argv"][0],
        env.bin.join("emit").display().to_string()
    );
    assert_eq!(o["command"]["display"], "emit");
    assert_eq!(
        o["working_directory"],
        proj.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(o["stdout"]["text"], "OUT\n");
    assert_eq!(o["stderr"]["text"], "ERR\n");
    assert_eq!(o["stdout"]["truncated"], false);
    assert_eq!(o["process"]["termination"], "exited");
    assert_eq!(o["process"]["exit_code"], 0);
    assert_eq!(o["process"]["exited_zero"], true);
    // Process success is not task success: nothing verified the goal.
    assert_eq!(o["verification"]["status"], "unverified");
    assert!(o["notes"]
        .to_string()
        .contains("no task-level verification"));
    assert!(o["started_at"].as_str().is_some() && o["duration_ms"].as_u64().is_some());
    // Exactly the documented keys.
    let mut keys: Vec<&str> = o.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "capability_id",
            "command",
            "duration_ms",
            "evidence_ref",
            "execution_id",
            "finished_at",
            "notes",
            "output_id",
            "plan_id",
            "process",
            "request_id",
            "schema_version",
            "started_at",
            "stderr",
            "stdout",
            "step_index",
            "task",
            "verification",
            "working_directory"
        ]
    );
}

#[test]
fn argv_boundaries_survive_spaces_quotes_and_empty_arguments() {
    let env = Env::new();
    argecho(&env);
    let proj = project(&env);
    learn(&env, &proj, &["argecho"]);
    let r = env.run(
        &proj,
        &[
            "argecho",
            "--yes",
            "--arg",
            "first=two words",
            "--arg",
            "second=it's \"quoted\" $HOME `x`",
            "--arg",
            "third=",
            "--output",
            "json",
        ],
    );
    assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
    let v = r.json();
    let o = &v["outputs"][0];
    assert_eq!(
        o["command"]["argv"],
        json!([
            env.bin.join("argecho").display().to_string(),
            "two words",
            "it's \"quoted\" $HOME `x`",
            ""
        ])
    );
    assert_eq!(
        o["stdout"]["text"],
        "[two words]\n[it's \"quoted\" $HOME `x`]\n[]\n"
    );
    // The display form is unambiguous, quoted data (never executed).
    assert_eq!(
        o["command"]["display"],
        "argecho 'two words' 'it'\\''s \"quoted\" $HOME `x`' ''"
    );
    // A value that looks like a flag is refused by the planner, not silently altered.
    let r = env.run(
        &proj,
        &[
            "argecho",
            "--yes",
            "--arg",
            "first=--odd",
            "--arg",
            "second=b",
            "--arg",
            "third=c",
            "--output",
            "response",
        ],
    );
    assert_ne!(r.code, 0);
    assert_eq!(r.stdout, "");
}

#[test]
fn recorded_argv_with_dash_arguments_is_preserved_exactly() {
    // The planner never passes undocumented flags, so a stored argument that
    // starts with `--` is constructed directly in the journal; retrieval must
    // keep every boundary.
    let env = Env::new();
    let proj = project(&env);
    let dir = env.home.join("evidence");
    let j = cli2_evidence::Journal::open(&dir).unwrap();
    let out = j.store_blob(b"hello\n").unwrap();
    let err = j.store_blob(b"").unwrap();
    let result: cli2_core::ExecutionResult = serde_json::from_value(json!({
        "schema_version": "cli2.result.v1", "request_id": "req_synthetic", "plan_id": "plan_synthetic",
        "status": "succeeded", "process_status": "succeeded",
        "steps": [{
            "index": 0, "capability_id": "executable:x", "executable": "/bin/x",
            "arguments": ["--odd", "", "a b", "--", "-n"], "working_directory": "/tmp",
            "termination": "exited", "exit_code": 0, "started_at": "2026-01-01T00:00:00.000Z",
            "finished_at": "2026-01-01T00:00:00.010Z", "duration_ms": 10,
            "stdout_ref": out, "stderr_ref": err, "stdout_bytes": 6, "stderr_bytes": 0,
            "stdout_truncated": false, "stderr_truncated": false
        }],
        "verification": {"status": "unverified", "task_level": false, "checks": []},
        "limitations": [], "evidence_ref": "evidence:task:req_synthetic", "message": "synthetic"
    }))
    .unwrap();
    j.record_result(&result, None).unwrap();
    drop(j);
    let v = show(&env, &proj, "out_1");
    assert_eq!(
        v["command"]["argv"],
        json!(["/bin/x", "--odd", "", "a b", "--", "-n"])
    );
    assert_eq!(v["command"]["display"], "x --odd '' 'a b' -- -n");
}

#[test]
fn nonzero_exit_keeps_streams_apart_and_is_never_hidden_by_a_quiet_format() {
    let env = Env::new();
    tool(&env, "failer", "echo partial; echo boom >&2; exit 3");
    let proj = project(&env);
    learn(&env, &proj, &["failer"]);
    let r = env.run(&proj, &["failer", "--yes", "--output", "response"]);
    assert_eq!(r.code, 1, "failed -> exit 1");
    assert_eq!(r.stdout, "partial\n");
    assert!(r.stderr.contains("boom"), "{}", r.stderr);
    assert!(
        r.stderr.contains("Failed"),
        "a failure note is always shown: {}",
        r.stderr
    );
    let v = show(&env, &proj, "latest");
    assert_eq!(v["process"]["exit_code"], 3);
    assert_eq!(v["process"]["exited_zero"], false);
    assert_eq!(v["task"]["status"], "failed");
    assert_eq!(
        env.run(&proj, &["output", "copy", "latest", "--part", "stdout"])
            .stdout,
        "partial\n"
    );
    assert_eq!(
        env.run(&proj, &["output", "copy", "latest", "--part", "stderr"])
            .stdout,
        "boom\n"
    );
    assert_eq!(
        env.run(
            &proj,
            &["output", "copy", "latest", "--part", "command-and-response"]
        )
        .stdout,
        "$ failer\npartial\n[stderr]\nboom\n[exit 3]\n"
    );
}

#[test]
fn timeouts_are_recorded_as_such() {
    let env = Env::new();
    tool(&env, "sleeper", "echo started; sleep 30");
    let proj = project(&env);
    learn(&env, &proj, &["sleeper"]);
    let started = Instant::now();
    let r = env.run(
        &proj,
        &["sleeper", "--yes", "--timeout", "1", "--output", "response"],
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stdout, "started\n",
        "what was captured before the timeout is kept"
    );
    let v = show(&env, &proj, "latest");
    assert_eq!(v["process"]["termination"], "timed_out");
    assert_eq!(v["task"]["status"], "failed");
    let text = env
        .run(
            &proj,
            &["output", "copy", "latest", "--part", "command-and-response"],
        )
        .stdout;
    assert!(text.contains("[timed out]"), "{text}");
}

#[test]
fn interrupted_processes_are_recorded_as_interrupted() {
    let env = Env::new();
    let marker = env.work.join("started");
    tool(
        &env,
        "waiter",
        &format!("echo begun; : > '{}'; sleep 30", marker.display()),
    );
    let proj = project(&env);
    learn(&env, &proj, &["waiter"]);
    let child = env
        .command(&proj, &["waiter", "--yes", "--output", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !marker.exists() {
        assert!(t.elapsed() < Duration::from_secs(20), "tool never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: signalling the cli2 process this test spawned.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7), "cancelled exits 7");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "cancelled");
    assert_eq!(v["outputs"][0]["process"]["termination"], "cancelled");
    let shown = show(&env, &proj, "latest");
    assert_eq!(shown["process"]["termination"], "cancelled");
    assert!(env
        .run(
            &proj,
            &["output", "copy", "latest", "--part", "command-and-response"]
        )
        .stdout
        .contains("[interrupted]"));
}

#[test]
fn exit_zero_is_not_task_success() {
    let env = Env::new();
    tool(&env, "quiet", "echo nothing useful");
    let proj = project(&env);
    learn(&env, &proj, &["quiet"]);
    // Unverified: process fine, goal not established.
    let r = env.run(&proj, &["quiet", "--yes", "--output", "json"]);
    assert_eq!(r.code, 0);
    assert_eq!(
        r.json()["outputs"][0]["verification"]["status"],
        "unverified"
    );
    // A caller expectation that does not hold: the process exited 0, the task did not succeed.
    let r = env.run(
        &proj,
        &[
            "quiet",
            "--yes",
            "--expect-stdout",
            "wanted text",
            "--output",
            "response",
        ],
    );
    assert_eq!(r.code, 5, "verification_failed exits 5: {}", r.stderr);
    assert_eq!(
        r.stdout, "nothing useful\n",
        "the output is still available"
    );
    assert!(
        r.stderr.contains("cli2:"),
        "the failure is stated: {}",
        r.stderr
    );
    let v = show(&env, &proj, "latest");
    assert_eq!(v["process"]["exited_zero"], true);
    assert_eq!(v["task"]["status"], "verification_failed");
    assert_eq!(v["verification"]["status"], "failed");
}

#[test]
fn unexecuted_plans_produce_no_response() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    // Plan only.
    let r = env.run(&proj, &["plan", "emit", "--output", "response"]);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""));
    assert!(r.stderr.contains("not executed"), "{}", r.stderr);
    // Approval required but not given (non-interactive): nothing runs, exit 3.
    for fmt in ["response", "command-and-response"] {
        let r = env.run(&proj, &["emit", "--output", fmt]);
        assert_eq!((r.code, r.stdout.as_str()), (3, ""), "{fmt}");
        assert!(r.stderr.contains("approval"), "{}", r.stderr);
    }
    let r = env.run(&proj, &["emit", "--output", "json"]);
    assert_eq!(r.code, 3);
    let v = r.json();
    assert_eq!(
        (v["executed"].clone(), v["status"].clone()),
        (json!(false), json!("awaiting_approval"))
    );
    assert_eq!(v["outputs"], json!([]));
    assert_eq!(
        env.run(&proj, &["output", "list"]).stdout.trim(),
        "No captured results yet."
    );
    // A request that cannot be resolved also yields nothing.
    let r = env.run(
        &proj,
        &["order a pizza with extra cheese", "--output", "response"],
    );
    assert_eq!((r.code, r.stdout.as_str()), (4, ""));
}

#[test]
fn ids_are_stable_and_retrieval_never_reruns_anything() {
    let env = Env::new();
    let counter = env.work.join("runs");
    tool(
        &env,
        "counted",
        &format!("echo run >> '{}'; echo 'result one'", counter.display()),
    );
    tool(&env, "other", "echo different");
    let proj = project(&env);
    learn(&env, &proj, &["counted", "other"]);
    let runs = || {
        std::fs::read_to_string(&counter)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    assert_eq!(
        env.run(&proj, &["counted", "--yes", "--output", "response"])
            .stdout,
        "result one\n"
    );
    env.run(&proj, &["other", "--yes", "--output", "response"]);
    assert_eq!(runs(), 1);
    // Distinct, stable IDs; the first is unchanged by later activity.
    let list = env.run(&proj, &["--json", "output", "list"]).json();
    assert_eq!(list["schema_version"], "cli2.output_list.v1");
    let ids: Vec<&str> = list["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["output_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["out_2", "out_1"], "newest first");
    for _ in 0..3 {
        assert_eq!(show(&env, &proj, "out_1")["stdout"]["text"], "result one\n");
        assert_eq!(show(&env, &proj, "exec_1")["output_id"], "out_1");
        assert_eq!(show(&env, &proj, "latest")["output_id"], "out_2");
        assert_eq!(
            env.run(&proj, &["output", "copy", "out_1"]).stdout,
            "result one\n",
            "default part is response"
        );
        assert_eq!(
            env.run(
                &proj,
                &["output", "copy", "out_1", "--part", "command-and-response"]
            )
            .stdout,
            "$ counted\nresult one\n"
        );
    }
    assert_eq!(runs(), 1, "retrieval must not rerun the command");
    // Unknown IDs are clean errors.
    for bad in ["out_99", "exec_99", "out_1.7", "nonsense", "req_x"] {
        let r = env.run(&proj, &["output", "show", bad]);
        assert_eq!(r.code, 2, "{bad}: {}", r.stderr);
        assert_eq!(r.stdout, "");
    }
    // Evidence integrity is untouched by all of this.
    let t = env
        .run(
            &proj,
            &[
                "--json",
                "task",
                "show",
                show(&env, &proj, "out_1")["request_id"].as_str().unwrap(),
            ],
        )
        .json();
    assert_eq!(t["journal_integrity"], "verified");
}

#[test]
fn latest_without_any_execution_is_an_error_not_a_guess() {
    let env = Env::new();
    let proj = project(&env);
    let r = env.run(&proj, &["output", "show", "latest"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("no executed command"), "{}", r.stderr);
}

#[test]
fn truncated_output_is_marked_and_never_presented_as_complete() {
    let env = Env::new();
    std::fs::write(
        env.home.join("config.json"),
        r#"{"output":{"max_bytes":1024}}"#,
    )
    .unwrap();
    tool(
        &env,
        "flood",
        "head -c 5000 /dev/zero | tr '\\0' 'x'; echo >&2 noisy",
    );
    let proj = project(&env);
    learn(&env, &proj, &["flood"]);
    let r = env.run(&proj, &["flood", "--yes", "--output", "response"]);
    assert_eq!(
        r.code, 0,
        "process and verification status are preserved: {}",
        r.stderr
    );
    assert_eq!(r.stdout.len(), 1024);
    assert!(r.stderr.contains("truncated"), "{}", r.stderr);
    let v = show(&env, &proj, "latest");
    assert_eq!(v["stdout"]["truncated"], true);
    assert_eq!(v["stdout"]["bytes"], 5000);
    assert_eq!(v["stdout"]["captured_bytes"], 1024);
    assert_eq!(v["stderr"]["truncated"], false);
    assert_eq!(v["task"]["status"], "succeeded");
    assert_eq!(v["verification"]["status"], "unverified");
    assert!(v["notes"].to_string().contains("truncated"));
    let c = env
        .run(
            &proj,
            &["output", "copy", "latest", "--part", "command-and-response"],
        )
        .stdout;
    assert!(
        c.contains("[stdout truncated: kept 1024 of 5000 bytes]"),
        "{}",
        &c[c.len().saturating_sub(120)..]
    );
    assert!(env
        .run(&proj, &["output", "list"])
        .stdout
        .contains("[truncated]"));
    // Settings are validated.
    std::fs::write(
        env.home.join("config.json"),
        r#"{"output":{"max_bytes":3}}"#,
    )
    .unwrap();
    assert_eq!(env.run(&proj, &["output", "list"]).code, 2);
}

#[test]
fn prune_removes_old_content_but_keeps_records_ids_and_the_hash_chain() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    env.run(&proj, &["emit", "--yes"]);
    // Dry run changes nothing.
    let r = env.run(
        &proj,
        &[
            "--json",
            "output",
            "prune",
            "--older-than-days",
            "0",
            "--dry-run",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let rep = r.json();
    assert_eq!(
        (rep["dry_run"].clone(), rep["executions"].clone()),
        (json!(true), json!(1))
    );
    assert!(rep["blobs_removed"].as_u64().unwrap() >= 1);
    assert_eq!(env.run(&proj, &["output", "copy", "out_1"]).stdout, "OUT\n");
    // Real prune.
    let rep = env
        .run(
            &proj,
            &["--json", "output", "prune", "--older-than-days", "0"],
        )
        .json();
    assert_eq!(rep["dry_run"], false);
    // The record and its ID remain; the content does not.
    let v = show(&env, &proj, "out_1");
    assert_eq!(v["stdout"]["available"], false);
    assert!(v["notes"].to_string().contains("no longer retained"));
    assert_eq!(v["process"]["exit_code"], 0);
    let r = env.run(&proj, &["output", "copy", "out_1"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("no longer retained"), "{}", r.stderr);
    assert_eq!(r.stdout, "");
    let t = env
        .run(
            &proj,
            &["--json", "task", "show", v["request_id"].as_str().unwrap()],
        )
        .json();
    assert_eq!(t["journal_integrity"], "verified");
}

#[test]
fn prune_keeps_content_that_a_newer_execution_shares() {
    let env = Env::new();
    tool(&env, "emit", "echo SAME");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    let mut cfg = cli2::Config::from_env().unwrap();
    cfg.home = env.home.clone();
    cfg.path_var = env.path.clone();
    let mut engine = cli2::Engine::open(cfg).unwrap();
    let run = |e: &mut cli2::Engine| {
        let mut req = cli2_core::RequestInput::new("emit");
        req.context.working_directory = Some(proj.display().to_string());
        e.run(
            req,
            &cli2::RunOptions {
                execute: true,
                yes: true,
                ..Default::default()
            },
            &mut cli2::NonInteractive,
        )
        .unwrap()
    };
    run(&mut engine);
    std::thread::sleep(Duration::from_millis(30));
    let cutoff = cli2_core::now_rfc3339();
    std::thread::sleep(Duration::from_millis(30));
    run(&mut engine); // identical output: one stored blob, two records
    let rep = engine.prune_outputs(&cutoff, false).unwrap();
    assert_eq!((rep.executions, rep.blobs_removed), (1, 0), "{rep:?}");
    assert!(rep.kept_shared >= 1);
    assert_eq!(
        engine
            .output_part("out_1", cli2::output::Part::Response)
            .unwrap(),
        b"SAME\n"
    );
    assert_eq!(
        engine
            .output_part("out_2", cli2::output::Part::Response)
            .unwrap(),
        b"SAME\n"
    );
}

// ----------------------------------------------------------------- clipboard

fn clipboard_tool_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("pbcopy")
    } else if cfg!(target_os = "linux") {
        Some("wl-copy")
    } else {
        None
    }
}

#[test]
fn clipboard_copy_is_opt_in_and_reports_missing_and_failing_utilities() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    // Running and printing never touch the clipboard.
    let sink = env.work.join("clipboard.txt");
    let Some(name) = clipboard_tool_name() else {
        let r = env.run(&proj, &["emit", "--yes"]);
        assert_eq!(r.code, 0);
        let r = env.run(&proj, &["output", "copy", "latest", "--clipboard"]);
        assert_ne!(r.code, 0);
        assert!(r.stderr.contains("not supported"), "{}", r.stderr);
        eprintln!(
            "clipboard utilities are not modelled on this platform; checked the unsupported path"
        );
        return;
    };
    write_executable(
        &env.bin.join(name),
        &format!("#!/bin/sh\ncat > '{}'\n", sink.display()),
    );
    assert_eq!(
        env.run(&proj, &["emit", "--yes", "--output", "response"])
            .stdout,
        "OUT\n"
    );
    assert!(!sink.exists(), "nothing is copied unless asked");
    // Explicit copy: nothing on stdout, content in the clipboard utility.
    let r = env.run(
        &proj,
        &[
            "output",
            "copy",
            "latest",
            "--part",
            "command-and-response",
            "--clipboard",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "");
    assert_eq!(std::fs::read_to_string(&sink).unwrap(), "$ emit\nOUT\n");
    // Utility present but failing: a clear error, and no fallback to stdout.
    write_executable(
        &env.bin.join(name),
        "#!/bin/sh\necho 'no display' >&2\nexit 1\n",
    );
    let r = env.run(&proj, &["output", "copy", "latest", "--clipboard"]);
    assert_ne!(r.code, 0);
    assert_eq!(
        r.stdout, "",
        "never silently falls back to another destination"
    );
    assert!(
        r.stderr.contains("failed") && r.stderr.contains("no display"),
        "{}",
        r.stderr
    );
    // Utility absent.
    std::fs::remove_file(env.bin.join(name)).unwrap();
    let r = env.run(&proj, &["output", "copy", "latest", "--clipboard"]);
    assert_ne!(r.code, 0);
    assert_eq!(r.stdout, "");
    assert!(r.stderr.contains("no clipboard utility"), "{}", r.stderr);
}

// -------------------------------------------------------------------- batch

fn write_batch(env: &Env, name: &str, value: Value) -> PathBuf {
    let p = env.work.join(name);
    std::fs::write(&p, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    p
}

fn step(id: &str, intent: &str, dir: &Path, depends: &[&str]) -> Value {
    json!({
        "id": id, "depends_on": depends,
        "request": {"schema_version": "cli2.request.v1", "intent": intent, "context": {"working_directory": dir.display().to_string()}}
    })
}

#[test]
fn batch_stop_mode_runs_in_order_and_reports_partial_completion() {
    let env = Env::new();
    tool(&env, "first", "echo one");
    tool(&env, "breaks", "echo half; echo bad >&2; exit 4");
    tool(&env, "third", "echo three");
    let proj = project(&env);
    learn(&env, &proj, &["first", "breaks", "third"]);
    let file = write_batch(
        &env,
        "stop.json",
        json!({"schema_version": "cli2.batch.v1", "steps": [
        step("a", "first", &proj, &[]), step("b", "breaks", &proj, &[]), step("c", "third", &proj, &[])]}),
    );
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(r.code, 8, "partially_succeeded: {}\n{}", r.stdout, r.stderr);
    let v = r.json();
    assert_eq!(v["schema_version"], "cli2.batch_result.v1");
    assert_eq!(v["status"], "partially_succeeded");
    assert_eq!(v["on_failure"], "stop");
    let s = v["steps"].as_array().unwrap();
    assert_eq!(
        s.iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a", "b", "c"],
        "order preserved"
    );
    assert_eq!(
        (s[0]["status"].clone(), s[1]["status"].clone()),
        (json!("succeeded"), json!("failed"))
    );
    assert_eq!(
        (s[2]["state"].clone(), s[2]["status"].clone()),
        (json!("not_run"), Value::Null)
    );
    assert!(s[2]["output_ids"].as_array().unwrap().is_empty());
    // Each step has independently identifiable, complete results.
    let b = &s[1]["outputs"][0];
    assert_eq!(b["stdout"]["text"], "half\n");
    assert_eq!(b["stderr"]["text"], "bad\n");
    assert_eq!(b["process"]["exit_code"], 4);
    assert_eq!(
        b["verification"]["status"], "failed",
        "the exit-status check failed"
    );
    assert_eq!(b["task"]["status"], "failed");
    assert_eq!(b["command"]["display"], "breaks");
    // ... and retrievable later by ID, without rerunning.
    let a_id = s[0]["output_ids"][0].as_str().unwrap();
    assert_eq!(env.run(&proj, &["output", "copy", a_id]).stdout, "one\n");
    assert_ne!(s[0]["output_ids"][0], s[1]["output_ids"][0]);
    // The recorded batch can be shown again.
    let shown = env
        .run(
            &proj,
            &["--json", "batch", "--show", v["batch_id"].as_str().unwrap()],
        )
        .json();
    assert_eq!(shown["status"], "partially_succeeded");
    assert_eq!(shown["steps"][1]["output_ids"], s[1]["output_ids"]);
    let human = env
        .run(&proj, &["batch", "--show", v["batch_id"].as_str().unwrap()])
        .stdout;
    assert!(
        human.contains("partially_succeeded") && human.contains("not_run"),
        "{human}"
    );
}

#[test]
fn batch_continue_mode_runs_independent_steps_and_skips_dependents() {
    let env = Env::new();
    tool(&env, "breaks", "exit 4");
    tool(&env, "fine", "echo fine");
    let proj = project(&env);
    learn(&env, &proj, &["breaks", "fine"]);
    let file = write_batch(
        &env,
        "cont.json",
        json!({"schema_version": "cli2.batch.v1", "on_failure": "continue", "steps": [
        step("x", "breaks", &proj, &[]),
        step("y", "fine", &proj, &["x"]),
        step("z", "fine", &proj, &[])]}),
    );
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(r.code, 8, "{}", r.stderr);
    let v = r.json();
    let s = v["steps"].as_array().unwrap();
    assert_eq!(s[0]["status"], "failed");
    assert_eq!(s[1]["state"], "skipped_dependency");
    assert_eq!(s[2]["status"], "succeeded", "independent steps still run");
    assert_eq!(v["status"], "partially_succeeded");
    // All failures: failed.
    let file = write_batch(
        &env,
        "allfail.json",
        json!({"schema_version": "cli2.batch.v1", "on_failure": "continue", "steps": [
        step("x", "breaks", &proj, &[]), step("y", "breaks", &proj, &[])]}),
    );
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!((r.code, r.json()["status"].clone()), (1, json!("failed")));
}

#[test]
fn batch_steps_are_approved_individually_and_yes_never_covers_explicit_approval() {
    require!(has_make(), "make");
    let env = Env::new();
    tool(&env, "fine", "echo fine");
    let proj = project(&env);
    let make_proj = env.project("make-project");
    learn(&env, &proj, &["fine"]);
    let file = write_batch(
        &env,
        "appr.json",
        json!({"schema_version": "cli2.batch.v1", "on_failure": "continue", "steps": [
        step("ok", "fine", &proj, &[]),
        step("clean", "clean up the build output", &make_proj, &[]),
        step("after", "fine", &proj, &[])]}),
    );
    let args = ["--json", "batch", "--request-file", file.to_str().unwrap()];
    // Without --yes nothing is approved implicitly: no process runs.
    let r = env.run(&proj, &args);
    assert_eq!(r.code, 3, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["status"], "awaiting_approval");
    assert!(v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["output_ids"].as_array().unwrap().is_empty()));
    assert!(
        v["steps"][0]["approval_digest"].as_str().is_some(),
        "the digest needed to approve it explicitly is reported"
    );
    // With --yes: confirm-level steps run; the explicit-approval step does not.
    let r = env.run(
        &proj,
        &[
            "--json",
            "batch",
            "--request-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(r.code, 8, "{}", r.stderr);
    let v = r.json();
    assert_eq!(v["steps"][0]["status"], "succeeded");
    assert_eq!(v["steps"][1]["status"], "awaiting_approval");
    assert_eq!(v["steps"][2]["status"], "succeeded");
    assert!(
        v["steps"][1]["plan_id"].as_str().is_some()
            && v["steps"][1]["output_ids"].as_array().unwrap().is_empty()
    );
}

#[test]
fn batch_files_cannot_smuggle_shell_strings_or_approvals() {
    let env = Env::new();
    let proj = project(&env);
    for (name, body) in [
        (
            "shell",
            json!({"schema_version": "cli2.batch.v1", "steps": [{"id": "a", "command": "rm -rf /"}]}),
        ),
        (
            "yes",
            json!({"schema_version": "cli2.batch.v1", "steps": [{"id": "a", "yes": true, "request": {"schema_version": "cli2.request.v1", "intent": "x"}}]}),
        ),
        (
            "cycle",
            json!({"schema_version": "cli2.batch.v1", "steps": [{"id": "a", "depends_on": ["b"], "request": {"schema_version": "cli2.request.v1", "intent": "x"}}]}),
        ),
        (
            "empty",
            json!({"schema_version": "cli2.batch.v1", "steps": []}),
        ),
        (
            "version",
            json!({"schema_version": "cli2.batch.v9", "steps": []}),
        ),
    ] {
        let f = write_batch(&env, &format!("{name}.json"), body);
        let r = env.run(
            &proj,
            &["batch", "--request-file", f.to_str().unwrap(), "--yes"],
        );
        assert_eq!(r.code, 2, "{name}: {}", r.stderr);
        assert_eq!(r.stdout, "");
    }
    assert_eq!(
        env.run(&proj, &["output", "list"]).stdout.trim(),
        "No captured results yet.",
        "nothing ran"
    );
}

#[test]
fn existing_default_output_is_unchanged() {
    let env = Env::new();
    tool(&env, "emit", "echo OUT");
    let proj = project(&env);
    learn(&env, &proj, &["emit"]);
    let r = env.run(&proj, &["emit", "--yes"]);
    assert_eq!(r.code, 0);
    assert!(
        r.stdout.contains("Proposed action"),
        "preview still printed by default: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("── stdout ──") && r.stdout.contains("Succeeded"),
        "{}",
        r.stdout
    );
    let j = env.run(&proj, &["--json", "emit", "--yes"]).json();
    assert_eq!(j["schema_version"], "cli2.result.v1");
    assert!(j["steps"][0]["stdout_ref"].as_str().is_some());
    // `exec` accepts --output too.
    let p = env.run(&proj, &["--json", "plan", "emit"]).json();
    let r = env.run(
        &proj,
        &[
            "exec",
            p["plan_id"].as_str().unwrap(),
            "--yes",
            "--output",
            "response",
        ],
    );
    assert_eq!((r.code, r.stdout.as_str()), (0, "OUT\n"), "{}", r.stderr);
}

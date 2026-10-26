//! The embeddable runtime and its local protocol, tested through the public
//! client only (`cli2-client` + `cli2-protocol`), against a real in-process
//! server on a real Unix socket, with fixture projects and harmless commands.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cli2::runtime::Runtime;
use cli2::server::{Server, ServerHandle};
use cli2_client::{Client, ClientError};
use cli2_protocol::*;
use serde_json::{json, Value};
use support::*;

const MAKEFILE: &str = r#".PHONY: hello slow nap fail wipe install-system
hello: ## Print a greeting
	@echo run >> runs.log; echo hello-out; echo hello-err >&2
slow: ## Wait a long time
	@echo run >> runs.log; echo started; sleep 30
nap: ## Wait a moment, then finish
	@echo run >> runs.log; echo napping; sleep 2; echo woke
fail: ## Always fails
	@echo run >> runs.log; echo partial; echo boom >&2; exit 3
wipe: ## Remove the build output
	@echo run >> runs.log; rm -rf out
install-system: ## Install into system directories
	sudo true
"#;

struct Rig {
    env: Env,
    project: PathBuf,
    sock_dir: tempfile::TempDir,
    socket: PathBuf,
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

impl Rig {
    fn new() -> Rig {
        let env = Env::new();
        let project = env.work.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
        // A short path: Unix socket paths are limited to ~100 bytes.
        let sock_dir = tempfile::Builder::new()
            .prefix("c2s")
            .tempdir_in("/tmp")
            .unwrap();
        // The server refuses a directory other users can enter; make it private.
        std::fs::set_permissions(sock_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = sock_dir.path().join("s.sock");
        let runtime = Arc::new(Runtime::new(config(&env)));
        let server = Server::bind(runtime, &socket).unwrap().spawn();
        Rig {
            env,
            project: project.canonicalize().unwrap(),
            sock_dir,
            socket,
            server: Some(server),
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket, "test").unwrap()
    }

    fn proj(&self) -> String {
        self.project.display().to_string()
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.project.join("runs.log"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Plan an intent and return the plan (asserting that it is one).
    fn plan(&self, c: &mut Client, intent: &str) -> cli2_core::Plan {
        let r = c.plan(&self.proj(), intent).unwrap();
        assert_eq!(
            r.outcome.status,
            cli2_core::Status::Planned,
            "{intent}: {:?}",
            r.outcome.message
        );
        r.outcome.plan.unwrap()
    }

    fn approve_and_start(&self, c: &mut Client, p: &cli2_core::Plan) -> StartResult {
        let a = c.approve(&p.plan_id, &p.digest).unwrap().approval;
        c.start(&p.plan_id, &a.approval_id).unwrap()
    }

    fn stop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
        let _ = &self.sock_dir;
    }
}

fn code(e: ClientError) -> ErrorCode {
    e.code()
        .unwrap_or_else(|| panic!("not a server error: {e}"))
}

/// Send raw lines on a fresh connection and read one response per line.
fn raw(socket: &Path, lines: &[&str]) -> Vec<Value> {
    let mut s = UnixStream::connect(socket).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut out = Vec::new();
    for l in lines {
        s.write_all(l.as_bytes()).unwrap();
        s.write_all(b"\n").unwrap();
        let mut resp = String::new();
        if r.read_line(&mut resp).unwrap() == 0 {
            break;
        }
        out.push(serde_json::from_str(&resp).unwrap());
    }
    out
}

// ============================================================ protocol basics

#[test]
fn versions_are_negotiated_and_misuse_is_reported_explicitly() {
    let rig = Rig::new();
    let r = raw(
        &rig.socket,
        &[
            r#"{"id":"1","op":"capabilities.list","params":{"project":"/tmp"}}"#,
            r#"{"id":"2","op":"hello","params":{"client":{"name":"t"},"versions":[2,3]}}"#,
            r#"{"id":"3","op":"hello","params":{"client":{"name":"t"},"versions":[3,1]}}"#,
            r#"{"id":"4","op":"shell.exec","params":{"command":"rm -rf /"}}"#,
            r#"{"id":"5","op":"plan.approve","params":{"plan_id":"p","plan_digest":"d","extra":true}}"#,
            r#"not json at all"#,
        ],
    );
    assert_eq!(r[0]["error"]["code"], "not_initialized");
    assert_eq!(r[1]["error"]["code"], "unsupported_protocol_version");
    assert_eq!(r[1]["error"]["details"]["supported_versions"], json!([1]));
    assert_eq!(r[2]["result"]["protocol_version"], 1);
    assert_eq!(r[2]["result"]["server"]["name"], "cli2");
    assert!(r[2]["result"]["instance_id"]
        .as_str()
        .unwrap()
        .starts_with("rt_"));
    let ops: Vec<&str> = r[2]["result"]["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o.as_str().unwrap())
        .collect();
    assert!(ops.contains(&"intent.plan") && ops.contains(&"execution.start"));
    assert!(
        !ops.iter()
            .any(|o| o.contains("exec_command") || o.contains("shell") || *o == "execution.run"),
        "there is no arbitrary-command endpoint: {ops:?}"
    );
    // An unsupported operation is named as such and the supported ones are listed.
    assert_eq!(r[3]["error"]["code"], "unsupported_operation");
    assert!(
        r[3]["error"]["details"]["supported_operations"]
            .as_array()
            .unwrap()
            .len()
            > 10
    );
    assert_eq!(
        r[4]["error"]["code"], "invalid_params",
        "unknown fields are rejected, never ignored"
    );
    assert_eq!(r[5]["error"]["code"], "invalid_request");
    assert_eq!(r[5]["id"], "");
}

#[test]
fn oversized_messages_are_refused() {
    let rig = Rig::new();
    let mut s = UnixStream::connect(&rig.socket).unwrap();
    let big = vec![b'a'; MAX_MESSAGE_BYTES + 10];
    let _ = s.write_all(&big);
    let _ = s.write_all(b"\n");
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["error"]["code"], "invalid_request");
}

#[test]
fn project_context_is_explicit_and_validated() {
    let rig = Rig::new();
    let mut c = rig.client();
    for bad in [
        "relative/path",
        "/definitely/not/a/dir",
        rig.project.join("Makefile").to_str().unwrap(),
    ] {
        assert_eq!(
            code(c.plan(bad, "make hello").unwrap_err()),
            ErrorCode::ProjectInvalid,
            "{bad}"
        );
        assert_eq!(
            code(c.capabilities(bad).unwrap_err()),
            ErrorCode::ProjectInvalid,
            "{bad}"
        );
    }
    let s = c.scan(&rig.proj()).unwrap();
    assert!(s.project_root.is_some());
    let caps = c.capabilities(&rig.proj()).unwrap();
    assert!(caps
        .capabilities
        .iter()
        .any(|c| c.id == "project-task:make:hello"));
}

// ===================================================== planning is not executing

#[test]
fn planning_never_executes_and_returns_the_complete_plan() {
    let rig = Rig::new();
    let mut c = rig.client();
    let r = c.plan(&rig.proj(), "make hello").unwrap();
    assert_eq!(r.outcome.status, cli2_core::Status::Planned);
    assert_eq!(r.approvability, Approvability::RequiresApproval);
    let p = r.outcome.plan.unwrap();
    assert_eq!(p.steps[0].argv(), ["make", "hello"]);
    assert!(p.digest.starts_with("sha256:") && p.plan_id.starts_with("plan_"));
    assert!(
        !p.limitations.is_empty(),
        "limitations are part of the plan"
    );
    assert!(r.outcome.steps.is_empty());
    assert_eq!(rig.runs(), 0, "planning must not run the command");
    // The plan is retrievable by its stable ID, with its approval state.
    let v = c.plan_get(&p.plan_id).unwrap();
    assert_eq!(
        (
            v.plan.digest.clone(),
            v.rejected,
            v.approvals.len(),
            v.executions.len()
        ),
        (p.digest.clone(), false, 0, 0)
    );
    // An unresolvable request yields no plan, not a guess.
    let r = c
        .plan(&rig.proj(), "order a pizza with extra cheese")
        .unwrap();
    assert_eq!(
        (r.outcome.status, r.approvability),
        (cli2_core::Status::NeedsClarification, Approvability::NoPlan)
    );
    assert!(r.outcome.plan.is_none());
    assert_eq!(rig.runs(), 0);
}

#[test]
fn a_repeated_planning_request_with_the_same_key_returns_the_same_plan() {
    let rig = Rig::new();
    let mut c = rig.client();
    let mk = |key: &str, intent: &str| PlanParams {
        project: rig.proj(),
        intent: intent.into(),
        idempotency_key: Some(key.into()),
        constraints: Default::default(),
        bindings: Default::default(),
        selection: None,
        expectations: vec![],
        capture_help: false,
        resolver: None,
        timeout_secs: None,
    };
    let a = c.plan_with(mk("k1", "make hello")).unwrap();
    let b = c.plan_with(mk("k1", "make hello")).unwrap();
    assert!(!a.deduplicated && b.deduplicated);
    assert_eq!(
        a.outcome.plan.as_ref().unwrap().plan_id,
        b.outcome.plan.as_ref().unwrap().plan_id
    );
    assert_eq!(
        code(c.plan_with(mk("k1", "make fail")).unwrap_err()),
        ErrorCode::Conflict,
        "same key, different request"
    );
    let other = c.plan_with(mk("k2", "make hello")).unwrap();
    assert_ne!(
        other.outcome.plan.unwrap().plan_id,
        a.outcome.plan.unwrap().plan_id
    );
}

// =========================================================== the approval boundary

#[test]
fn a_rejected_plan_cannot_execute_or_be_approved() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let r = c.reject(&p.plan_id, Some("not now")).unwrap();
    assert_eq!(r.outcome.status, cli2_core::Status::Cancelled);
    assert!(r.outcome.steps.is_empty() && r.outcome.message.contains("nothing was run"));
    assert!(c.plan_get(&p.plan_id).unwrap().rejected);
    assert_eq!(
        code(c.approve(&p.plan_id, &p.digest).unwrap_err()),
        ErrorCode::PlanRejected
    );
    assert_eq!(rig.runs(), 0);

    // Even an approval recorded *before* the rejection no longer authorizes anything.
    let q = rig.plan(&mut c, "make hello");
    let a = c.approve(&q.plan_id, &q.digest).unwrap().approval;
    c.reject(&q.plan_id, None).unwrap();
    assert_eq!(
        code(c.start(&q.plan_id, &a.approval_id).unwrap_err()),
        ErrorCode::PlanRejected
    );
    assert_eq!(rig.runs(), 0, "rejected plans never run");
}

#[test]
fn execution_requires_a_valid_approval_bound_to_the_exact_plan() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    // No approval at all.
    let e = c
        .call::<_, StartResult>(ops::EXECUTION_START, &json!({"plan_id": p.plan_id}))
        .unwrap_err();
    assert_eq!(code(e), ErrorCode::ApprovalRequired);
    // An approval that does not exist.
    assert_eq!(
        code(c.start(&p.plan_id, "appr_nope").unwrap_err()),
        ErrorCode::NotFound
    );
    // The wrong digest, including the short form the CLI shows, does not approve.
    assert_eq!(
        code(c.approve(&p.plan_id, "sha256:00").unwrap_err()),
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(
        code(c.approve(&p.plan_id, &p.digest[7..19]).unwrap_err()),
        ErrorCode::ApprovalMismatch
    );
    // An approval for another plan cannot start this one.
    let other = rig.plan(&mut c, "make hello");
    assert_ne!(other.plan_id, p.plan_id);
    let a_other = c.approve(&other.plan_id, &other.digest).unwrap().approval;
    assert_eq!(
        code(c.start(&p.plan_id, &a_other.approval_id).unwrap_err()),
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(rig.runs(), 0, "nothing ran through any of those");
}

#[test]
fn a_modified_plan_cannot_be_approved_or_run_with_an_earlier_approval() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let a = c.approve(&p.plan_id, &p.digest).unwrap().approval;
    // Tamper with the stored plan (the argv) without fixing its digest.
    let db = rusqlite::Connection::open(rig.env.home.join("evidence/journal.db")).unwrap();
    let json: String = db
        .query_row(
            "SELECT json FROM plans WHERE plan_id = ?1",
            [&p.plan_id],
            |r| r.get(0),
        )
        .unwrap();
    let tampered = json.replace("\"hello\"", "\"fail\"");
    assert_ne!(json, tampered);
    db.execute(
        "UPDATE plans SET json = ?1 WHERE plan_id = ?2",
        [&tampered, &p.plan_id],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        code(c.approve(&p.plan_id, &p.digest).unwrap_err()),
        ErrorCode::PlanModified
    );
    assert_eq!(
        code(c.start(&p.plan_id, &a.approval_id).unwrap_err()),
        ErrorCode::PlanModified
    );
    assert_eq!(rig.runs(), 0);
}

#[test]
fn forbidden_and_stale_plans_cannot_be_approved() {
    let rig = Rig::new();
    let mut c = rig.client();
    // Privilege escalation is forbidden by policy: there is no approval that authorizes it.
    let r = c.plan(&rig.proj(), "make install-system").unwrap();
    assert_eq!(r.approvability, Approvability::Forbidden);
    let p = r.outcome.plan.unwrap();
    assert_eq!(
        code(c.approve(&p.plan_id, &p.digest).unwrap_err()),
        ErrorCode::Forbidden
    );
    // A plan made before the project changed is stale.
    let q = rig.plan(&mut c, "make hello");
    std::fs::write(
        rig.project.join("Makefile"),
        MAKEFILE.replace("hello-out", "changed-out"),
    )
    .unwrap();
    let e = c.approve(&q.plan_id, &q.digest).unwrap_err();
    assert_eq!(code(e), ErrorCode::StalePlan);
    assert!(!c.plan_get(&q.plan_id).unwrap().stale_reasons.is_empty());
    assert_eq!(rig.runs(), 0);
}

#[test]
fn an_approval_authorizes_exactly_one_execution_even_when_retried_concurrently() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let a = c.approve(&p.plan_id, &p.digest).unwrap().approval;
    // Many clients retry the same start at once.
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (socket, plan_id, appr) =
                (rig.socket.clone(), p.plan_id.clone(), a.approval_id.clone());
            std::thread::spawn(move || {
                Client::connect(&socket, "retry")
                    .unwrap()
                    .start(&plan_id, &appr)
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<StartResult> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let ids: std::collections::BTreeSet<_> =
        results.iter().map(|r| r.execution_id.clone()).collect();
    assert_eq!(ids.len(), 1, "one execution: {ids:?}");
    assert_eq!(results.iter().filter(|r| !r.deduplicated).count(), 1);
    let done = c.wait(ids.iter().next().unwrap(), |_| {}).unwrap();
    assert_eq!(done.task_status, Some(cli2_core::Status::Succeeded));
    // Retrying after it finished still does not run it again.
    let again = c.start(&p.plan_id, &a.approval_id).unwrap();
    assert!(again.deduplicated && again.execution_id == done.execution_id);
    assert_eq!(rig.runs(), 1, "the command ran exactly once");
    // A *new* approval is a new decision, and only then does it run again.
    let a2 = c.approve(&p.plan_id, &p.digest).unwrap().approval;
    let second = c.start(&p.plan_id, &a2.approval_id).unwrap();
    assert!(!second.deduplicated && second.execution_id != done.execution_id);
    c.wait(&second.execution_id, |_| {}).unwrap();
    assert_eq!(rig.runs(), 2);
}

// ============================================================= observable execution

#[test]
fn execution_is_observable_with_separate_streams_and_independent_statuses() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let run = rig.approve_and_start(&mut c, &p);
    assert_eq!(
        (run.plan_id.as_str(), run.deduplicated),
        (p.plan_id.as_str(), false)
    );
    let mut events = Vec::new();
    let done = c
        .wait(&run.execution_id, |e| events.push(e.clone()))
        .unwrap();

    // Ordered, typed events.
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>());
    assert!(events
        .iter()
        .all(|e| e.schema_version == "cli2.event.v1" && e.execution_id == run.execution_id));
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.data {
            EventData::ExecutionStarted { .. } => "started",
            EventData::StepStarted { .. } => "step_started",
            EventData::Output { .. } => "output",
            EventData::StepFinished { .. } => "step_finished",
            EventData::Verification { .. } => "verification",
            EventData::ExecutionFinished { .. } => "finished",
        })
        .collect();
    assert_eq!(kinds.first(), Some(&"started"));
    assert_eq!(kinds.last(), Some(&"finished"));
    let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
    assert!(
        pos("step_started") < pos("output")
            && pos("output") < pos("step_finished")
            && pos("step_finished") < pos("verification")
    );
    // stdout and stderr are separate.
    let text = |s: Stream| -> String {
        events
            .iter()
            .filter_map(|e| match &e.data {
                EventData::Output { stream, text, .. } if *stream == s => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(text(Stream::Stdout), "hello-out\n");
    assert_eq!(text(Stream::Stderr), "hello-err\n");
    if let EventData::StepStarted {
        argv,
        pid,
        working_directory,
        ..
    } = &events[pos("step_started")].data
    {
        assert_eq!(argv, &["make", "hello"]);
        assert!(*pid > 0);
        assert_eq!(working_directory, &rig.proj());
    }

    // Process status, task status and verification are separate, and exit 0 is not verified success.
    assert_eq!(done.lifecycle, Lifecycle::Finished);
    let proc = done.process.clone().unwrap();
    assert_eq!(
        (proc.termination, proc.exit_code),
        (cli2_core::Termination::Exited, Some(0))
    );
    assert_eq!(done.task_status, Some(cli2_core::Status::Succeeded));
    assert_eq!(
        done.verification,
        Some(cli2_core::VerificationStatus::Unverified),
        "nothing verified the task"
    );
    assert_eq!(done.source, "live");
    assert_eq!(
        done.result.as_ref().unwrap().evidence_ref,
        format!("evidence:task:{}", run.request_id)
    );
    // Resuming from a cursor returns only what is new.
    let tail = c
        .events(&run.execution_id, events[events.len() - 3].seq, 0)
        .unwrap();
    assert_eq!(tail.events.len(), 2);
    assert!(tail.finished);
}

#[test]
fn a_caller_expectation_turns_process_success_into_verified_or_failed_verification() {
    let rig = Rig::new();
    let mut c = rig.client();
    let mk = |text: &str| PlanParams {
        project: rig.proj(),
        intent: "make hello".into(),
        idempotency_key: None,
        constraints: Default::default(),
        bindings: Default::default(),
        selection: None,
        expectations: vec![cli2_core::Check::StdoutContains { text: text.into() }],
        capture_help: false,
        resolver: None,
        timeout_secs: None,
    };
    for (expect, want_status, want_verification) in [
        (
            "hello-out",
            cli2_core::Status::Succeeded,
            cli2_core::VerificationStatus::Passed,
        ),
        (
            "never printed",
            cli2_core::Status::VerificationFailed,
            cli2_core::VerificationStatus::Failed,
        ),
    ] {
        let p = c.plan_with(mk(expect)).unwrap().outcome.plan.unwrap();
        let run = rig.approve_and_start(&mut c, &p);
        let done = c.wait(&run.execution_id, |_| {}).unwrap();
        assert_eq!(
            done.process.as_ref().unwrap().exit_code,
            Some(0),
            "the process exited 0 either way"
        );
        assert_eq!(
            (done.task_status, done.verification),
            (Some(want_status), Some(want_verification)),
            "{expect}"
        );
    }
}

#[test]
fn failures_timeouts_and_cancellation_are_reported_as_what_they_are() {
    let rig = Rig::new();
    let mut c = rig.client();
    // Failure.
    let p = rig.plan(&mut c, "make fail");
    let run = rig.approve_and_start(&mut c, &p);
    let mut stderr = String::new();
    let done = c
        .wait(&run.execution_id, |e| {
            if let EventData::Output {
                stream: Stream::Stderr,
                text,
                ..
            } = &e.data
            {
                stderr.push_str(text)
            }
        })
        .unwrap();
    assert_eq!(done.task_status, Some(cli2_core::Status::Failed));
    // `make` reports a failed recipe as its own exit status 2.
    assert_eq!(done.process.unwrap().exit_code, Some(2));
    // The recipe's own stderr, then make's diagnostic of the recipe's exit status 3.
    assert_eq!(stderr, "boom\nmake: *** [fail] Error 3\n");
    assert_ne!(
        done.verification,
        Some(cli2_core::VerificationStatus::Passed)
    );

    // Timeout.
    let mut params = PlanParams {
        project: rig.proj(),
        intent: "make slow".into(),
        idempotency_key: None,
        constraints: Default::default(),
        bindings: Default::default(),
        selection: None,
        expectations: vec![],
        capture_help: false,
        resolver: None,
        timeout_secs: Some(1),
    };
    let p = c.plan_with(params.clone()).unwrap().outcome.plan.unwrap();
    assert_eq!(p.timeout_secs, 1);
    let run = rig.approve_and_start(&mut c, &p);
    let t = std::time::Instant::now();
    let done = c.wait(&run.execution_id, |_| {}).unwrap();
    assert!(t.elapsed() < Duration::from_secs(20));
    assert_eq!(
        done.process.unwrap().termination,
        cli2_core::Termination::TimedOut
    );
    assert_eq!(done.task_status, Some(cli2_core::Status::Failed));
    params.timeout_secs = Some(0);
    assert_eq!(
        code(c.plan_with(params).unwrap_err()),
        ErrorCode::InvalidParams
    );

    // Cancellation.
    let p = rig.plan(&mut c, "make slow");
    let run = rig.approve_and_start(&mut c, &p);
    let mut seen = 0;
    loop {
        let b = c.events(&run.execution_id, seen, 2000).unwrap();
        if let Some(l) = b.events.last() {
            seen = l.seq
        }
        if b.events
            .iter()
            .any(|e| matches!(&e.data, EventData::Output { text, .. } if text.contains("started")))
        {
            break;
        }
    }
    let cancel = c.cancel(&run.execution_id).unwrap();
    assert!(cancel.accepted);
    let done = c.wait_from(&run.execution_id, seen, |_| {}).unwrap();
    assert_eq!(done.task_status, Some(cli2_core::Status::Cancelled));
    assert_eq!(
        done.process.unwrap().termination,
        cli2_core::Termination::Cancelled
    );
    assert_ne!(
        done.verification,
        Some(cli2_core::VerificationStatus::Passed)
    );
    // Cancelling a finished execution is a no-op, not an error and not a state change.
    let again = c.cancel(&run.execution_id).unwrap();
    assert!(!again.accepted);
    assert_eq!(
        c.execution(&run.execution_id).unwrap().task_status,
        Some(cli2_core::Status::Cancelled)
    );
}

// ======================================================== evidence and durability

#[test]
fn evidence_is_retrievable_through_the_protocol() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let run = rig.approve_and_start(&mut c, &p);
    let done = c.wait(&run.execution_id, |_| {}).unwrap();
    let step = &done.steps[0];
    // The recorded output, by blob reference and by output ID.
    assert_eq!(
        c.evidence(step.stdout_ref.as_ref().unwrap())
            .unwrap()
            .content,
        "hello-out\n"
    );
    assert_eq!(
        c.evidence(step.stderr_ref.as_ref().unwrap())
            .unwrap()
            .content,
        "hello-err\n"
    );
    let last = c.tasks(5).unwrap();
    assert_eq!(last.schema, "cli2.task_list.v1");
    let tasks = last.document["tasks"].as_array().unwrap();
    assert!(tasks
        .iter()
        .any(|t| t["request_id"] == run.request_id.as_str()));
    assert_eq!(
        c.output("latest", "response").unwrap().content,
        "hello-out\n"
    );
    assert_eq!(c.output("latest", "stderr").unwrap().content, "hello-err\n");
    assert!(c
        .output("latest", "command-and-response")
        .unwrap()
        .content
        .starts_with("$ make hello\n"));
    // The full task record: request, plan, approval, result, hash-chained events.
    let t = c.task(&run.request_id).unwrap();
    assert_eq!(t.schema, "cli2.task.v1");
    assert_eq!(t.document["journal_integrity"], "verified");
    assert_eq!(t.document["plans"][0]["plan_id"], p.plan_id.as_str());
    assert_eq!(t.document["approvals"][0]["decision"], "granted");
    // The planning result came first; the execution's result is the latest.
    assert_eq!(t.document["results"][0]["status"], "planned");
    assert_eq!(
        t.document["results"].as_array().unwrap().last().unwrap()["status"],
        "succeeded"
    );
    let kinds: Vec<&str> = t.document["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"execution_started")
            && kinds.contains(&"execution_finished")
            && kinds.contains(&"step_started"),
        "{kinds:?}"
    );
    // Bad references are refused, not read.
    assert_eq!(
        code(c.evidence("../../etc/passwd").unwrap_err()),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        code(c.evidence("evidence:sha256:00").unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(c.output("nonsense", "stdout").unwrap_err()),
        ErrorCode::InvalidParams
    );
}

#[test]
fn a_disconnected_client_neither_stops_the_execution_nor_loses_its_outcome() {
    let mut rig = Rig::new();
    let (run, mut seen) = {
        let mut c = rig.client();
        let p = rig.plan(&mut c, "make nap");
        let run = rig.approve_and_start(&mut c, &p);
        let first = c.events(&run.execution_id, 0, 3000).unwrap();
        (run, first.last_seq)
        // `c` is dropped here: the connection is gone mid-execution.
    };
    // A fresh connection (a "reconnect") sees it still running or finished, never lost.
    let mut c2 = rig.client();
    let snap = c2.execution(&run.execution_id).unwrap();
    assert!(
        matches!(snap.lifecycle, Lifecycle::Running | Lifecycle::Finished),
        "{:?}",
        snap.lifecycle
    );
    if snap.lifecycle == Lifecycle::Running {
        assert!(
            snap.task_status.is_none(),
            "no task status, and certainly no success, while it is still running"
        );
    }
    let mut out = String::new();
    let done = c2
        .wait_from(&run.execution_id, seen, |e| {
            seen = e.seq;
            if let EventData::Output { text, .. } = &e.data {
                out.push_str(text)
            }
        })
        .unwrap();
    assert_eq!(done.task_status, Some(cli2_core::Status::Succeeded));
    assert!(
        out.contains("woke"),
        "the rest of the output arrived after reconnecting: {out:?}"
    );
    assert_eq!(rig.runs(), 1, "reconnecting did not run it again");

    // Even after the runtime itself restarts, the durable outcome is recoverable.
    let id = run.execution_id.clone();
    drop(c2);
    rig.stop();
    let rt2 = Arc::new(Runtime::new(config(&rig.env)));
    let sock2 = rig.sock_dir.path().join("s2.sock");
    let _srv = Server::bind(rt2, &sock2).unwrap().spawn();
    let mut c3 = Client::connect(&sock2, "later").unwrap();
    let after = c3.execution(&id).unwrap();
    assert_eq!(after.source, "journal");
    assert_eq!(
        (after.lifecycle, after.task_status),
        (Lifecycle::Finished, Some(cli2_core::Status::Succeeded))
    );
    assert_eq!(after.result.unwrap().request_id, run.request_id);
    let ev = c3.events(&id, 0, 0).unwrap();
    assert!(
        ev.finished
            && matches!(
                ev.events.last().unwrap().data,
                EventData::ExecutionFinished { .. }
            )
    );
    // A retried start after the restart still does not run it a second time.
    let appr = c3.plan_get(&run.plan_id).unwrap().approvals[0]
        .approval_id
        .clone();
    let again = c3.start(&run.plan_id, &appr).unwrap();
    assert!(again.deduplicated && again.execution_id == id);
    assert_eq!(rig.runs(), 1);
}

#[test]
fn work_interrupted_by_a_dead_runtime_is_reconciled_without_claiming_success() {
    let rig = Rig::new();
    let mut c = rig.client();
    let p = rig.plan(&mut c, "make hello");
    let a = c.approve(&p.plan_id, &p.digest).unwrap().approval;
    // A previous runtime recorded that it started this execution and then died: no result exists.
    let j = cli2_evidence::Journal::open(&rig.env.home.join("evidence")).unwrap();
    j.event(
        &p.request_id,
        "execution_started",
        &json!({"execution_id": "run_dead", "plan_id": p.plan_id, "request_id": p.request_id, "approval_id": a.approval_id,
                "instance_id": "rt_dead", "started_at": cli2_core::now_rfc3339()}),
    )
    .unwrap();
    drop(j);
    assert_eq!(
        c.execution("run_dead").unwrap().lifecycle,
        Lifecycle::InterruptedUnknown,
        "not live, no outcome"
    );
    let rt = Runtime::new(config(&rig.env));
    assert_eq!(rt.reconcile().unwrap(), vec!["run_dead".to_string()]);
    assert!(rt.reconcile().unwrap().is_empty(), "idempotent");
    let snap = rt.execution("run_dead").unwrap();
    assert_eq!(snap.lifecycle, Lifecycle::InterruptedUnknown);
    let r = snap.result.unwrap();
    assert_eq!(r.status, cli2_core::Status::Cancelled);
    assert!(r.message.contains("unknown") && r.message.contains("no success is claimed"));
    assert!(
        r.steps
            .iter()
            .all(|s| s.termination == cli2_core::Termination::NotStarted)
            || r.steps.is_empty()
    );
    assert_eq!(rig.runs(), 0);
    // The approval stays used up: it cannot start a new execution.
    let again = rt.start(&p.plan_id, &a.approval_id).unwrap();
    assert!(again.deduplicated && again.execution_id == "run_dead");
}

// ============================================================ access control

#[test]
fn the_socket_is_private_and_a_second_runtime_cannot_take_it() {
    let rig = Rig::new();
    let dir_mode = std::fs::metadata(rig.sock_dir.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let sock_mode = std::fs::metadata(&rig.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!((dir_mode, sock_mode), (0o700, 0o600));
    let second = Server::bind(Arc::new(Runtime::new(config(&rig.env))), &rig.socket);
    assert!(second.err().unwrap().to_string().contains("already owns"));
    // A group- or world-accessible directory is refused outright.
    let open = tempfile::Builder::new()
        .prefix("c2o")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = Server::bind(
        Arc::new(Runtime::new(config(&rig.env))),
        &open.path().join("s.sock"),
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("accessible to other users"), "{e}");
    // An over-long path is a clear error, not a truncated socket.
    let long = open.path().join("x".repeat(120));
    assert!(Server::bind(
        Arc::new(Runtime::new(config(&rig.env))),
        &long.join("s.sock")
    )
    .err()
    .unwrap()
    .to_string()
    .contains("too long"));
    // A regular file is never replaced.
    let f = rig.sock_dir.path().join("notasocket");
    std::fs::write(&f, "x").unwrap();
    assert!(Server::bind(Arc::new(Runtime::new(config(&rig.env))), &f)
        .err()
        .unwrap()
        .to_string()
        .contains("not a socket"));
    assert!(f.exists());
}

// ===================================================== shared with the CLI

#[test]
fn the_cli_and_the_protocol_share_one_runtime_and_one_authority() {
    let rig = Rig::new();
    let mut c = rig.client();
    // A plan made through the protocol is executed by the CLI under the same digest rule.
    let p = rig.plan(&mut c, "make hello");
    let denied = rig.env.run(&rig.project, &["--json", "exec", &p.plan_id]);
    assert_eq!(
        denied.code, 3,
        "the CLI also needs approval: {}",
        denied.stdout
    );
    assert_eq!(rig.runs(), 0);
    let ok = rig.env.run(
        &rig.project,
        &["--json", "exec", &p.plan_id, "--approve", &p.digest],
    );
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(rig.runs(), 1);
    // ... and the protocol sees that execution's evidence.
    assert_eq!(
        c.task(&p.request_id).unwrap().document["results"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["status"],
        "succeeded"
    );
    assert_eq!(c.output("latest", "stdout").unwrap().content, "hello-out\n");

    // A plan made by the CLI can be approved and run through the protocol.
    let cli_plan = rig
        .env
        .run(&rig.project, &["--json", "plan", "make hello"])
        .json();
    let pid = cli_plan["plan_id"].as_str().unwrap().to_string();
    let digest = cli_plan["plan"]["digest"].as_str().unwrap().to_string();
    let run = {
        let a = c.approve(&pid, &digest).unwrap().approval;
        c.start(&pid, &a.approval_id).unwrap()
    };
    assert_eq!(
        c.wait(&run.execution_id, |_| {}).unwrap().task_status,
        Some(cli2_core::Status::Succeeded)
    );
    assert_eq!(rig.runs(), 2);

    // A plan the CLI rejects can no longer be approved through the protocol.
    let q = rig
        .env
        .run(&rig.project, &["--json", "plan", "make hello"])
        .json();
    let qid = q["plan_id"].as_str().unwrap();
    let r = rig
        .env
        .run(&rig.project, &["--json", "reject", qid, "--reason", "no"]);
    assert_eq!(r.code, 7, "cancelled: {}{}", r.stdout, r.stderr);
    assert_eq!(
        code(
            c.approve(qid, q["plan"]["digest"].as_str().unwrap())
                .unwrap_err()
        ),
        ErrorCode::PlanRejected
    );
}

// ======================================================= history and reuse

#[test]
fn prior_work_is_searchable_and_reuse_always_means_a_fresh_plan_and_a_new_approval() {
    let rig = Rig::new();
    let mut c = rig.client();
    let params = PlanParams {
        project: rig.proj(),
        intent: "make hello".into(),
        idempotency_key: None,
        constraints: Default::default(),
        bindings: Default::default(),
        selection: None,
        expectations: vec![cli2_core::Check::StdoutContains {
            text: "hello-out".into(),
        }],
        capture_help: false,
        resolver: None,
        timeout_secs: None,
    };
    let p = c.plan_with(params).unwrap().outcome.plan.unwrap();
    let run = rig.approve_and_start(&mut c, &p);
    let first_approval = c.plan_get(&p.plan_id).unwrap().approvals[0]
        .approval_id
        .clone();
    assert_eq!(
        c.wait(&run.execution_id, |_| {}).unwrap().verification,
        Some(cli2_core::VerificationStatus::Passed)
    );

    let h = c.history_search(&rig.proj(), "make hello").unwrap();
    assert_eq!(h.schema, "cli2.history_search.v1");
    let hit = &h.document["hits"][0];
    let wf = hit["workflow"]["id"].as_str().unwrap().to_string();
    assert_eq!(hit["validity"]["state"], "validated");
    let view = c.workflow(&rig.proj(), &wf).unwrap();
    assert_eq!(view.document["workflow"]["outcome"], "task_verified");

    let replan = c.workflow_replan(&rig.proj(), &wf).unwrap();
    assert_eq!(replan.document["executed"], false);
    assert_eq!(rig.runs(), 1, "re-planning ran nothing");
    let fresh = &replan.document["steps"][0]["result"]["plan"];
    let fresh_id = fresh["plan_id"].as_str().unwrap();
    assert_ne!(fresh_id, p.plan_id, "a fresh plan, not the old one");
    // The historical approval does not carry over to the fresh plan.
    assert_eq!(
        code(c.start(fresh_id, &first_approval).unwrap_err()),
        ErrorCode::ApprovalMismatch
    );
    assert_eq!(rig.runs(), 1);
}

// ===================================================== the minimal external client

#[test]
fn a_minimal_client_can_do_the_whole_journey_without_terminal_text() {
    let mut rig = Rig::new();
    // 1-2. Connect and select a project context.
    let mut c = Client::connect(&rig.socket, "minimal").unwrap();
    assert_eq!(c.server().protocol_version, 1);
    c.scan(&rig.proj()).unwrap();
    // 3-4. Submit an intent; retrieve and display the plan from typed fields.
    let plan = c.plan(&rig.proj(), "make hello").unwrap();
    let p = plan.outcome.plan.clone().unwrap();
    let shown = format!(
        "{:?} in {} (needs {:?}, digest {})",
        p.steps[0].argv(),
        p.steps[0].working_directory,
        plan.required_approval.unwrap(),
        &p.digest[..19]
    );
    assert!(
        shown.contains("make") && shown.contains("RequiresApproval") || shown.contains("Confirm"),
        "{shown}"
    );
    // 5. Reject one plan: no execution.
    let q = rig.plan(&mut c, "make hello");
    assert_eq!(
        c.reject(&q.plan_id, None).unwrap().outcome.status,
        cli2_core::Status::Cancelled
    );
    assert_eq!(rig.runs(), 0);
    // 6-7. Approve the other with the existing mechanism, observe, retrieve evidence.
    let run = rig.approve_and_start(&mut c, &p);
    // 8. Drop the connection and recover the result from a new one (and after a runtime restart).
    drop(c);
    let mut c2 = Client::connect(&rig.socket, "minimal-again").unwrap();
    let done = c2.wait(&run.execution_id, |_| {}).unwrap();
    assert_eq!(done.task_status, Some(cli2_core::Status::Succeeded));
    let evidence = c2
        .evidence(done.steps[0].stdout_ref.as_ref().unwrap())
        .unwrap();
    assert_eq!(evidence.content, "hello-out\n");
    drop(c2);
    rig.stop();
    let rt = Arc::new(Runtime::new(config(&rig.env)));
    let sock = rig.sock_dir.path().join("again.sock");
    let _s = Server::bind(rt, &sock).unwrap().spawn();
    let mut c3 = Client::connect(&sock, "minimal-later").unwrap();
    assert_eq!(
        c3.execution(&run.execution_id)
            .unwrap()
            .result
            .unwrap()
            .status,
        cli2_core::Status::Succeeded
    );
    assert_eq!(rig.runs(), 1);
}

// ================================================================ schemas

fn schemas_dir() -> PathBuf {
    repo_root().join("schemas/protocol/v1")
}

#[test]
fn published_protocol_schemas_match_the_protocol_types() {
    let update = std::env::var_os("UPDATE_SCHEMAS").is_some();
    let mut stale = Vec::new();
    for (name, schema) in cli2_protocol::schemas() {
        let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        let path = schemas_dir().join(format!("{name}.schema.json"));
        if update {
            std::fs::create_dir_all(schemas_dir()).unwrap();
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            stale.push(name);
        }
    }
    assert!(stale.is_empty(), "protocol schemas out of date: {stale:?}; run `UPDATE_SCHEMAS=1 cargo test -p cli2 --test runtime_protocol`");
    // No stray schema files for types that no longer exist.
    let known: Vec<String> = cli2_protocol::schemas()
        .iter()
        .map(|(n, _)| format!("{n}.schema.json"))
        .collect();
    for e in std::fs::read_dir(schemas_dir()).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        assert!(known.contains(&name), "unexpected schema file {name}");
    }
}

fn validate(schema_name: &str, instance: &Value) {
    let text =
        std::fs::read_to_string(schemas_dir().join(format!("{schema_name}.schema.json"))).unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = v.iter_errors(instance).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{schema_name}: {errors:?}\n{instance:#}");
}

#[test]
fn every_message_on_the_wire_conforms_to_its_published_schema() {
    let rig = Rig::new();
    let proj = rig.proj();
    let send =
        |s: &mut UnixStream, r: &mut BufReader<UnixStream>, op: &str, params: Value| -> Value {
            let req = json!({"id": "x", "op": op, "params": params});
            validate("request", &req);
            s.write_all(format!("{req}\n").as_bytes()).unwrap();
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let resp: Value = serde_json::from_str(&line).unwrap();
            validate("response", &resp);
            if resp["ok"] == true {
                resp["result"].clone()
            } else {
                validate("error", &resp["error"]);
                resp["error"].clone()
            }
        };
    let mut s = UnixStream::connect(&rig.socket).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());

    let hello = send(
        &mut s,
        &mut r,
        "hello",
        json!({"client": {"name": "schema-test"}, "versions": [1]}),
    );
    validate("hello.result", &hello);
    validate(
        "project.scan.result",
        &send(&mut s, &mut r, "project.scan", json!({"project": proj})),
    );
    validate(
        "capabilities.list.result",
        &send(
            &mut s,
            &mut r,
            "capabilities.list",
            json!({"project": proj}),
        ),
    );
    let planned = send(
        &mut s,
        &mut r,
        "intent.plan",
        json!({"project": proj, "intent": "make hello"}),
    );
    validate("intent.plan.result", &planned);
    let plan = &planned["outcome"]["plan"];
    let (plan_id, digest) = (
        plan["plan_id"].as_str().unwrap(),
        plan["digest"].as_str().unwrap(),
    );
    validate(
        "plan.get.result",
        &send(&mut s, &mut r, "plan.get", json!({"plan_id": plan_id})),
    );
    let approved = send(
        &mut s,
        &mut r,
        "plan.approve",
        json!({"plan_id": plan_id, "plan_digest": digest}),
    );
    validate("plan.approve.result", &approved);
    let started = send(
        &mut s,
        &mut r,
        "execution.start",
        json!({"plan_id": plan_id, "approval_id": approved["approval"]["approval_id"]}),
    );
    validate("execution.start.result", &started);
    let id = started["execution_id"].as_str().unwrap();
    let mut after = 0;
    let mut all_events = Vec::new();
    loop {
        let ev = send(
            &mut s,
            &mut r,
            "execution.events",
            json!({"execution_id": id, "after_seq": after, "wait_ms": 2000}),
        );
        validate("execution.events.result", &ev);
        for e in ev["events"].as_array().unwrap() {
            validate("event", e);
            after = e["seq"].as_u64().unwrap();
            all_events.push(e.clone());
        }
        if ev["finished"] == true {
            break;
        }
    }
    assert!(
        all_events.iter().any(|e| e["type"] == "output")
            && all_events.last().unwrap()["type"] == "execution_finished"
    );
    validate(
        "execution.get.result",
        &send(&mut s, &mut r, "execution.get", json!({"execution_id": id})),
    );
    validate(
        "execution.cancel.result",
        &send(
            &mut s,
            &mut r,
            "execution.cancel",
            json!({"execution_id": id}),
        ),
    );
    validate(
        "document.result",
        &send(&mut s, &mut r, "task.list", json!({"limit": 3})),
    );
    validate(
        "document.result",
        &send(
            &mut s,
            &mut r,
            "task.get",
            json!({"id": planned["outcome"]["request_id"]}),
        ),
    );
    validate(
        "content.result",
        &send(
            &mut s,
            &mut r,
            "output.read",
            json!({"output_id": "latest", "part": "stdout"}),
        ),
    );
    validate(
        "document.result",
        &send(
            &mut s,
            &mut r,
            "history.search",
            json!({"project": proj, "text": "make hello"}),
        ),
    );
    let rej = send(
        &mut s,
        &mut r,
        "intent.plan",
        json!({"project": proj, "intent": "make hello"}),
    );
    validate(
        "plan.reject.result",
        &send(
            &mut s,
            &mut r,
            "plan.reject",
            json!({"plan_id": rej["outcome"]["plan"]["plan_id"], "reason": "x"}),
        ),
    );
    // Errors conform too.
    let err = send(
        &mut s,
        &mut r,
        "plan.get",
        json!({"plan_id": "plan_missing"}),
    );
    assert_eq!(err["code"], "not_found");
}

#[test]
fn development_task_messages_conform_to_their_published_schemas() {
    use cli2_protocol::devtask::*;
    let rig = Rig::new();
    let proj = rig.proj();
    let mut s = UnixStream::connect(&rig.socket).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut send = |op: &str, params: Value| -> Value {
        let req = json!({"id": "x", "op": op, "params": params});
        validate("request", &req);
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        validate("response", &resp);
        if resp["ok"] == true {
            resp["result"].clone()
        } else {
            validate("error", &resp["error"]);
            resp["error"].clone()
        }
    };
    send(
        "hello",
        json!({"client": {"name": "schema"}, "versions": [1]}),
    );
    let create = json!({
        "project": proj, "goal": "greet",
        "criteria": [{"id": "greeted", "description": "it greets", "checks": [{"type": "stdout_contains", "text": "hello-out"}]}],
        "outline": [{"kind": "verify", "objective": "greet", "intent": "make hello", "satisfies": ["greeted"]}]
    });
    validate("devtask.create.params", &create);
    let view = send("devtask.create", create);
    validate("devtask.view", &view);
    let id = view["task_id"].as_str().unwrap().to_string();
    validate(
        "devtask.view",
        &send("devtask.start", json!({"task_id": id})),
    );
    validate(
        "devtask.list.result",
        &send("devtask.list", json!({"limit": 5})),
    );
    // Wait for the proposal, approve it by digest, and follow the events to the end.
    let mut after = 0;
    let mut all = Vec::new();
    let t = std::time::Instant::now();
    loop {
        let v = send("devtask.get", json!({"task_id": id}));
        validate("devtask.view", &v);
        if v["state"] == "awaiting_approval" {
            let step = &v["steps"][0];
            assert!(
                v["current_plan"]["plan_id"].is_string(),
                "the full plan is part of the view"
            );
            validate(
                "devtask.view",
                &send(
                    "devtask.approve",
                    json!({"task_id": id, "step_id": step["step_id"], "plan_digest": step["plan_digest"]}),
                ),
            );
        }
        let ev = send(
            "devtask.events",
            json!({"task_id": id, "after_seq": after, "wait_ms": 300}),
        );
        validate("devtask.events.result", &ev);
        for e in ev["events"].as_array().unwrap() {
            validate("devtask.event", e);
            after = e["seq"].as_i64().unwrap();
            all.push(e.clone());
        }
        if ev["finished"] == true {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "stuck at {}",
            v["state"]
        );
    }
    assert_eq!(all.last().unwrap()["type"], "completed");
    validate(
        "devtask.report",
        &send("devtask.report", json!({"task_id": id})),
    );
    let err = send("devtask.get", json!({"task_id": "dev_nope"}));
    assert_eq!(err["code"], "not_found");
    // The unknown-field rule applies here too.
    assert_eq!(
        send(
            "devtask.approve",
            json!({"task_id": id, "step_id": "s1", "plan_digest": "d", "yes": true})
        )["code"],
        "invalid_params"
    );
    let _: TaskView = serde_json::from_value(send("devtask.get", json!({"task_id": id}))).unwrap();
}

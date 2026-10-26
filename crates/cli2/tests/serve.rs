//! The real `cli2 serve` process: readiness, clean shutdown with a running
//! execution, and recovery after a crash.

mod support;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use cli2_client::Client;
use cli2_protocol::*;
use serde_json::Value;
use support::*;

const MAKEFILE: &str = ".PHONY: slow\nslow: ## Wait a long time\n\t@echo started; sleep 30\n";

struct Served {
    child: Child,
    ready: Value,
}

fn serve(env: &Env, project: &Path, socket: &Path) -> Served {
    let mut child = env
        .command(project, &["serve", "--socket", socket.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: Value =
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("no ready line ({e}): {line:?}"));
    assert_eq!(ready["event"], "ready");
    Served { child, ready }
}

fn setup() -> (Env, PathBuf, tempfile::TempDir, PathBuf) {
    let env = Env::new();
    let project = env.work.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("Makefile"), MAKEFILE).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("c2v")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("s.sock");
    (env, project.canonicalize().unwrap(), dir, socket)
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn start_slow(c: &mut Client, project: &str) -> (StartResult, u32) {
    let p = c.plan(project, "make slow").unwrap().outcome.plan.unwrap();
    let a = c.approve(&p.plan_id, &p.digest).unwrap().approval;
    let run = c.start(&p.plan_id, &a.approval_id).unwrap();
    let mut seen = 0;
    let t = Instant::now();
    loop {
        let b = c.events(&run.execution_id, seen, 1000).unwrap();
        for e in &b.events {
            seen = e.seq;
            if let EventData::StepStarted { pid, .. } = &e.data {
                // The command has really started once it prints.
                let _ = pid;
            }
        }
        let started = b.events.iter().find_map(|e| match &e.data {
            EventData::Output { text, .. } if text.contains("started") => Some(()),
            _ => None,
        });
        if started.is_some() {
            let snap_pid = c
                .events(&run.execution_id, 0, 0)
                .unwrap()
                .events
                .iter()
                .find_map(|e| match &e.data {
                    EventData::StepStarted { pid, .. } => Some(*pid),
                    _ => None,
                });
            return (run, snap_pid.unwrap());
        }
        assert!(t.elapsed() < Duration::from_secs(20), "never started");
    }
}

#[test]
fn a_clean_shutdown_cancels_and_records_the_running_execution() {
    let (env, project, _dir, socket) = setup();
    let mut s = serve(&env, &project, &socket);
    assert_eq!(s.ready["protocol_versions"], serde_json::json!([1]));
    assert_eq!(s.ready["socket"], socket.to_str().unwrap());
    let mut c = Client::connect(&socket, "t").unwrap();
    let (run, pid) = start_slow(&mut c, project.to_str().unwrap());
    assert!(alive(pid));

    // SAFETY: signalling the server process this test spawned.
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    let status = s.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!socket.exists(), "the socket is removed on shutdown");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(pid), "the command was terminated, not orphaned");

    // The outcome was recorded: cancelled, never success.
    let mut s2 = serve(&env, &project, &socket);
    assert_eq!(
        s2.ready["reconciled_executions"],
        serde_json::json!([]),
        "nothing was left unrecorded"
    );
    let mut c2 = Client::connect(&socket, "t2").unwrap();
    let snap = c2.execution(&run.execution_id).unwrap();
    assert_eq!(snap.source, "journal");
    assert_eq!(snap.task_status, Some(cli2_core::Status::Cancelled));
    assert_eq!(
        snap.process.unwrap().termination,
        cli2_core::Termination::Cancelled
    );
    let _ = s2.child.kill();
    let _ = s2.child.wait();
}

#[test]
fn after_a_crash_the_next_runtime_reconciles_the_orphaned_execution_without_claiming_success() {
    let (env, project, _dir, socket) = setup();
    let mut s = serve(&env, &project, &socket);
    let mut c = Client::connect(&socket, "t").unwrap();
    let (run, pid) = start_slow(&mut c, project.to_str().unwrap());

    // The runtime dies without any chance to clean up.
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    assert!(socket.exists(), "a crash leaves a stale socket behind");
    // (The orphaned command, in its own process group, is cleaned up by the test.)
    // SAFETY: terminating the process group of the command this test started.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };

    let mut s2 = serve(&env, &project, &socket);
    assert_eq!(
        s2.ready["reconciled_executions"],
        serde_json::json!([run.execution_id])
    );
    let mut c2 = Client::connect(&socket, "t2").unwrap();
    let snap = c2.execution(&run.execution_id).unwrap();
    assert_eq!(snap.lifecycle, Lifecycle::InterruptedUnknown);
    assert_eq!(snap.task_status, Some(cli2_core::Status::Cancelled));
    assert_ne!(
        snap.verification,
        Some(cli2_core::VerificationStatus::Passed)
    );
    assert!(snap
        .result
        .unwrap()
        .message
        .contains("no success is claimed"));
    // The evidence journal is intact.
    assert_eq!(
        c2.task(&run.request_id).unwrap().document["journal_integrity"],
        "verified"
    );
    // A second server on the same socket is refused while one is running.
    let again = env.run(&project, &["serve", "--socket", socket.to_str().unwrap()]);
    assert_eq!(again.code, 2, "{}", again.stderr);
    assert!(again.stderr.contains("already owns"), "{}", again.stderr);
    let _ = s2.child.kill();
    let _ = s2.child.wait();
}

#[test]
fn the_cli_works_without_any_runtime_running_and_protocol_schemas_are_printable() {
    let (env, project, _dir, _socket) = setup();
    // No `serve` anywhere: the CLI is a complete client on its own.
    let r = env.run(&project, &["--json", "plan", "make slow"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["status"], "planned");
    let s = env.run(&project, &["schema", "protocol.event"]);
    assert_eq!(s.code, 0, "{}", s.stderr);
    assert_eq!(s.json()["title"], "Event");
    assert_eq!(env.run(&project, &["schema", "protocol.nope"]).code, 2);
}

#[test]
fn an_independent_python_client_can_use_the_documented_protocol() {
    if which("python3").is_none() {
        eprintln!("SKIPPED: python3 is not installed");
        return;
    }
    let env = Env::new();
    let project = env.work.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("Makefile"),
        ".PHONY: hi\nhi: ## Say hi\n\t@echo hi-out; echo hi-err >&2\n",
    )
    .unwrap();
    let project = project.canonicalize().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("c2p")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("s.sock");
    let mut s = serve(&env, &project, &socket);
    let script = repo_root().join("docs/examples/client.py");
    let py = |extra: &[&str]| {
        let mut a = vec![
            script.to_str().unwrap(),
            socket.to_str().unwrap(),
            project.to_str().unwrap(),
            "make hi",
        ];
        a.extend_from_slice(extra);
        std::process::Command::new(which("python3").unwrap())
            .args(a)
            .output()
            .unwrap()
    };
    // Plan only: nothing runs.
    let plan_only = py(&[]);
    let text = String::from_utf8_lossy(&plan_only.stdout).into_owned();
    assert!(
        plan_only.status.success(),
        "{}",
        String::from_utf8_lossy(&plan_only.stderr)
    );
    assert!(
        text.contains("status=planned") && text.contains("not approved; nothing executed"),
        "{text}"
    );
    assert_eq!(
        env.run(&project, &["output", "list"]).stdout.trim(),
        "No captured results yet."
    );
    // Reject: nothing runs.
    let rejected = String::from_utf8_lossy(&py(&["--reject"]).stdout).into_owned();
    assert!(rejected.contains("rejected: cancelled"), "{rejected}");
    assert_eq!(
        env.run(&project, &["output", "list"]).stdout.trim(),
        "No captured results yet."
    );
    // Approve: runs, streams, and reports the three statuses separately.
    let ran = py(&["--approve"]);
    let out = String::from_utf8_lossy(&ran.stdout).into_owned();
    assert!(
        ran.status.success(),
        "{}{}",
        out,
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        out.contains("[stdout] hi-out") && out.contains("[stderr] hi-err"),
        "{out}"
    );
    let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(last["process"]["exit_code"], 0);
    assert_eq!(last["task_status"], "succeeded");
    assert_eq!(
        last["verification"], "unverified",
        "exit 0 was not treated as verified success"
    );
    let _ = s.child.kill();
    let _ = s.child.wait();
}

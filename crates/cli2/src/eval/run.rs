//! Running the suite: sandboxes, attempts, metrics and the report.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use cli2_core::{
    ApprovalLevel, Capability, Check, ExecutionResult, Plan, RequestInput, Status, Termination,
    VerificationStatus,
};
use cli2_execution::Policy;
use sha2::{Digest, Sha256};

use super::*;
use crate::engine::{Config, Engine, EngineError, NonInteractive, RunOptions};
use crate::output::{started_steps, OutputId, Part};
use crate::semantic::{status::model_status, Provider, ResolverMode, ResolverSettings};
use crate::workflow::engine_ext::SearchOptions;

/// Host executables every sandbox may use (fixture Makefiles and scripts).
const BASE_TOOLS: &[&str] = &[
    "sh", "env", "make", "cat", "ls", "mkdir", "rm", "cp", "mv", "touch", "echo", "printf", "sed",
    "tr", "head", "tail", "wc", "sort", "test", "true", "false", "uname", "dirname", "basename",
    "sleep", "grep", "node", "npm",
];

#[derive(Debug, Clone, Default)]
pub struct SuiteOptions {
    /// Run only the case with this ID.
    pub only: Option<String>,
    /// Retain sandboxes (journal, blobs, index, project) under this directory.
    pub out_dir: Option<PathBuf>,
    /// Also run the real on-device model comparison when it is ready.
    pub model: bool,
    /// The matching evaluation set, if any.
    pub matching: Option<PathBuf>,
}

fn host_which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        let to = dst.join(&name);
        if e.file_type()?.is_dir() {
            if matches!(
                name.to_str(),
                Some("target" | "node_modules" | "out" | "dist" | ".git")
            ) {
                continue;
            }
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    proj: PathBuf,
    path_var: OsString,
}

fn git(git_bin: &Path, root: &Path, proj: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(git_bin)
        .args(args)
        .current_dir(proj)
        .env_clear()
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "eval")
        .env("GIT_AUTHOR_EMAIL", "eval@example.invalid")
        .env("GIT_COMMITTER_NAME", "eval")
        .env("GIT_COMMITTER_EMAIL", "eval@example.invalid")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn apply_edit(proj: &Path, e: &Edit) -> Result<(), String> {
    let path = proj.join(&e.path);
    let mut text = std::fs::read_to_string(&path).map_err(|er| format!("{}: {er}", e.path))?;
    if let Some(a) = &e.append {
        text.push_str(a);
    }
    if let (Some(from), Some(to)) = (&e.from, &e.to) {
        if !text.contains(from.as_str()) {
            return Err(format!("{}: text to replace not found", e.path));
        }
        text = text.replacen(from.as_str(), to, 1);
    }
    std::fs::write(&path, text).map_err(|er| er.to_string())
}

/// Create an isolated sandbox for a case. `Err` is a reason to skip.
fn make_sandbox(repo: &Path, case: &Case, base: &Path, label: &str) -> Result<Sandbox, String> {
    for t in case.requires.iter().chain(case.host_tools.iter()) {
        if host_which(t).is_none() {
            return Err(format!("`{t}` is not installed on this host"));
        }
    }
    if host_which("make").is_none() && case.fixture == "dev-project" {
        return Err("`make` is not installed on this host".into());
    }
    let git_bin = if case.git || case.host_tools.iter().any(|t| t == "git") {
        Some(host_which("git").ok_or_else(|| "`git` is not installed on this host".to_string())?)
    } else {
        None
    };
    let root = base.join(&case.id).join(label);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    // Canonical paths: the engine canonicalizes working directories, and a
    // symlinked temp directory (/var vs /private/var) must not split one
    // project into two index scopes.
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let (home, bin, proj) = (root.join("home"), root.join("bin"), root.join("project"));
    for d in [&home, &bin] {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    copy_dir(&repo.join("fixtures").join(&case.fixture), &proj)
        .map_err(|e| format!("fixture {}: {e}", case.fixture))?;
    // Hermetic PATH: only what the case needs.
    let mut tools: BTreeSet<&str> = BASE_TOOLS.iter().copied().collect();
    tools.extend(case.host_tools.iter().map(String::as_str));
    if git_bin.is_some() {
        tools.insert("git");
    }
    for t in tools {
        if let Some(p) = host_which(t) {
            let _ = std::os::unix::fs::symlink(p, bin.join(t));
        }
    }
    for t in &case.tools {
        let src = repo.join("fixtures/tools").join(t);
        std::fs::copy(&src, bin.join(t)).map_err(|e| format!("fixture tool {t}: {e}"))?;
    }
    if let Some(g) = &git_bin {
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "initial"],
        ] {
            git(g, &root, &proj, &args).map_err(|e| format!("git {args:?}: {e}"))?;
        }
    }
    for e in &case.edits {
        apply_edit(&proj, e)?;
    }
    Ok(Sandbox {
        path_var: std::env::join_paths([&bin]).map_err(|e| e.to_string())?,
        root,
        home,
        proj,
    })
}

fn open_engine(sb: &Sandbox, provider: Provider) -> Result<Engine, EngineError> {
    let mut c = Config::from_env()?;
    c.home = sb.home.clone();
    c.path_var = sb.path_var.clone();
    c.policy = Policy::default();
    c.max_output_bytes = None;
    c.resolver = Some(ResolverSettings {
        provider,
        ..Default::default()
    });
    let mut e = Engine::open(c)?;
    e.set_resolver_mode(if provider == Provider::AppleFoundationModels {
        ResolverMode::AppleFm
    } else {
        ResolverMode::Deterministic
    });
    Ok(e)
}

type Snapshot = BTreeMap<String, (u64, String)>;

fn snapshot(proj: &Path) -> Snapshot {
    fn walk(base: &Path, dir: &Path, out: &mut Snapshot) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if rel == ".git" {
                continue; // git's own bookkeeping is not the project's content
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    out.insert(format!("{rel}/"), (0, String::new()));
                    walk(base, &p, out);
                }
                Ok(_) => {
                    let bytes = std::fs::read(&p).unwrap_or_default();
                    out.insert(
                        rel,
                        (bytes.len() as u64, hex::encode(Sha256::digest(&bytes))),
                    );
                }
                Err(_) => {}
            }
        }
    }
    let mut out = Snapshot::new();
    walk(proj, proj, &mut out);
    out
}

fn changed_paths(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut v: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    v.sort();
    v
}

fn expectations(args: &[String]) -> Vec<Check> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--expect-stdout", Some(v)) => out.push(Check::StdoutContains { text: v.clone() }),
            ("--expect-file", Some(v)) => out.push(Check::FileExists {
                path: v.clone(),
                modified_after_start: true,
            }),
            _ => {}
        }
    }
    out
}

fn request(intent: &str, proj: &Path, args: &[String]) -> RequestInput {
    let mut r = RequestInput::new(intent);
    r.context.working_directory = Some(proj.display().to_string());
    r.expectations = expectations(args);
    r
}

fn lower<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Classify why a request did not produce a plan, by the earliest stage responsible.
pub fn classify_unplanned(
    r: &ExecutionResult,
    acceptable: &[String],
    inventory: &BTreeSet<String>,
) -> Failure {
    let q = r
        .clarification
        .as_ref()
        .map(|c| c.question.clone())
        .unwrap_or_else(|| r.message.clone());
    let stage = match r.status {
        Status::Rejected => Stage::Authority,
        _ if !acceptable.is_empty() && !acceptable.iter().any(|a| inventory.contains(a)) => {
            Stage::Discovery
        }
        _ if q.contains("interface of")
            || q.contains("only part of the interface")
            || q.contains("not characterized")
            || q.contains("could not be learned") =>
        {
            Stage::Interface
        }
        _ if q.contains("missing required input") || q.contains("cannot be planned yet") => {
            Stage::Planning
        }
        _ => Stage::Resolution,
    };
    let detail = if r.status == Status::Rejected {
        format!("rejected: {}", r.rejection.join("; "))
    } else {
        q.chars().take(240).collect()
    };
    Failure { stage, detail }
}

fn first_failure(a: &mut Attempt, stage: Stage, detail: impl Into<String>) {
    if a.failure.is_none() {
        a.failure = Some(Failure {
            stage,
            detail: detail.into(),
        });
    }
}

fn argument_refs(caps: &BTreeMap<&str, &Capability>, plan: &Plan) -> u32 {
    let mut n = 0;
    for s in plan.steps.iter().filter_map(|s| s.as_process()) {
        let Some(c) = caps.get(s.capability_id.as_str()) else {
            continue;
        };
        let fixed = &c.interface.arguments;
        if s.arguments.len() < fixed.len() || s.arguments[..fixed.len()] != fixed[..] {
            n += 1;
            continue;
        }
        let extra = &s.arguments[fixed.len()..];
        let max = if c.interface.parameters.iter().any(|p| p.variadic) {
            usize::MAX
        } else {
            c.interface.parameters.len()
        };
        if extra.len() > max || extra.iter().any(|a| a.starts_with('-')) {
            n += 1;
        }
    }
    n
}

fn output_text(engine: &Engine, r: &ExecutionResult) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut ids = Vec::new();
    if let Ok(Some(seq)) = engine.result_seq(&r.request_id) {
        for s in started_steps(r) {
            let id = OutputId { seq, step: s.index }.format();
            for part in [Part::Stdout, Part::Stderr] {
                if let Ok(b) = engine.output_part(&id, part) {
                    text.push_str(&String::from_utf8_lossy(&b));
                }
            }
            ids.push(id);
        }
    }
    (text, ids)
}

struct Prepared {
    sb: Sandbox,
    engine: Engine,
    inventory: BTreeSet<String>,
    caps: Vec<Capability>,
    discovery_ok: bool,
}

fn prepare(sb: Sandbox, case: &Case, provider: Provider) -> Result<Prepared, Failure> {
    let fail = |stage, detail: String| Failure { stage, detail };
    let mut engine =
        open_engine(&sb, provider).map_err(|e| fail(Stage::Discovery, e.to_string()))?;
    engine
        .scan(&sb.proj)
        .map_err(|e| fail(Stage::Discovery, format!("scan failed: {e}")))?;
    for exe in &case.capture_help {
        // The user's explicit consent to run `<exe> --help`; failures are recorded, not retried.
        if let Err(e) = engine.capture_interface(&format!("executable:{exe}"), &sb.proj, false) {
            return Err(fail(
                Stage::Interface,
                format!("capturing the interface of {exe}: {e}"),
            ));
        }
    }
    let caps = engine
        .inventory(&sb.proj)
        .map_err(|e| fail(Stage::Discovery, e.to_string()))?;
    let inventory: BTreeSet<String> = caps.iter().map(|c| c.id.clone()).collect();
    let discovery_ok =
        case.capabilities.is_empty() || case.capabilities.iter().any(|c| inventory.contains(c));
    Ok(Prepared {
        sb,
        engine,
        inventory,
        caps,
        discovery_ok,
    })
}

/// Checks on a plan; returns whether it is safe to go on to authority and execution.
fn check_plan(a: &mut Attempt, prep: &Prepared, case: &Case, res: &ExecutionResult) -> bool {
    a.plan_status = lower(&res.status);
    if res.status != Status::Planned {
        let f = classify_unplanned(res, &case.capabilities, &prep.inventory);
        a.selected = res
            .clarification
            .as_ref()
            .and_then(|c| c.options.first())
            .map(|o| o.capability_id.clone());
        if !case.capabilities.is_empty() {
            a.selection = Some(
                a.selected
                    .as_ref()
                    .is_some_and(|s| case.capabilities.contains(s)),
            );
        }
        first_failure(a, f.stage, f.detail);
        return false;
    }
    let Some(plan) = &res.plan else {
        first_failure(a, Stage::Planning, "planned without a plan");
        return false;
    };
    let caps: BTreeMap<&str, &Capability> = prep.caps.iter().map(|c| (c.id.as_str(), c)).collect();
    let selected = plan.steps.first().map(|s| s.capability_id().to_string());
    a.selected = selected.clone();
    a.argv = plan
        .steps
        .first()
        .and_then(|s| s.as_process())
        .map(|s| s.argv());
    a.approval_level = Some(lower(&plan.approval.level));
    a.invalid_capability_refs = plan
        .steps
        .iter()
        .map(|s| s.capability_id())
        .chain(plan.candidates.iter().map(|c| c.capability_id.as_str()))
        .filter(|id| !prep.inventory.contains(*id))
        .count() as u32;
    a.invalid_argument_refs = argument_refs(&caps, plan);
    let valid = plan.is_intact()
        && !plan.steps.is_empty()
        && plan.steps.iter().all(|s| {
            s.as_process()
                .is_some_and(|p| Path::new(&p.resolved_executable).is_file())
        })
        && a.invalid_capability_refs == 0
        && a.invalid_argument_refs == 0;
    a.plan_valid = Some(valid);
    let mut ok = true;
    if !case.capabilities.is_empty() {
        let sel_ok = selected
            .as_ref()
            .is_some_and(|s| case.capabilities.contains(s));
        a.selection = Some(sel_ok);
        if !sel_ok {
            first_failure(
                a,
                Stage::Resolution,
                format!(
                    "selected {:?}; expected one of {:?}",
                    selected, case.capabilities
                ),
            );
            ok = false;
        }
    }
    if !valid {
        first_failure(
            a,
            Stage::Planning,
            "the plan failed validation (integrity, executable, or references)",
        );
        ok = false;
    }
    if let Some(want) = &case.plan.argv {
        let good = a.argv.as_ref() == Some(want);
        a.argument = Some(good);
        if !good {
            first_failure(
                a,
                Stage::Planning,
                format!("argv {:?}; expected {:?}", a.argv, want),
            );
            ok = false;
        }
    }
    if let Some(want) = &case.plan.approval {
        if &lower(&plan.approval.level) != want {
            first_failure(
                a,
                Stage::Authority,
                format!(
                    "plan requires {} approval; expected {want}",
                    lower(&plan.approval.level)
                ),
            );
            a.approval = Some(false);
            ok = false;
        }
    }
    ok
}

/// Without approval nothing may run; `--yes` must not satisfy explicit approval.
fn authority_probe(
    a: &mut Attempt,
    prep: &mut Prepared,
    req: &RequestInput,
    level: ApprovalLevel,
    before: &Snapshot,
) {
    let mut enforced = true;
    let check = |yes: bool, a: &mut Attempt, prep: &mut Prepared| {
        let r = prep.engine.run(
            req.clone(),
            &RunOptions {
                execute: true,
                yes,
                ..Default::default()
            },
            &mut NonInteractive,
        );
        match r {
            Ok(r) => {
                a.request_ids.push(r.request_id.clone());
                if started_steps(&r).next().is_some() {
                    a.unauthorized_executions += 1;
                    first_failure(
                        a,
                        Stage::Authority,
                        format!("executed without the required approval (yes={yes})"),
                    );
                    return false;
                }
                r.status == Status::AwaitingApproval
            }
            Err(e) => {
                first_failure(a, Stage::Authority, format!("authority probe errored: {e}"));
                false
            }
        }
    };
    match level {
        ApprovalLevel::None => {}
        ApprovalLevel::Confirm => enforced &= check(false, a, prep),
        ApprovalLevel::Explicit => {
            enforced &= check(false, a, prep);
            enforced &= check(true, a, prep);
        }
        ApprovalLevel::Forbidden => enforced &= check(true, a, prep),
    }
    let after = snapshot(&prep.sb.proj);
    let changed = changed_paths(before, &after);
    if !changed.is_empty() {
        a.unintended_side_effects
            .extend(changed.iter().map(|p| format!("(authority probe) {p}")));
        first_failure(
            a,
            Stage::Authority,
            format!("the unapproved attempt changed the project: {changed:?}"),
        );
        enforced = false;
    }
    let level_ok = a.approval != Some(false);
    a.approval = Some(level_ok && enforced);
}

/// Process, verification, diagnostics and side effects of an executed attempt.
fn check_execution(
    a: &mut Attempt,
    prep: &Prepared,
    case: &Case,
    res: &ExecutionResult,
    before: &Snapshot,
) {
    let steps: Vec<_> = started_steps(res).collect();
    a.executed = !steps.is_empty();
    a.request_ids.push(res.request_id.clone());
    if steps.is_empty() {
        first_failure(
            a,
            Stage::Execution,
            format!("the plan did not execute (status {})", lower(&res.status)),
        );
        a.execution_completed = Some(false);
        return;
    }
    let last = steps.last().unwrap();
    a.exit_code = last.exit_code;
    a.execution_completed = Some(steps.iter().all(|s| s.termination == Termination::Exited));
    a.process_exit_zero =
        Some(last.exit_code == Some(0) && last.termination == Termination::Exited);
    a.verification_status = Some(lower(&res.verification.status));
    if a.execution_completed != Some(true) {
        first_failure(
            a,
            Stage::Execution,
            format!("the process did not complete ({:?})", last.termination),
        );
    }
    if let Some(want) = &case.expect.process {
        let ok = match want.as_str() {
            "succeeded" => a.process_exit_zero == Some(true),
            "failed" => {
                a.process_exit_zero == Some(false)
                    && case
                        .expect
                        .exit_code
                        .is_none_or(|c| last.exit_code == Some(c))
            }
            _ => true,
        };
        a.process_as_expected = Some(ok);
        if !ok {
            first_failure(
                a,
                Stage::Execution,
                format!(
                    "process outcome {:?}/exit {:?}; expected {want}",
                    last.termination, last.exit_code
                ),
            );
        }
    }
    if let Some(want) = &case.expect.task {
        let st = res.verification.status;
        let ok = match want.as_str() {
            "verified" => res.status == Status::Succeeded && st == VerificationStatus::Passed,
            "unverified" => res.status == Status::Succeeded && st == VerificationStatus::Unverified,
            "failed_verification" => res.status == Status::VerificationFailed,
            "not_run" => st == VerificationStatus::NotRun,
            // The process failed: the request failed, and nothing was verified.
            "process_failed" => res.status == Status::Failed && st != VerificationStatus::Passed,
            _ => true,
        };
        if want == "verified" {
            a.verification = Some(ok);
        }
        if !ok {
            first_failure(
                a,
                Stage::Verification,
                format!(
                    "task status {} / verification {}; expected {want}",
                    lower(&res.status),
                    lower(&st)
                ),
            );
        }
    }
    // Success must never be claimed for a failed process or an unverified task.
    let claims_success = res.status == Status::Succeeded;
    if claims_success && a.process_exit_zero == Some(false) {
        first_failure(
            a,
            Stage::Verification,
            "reported success although the process failed",
        );
    }
    let (text, ids) = output_text(&prep.engine, res);
    a.output_ids = ids;
    for needle in &case.expect.output_contains {
        if !text.contains(needle.as_str()) {
            first_failure(
                a,
                Stage::Verification,
                format!("expected diagnostic output {needle:?} was not captured"),
            );
        }
    }
    let after = snapshot(&prep.sb.proj);
    let unintended: Vec<String> = changed_paths(before, &after)
        .into_iter()
        .filter(|p| {
            !case
                .side_effects_allowed
                .iter()
                .any(|allowed| p.starts_with(allowed.as_str()))
        })
        .collect();
    if !unintended.is_empty() {
        first_failure(
            a,
            Stage::Execution,
            format!("unintended side effects: {unintended:?}"),
        );
        a.unintended_side_effects.extend(unintended);
    }
}

fn fresh_attempt(mut prep: Prepared, case: &Case, query: &str, approach: Approach) -> Attempt {
    let started = Instant::now();
    let mut a = Attempt::new(approach, query);
    a.discovery = Some(prep.discovery_ok);
    if !prep.discovery_ok {
        first_failure(
            &mut a,
            Stage::Discovery,
            format!("none of {:?} was discovered", case.capabilities),
        );
        return a;
    }
    let req = request(query, &prep.sb.proj, &case.args);
    let before = snapshot(&prep.sb.proj);
    let planned = prep.engine.run(
        req.clone(),
        &RunOptions {
            execute: false,
            ..Default::default()
        },
        &mut NonInteractive,
    );
    let res = match planned {
        Ok(r) => r,
        Err(e) => {
            first_failure(
                &mut a,
                Stage::Resolution,
                format!("the request failed: {e}"),
            );
            a.latency_ms = started.elapsed().as_millis() as u64;
            return a;
        }
    };
    a.request_ids.push(res.request_id.clone());
    let planning_changes = changed_paths(&before, &snapshot(&prep.sb.proj));
    if !planning_changes.is_empty() {
        first_failure(
            &mut a,
            Stage::Planning,
            format!("planning changed the project: {planning_changes:?}"),
        );
        a.unintended_side_effects.extend(planning_changes);
    }
    if !check_plan(&mut a, &prep, case, &res) {
        a.latency_ms = started.elapsed().as_millis() as u64;
        return a;
    }
    let level = res
        .plan
        .as_ref()
        .map(|p| p.approval.level)
        .unwrap_or(ApprovalLevel::Explicit);
    if a.approval.is_none() {
        a.approval = Some(true);
    }
    authority_probe(&mut a, &mut prep, &req, level, &before);
    if a.failure.is_none() && case.execute {
        let r = prep.engine.run(
            req,
            &RunOptions {
                execute: true,
                yes: level != ApprovalLevel::Explicit,
                ..Default::default()
            },
            &mut NonInteractive,
        );
        match r {
            Ok(r) => check_execution(&mut a, &prep, case, &r, &before),
            Err(e) => first_failure(&mut a, Stage::Execution, format!("execution errored: {e}")),
        }
    }
    a.latency_ms = started.elapsed().as_millis() as u64;
    a
}

struct HistoryRun<'a> {
    case: &'a Case,
    query: &'a str,
    prior: &'a str,
    mutations: &'a [HistoryMutation],
    query_in_copy: bool,
    expect_validity: &'a str,
    expect_replan_blocked: bool,
    expect_retrieval: bool,
}

/// B: record a verified prior execution, change the project if the case says so,
/// retrieve the workflow for `query`, validate it, re-plan it from scratch, and
/// (if the case permits) execute the *fresh plan* with approval.
fn history_attempt(mut prep: Prepared, h: HistoryRun<'_>, repo_label: &Path) -> Attempt {
    let started = Instant::now();
    let case = h.case;
    let mut a = Attempt::new(Approach::HistoryReplan, h.query);
    a.discovery = Some(prep.discovery_ok);
    let args = &case.args;
    // The prior run: executed with approval, expected to be verified.
    let prior_req = request(h.prior, &prep.sb.proj, args);
    let prior = match prep.engine.run(
        prior_req,
        &RunOptions {
            execute: true,
            yes: true,
            ..Default::default()
        },
        &mut NonInteractive,
    ) {
        Ok(r) => r,
        Err(e) => {
            first_failure(
                &mut a,
                Stage::History,
                format!("the prior execution errored: {e}"),
            );
            return a;
        }
    };
    if prior.status != Status::Succeeded || prior.verification.status != VerificationStatus::Passed
    {
        first_failure(
            &mut a,
            Stage::History,
            format!(
                "no verified prior execution to learn from (status {}, verification {})",
                lower(&prior.status),
                lower(&prior.verification.status)
            ),
        );
        return a;
    }
    let seq = prep
        .engine
        .result_seq(&prior.request_id)
        .ok()
        .flatten()
        .unwrap_or(0);
    let prior_wf = format!("wf_e{seq}");
    for m in h.mutations {
        let edit = Edit {
            path: m.path.clone(),
            append: m.append.clone(),
            from: m.from.clone(),
            to: m.to.clone(),
        };
        if let Err(e) = apply_edit(&prep.sb.proj, &edit) {
            first_failure(
                &mut a,
                Stage::History,
                format!("could not apply the scenario change: {e}"),
            );
            return a;
        }
    }
    // Where the user is when they ask again.
    let mut cwd = prep.sb.proj.clone();
    if h.query_in_copy {
        let second = prep.sb.root.join("second-copy");
        let _ = repo_label;
        if copy_dir(&prep.sb.proj, &second).is_err() {
            first_failure(
                &mut a,
                Stage::History,
                "could not create the second project copy",
            );
            return a;
        }
        cwd = second;
    }
    let before = snapshot(&cwd);
    // Retrieval.
    let search = prep.engine.search_history(
        h.query,
        &cwd,
        &SearchOptions {
            limit: 5,
            verified_only: false,
            include_batch_steps: false,
            freshness_budget: None,
        },
        None,
    );
    let hit = match search {
        Ok(s) => s.hits.into_iter().find(|hit| hit.workflow.id == prior_wf),
        Err(e) => {
            first_failure(
                &mut a,
                Stage::History,
                format!("history search failed: {e}"),
            );
            return a;
        }
    };
    let Some(hit) = hit else {
        a.retrieval = Some(false);
        if h.expect_retrieval {
            first_failure(
                &mut a,
                Stage::History,
                format!(
                    "the prior workflow {prior_wf} was not retrieved for {:?}",
                    h.query
                ),
            );
        } else {
            a.retrieval = Some(true); // correctly not retrieved
        }
        a.latency_ms = started.elapsed().as_millis() as u64;
        return a;
    };
    let top_is_prior = true;
    a.retrieval = Some(h.expect_retrieval && top_is_prior && hit.workflow.eligibility.eligible);
    if !h.expect_retrieval {
        a.retrieval = Some(false);
        first_failure(
            &mut a,
            Stage::History,
            format!(
                "{prior_wf} was retrieved although it should not match {:?}",
                h.query
            ),
        );
    }
    let validity = format!("{:?}", hit.validity.state).to_lowercase();
    a.validity = Some(validity.clone());
    if validity != h.expect_validity {
        first_failure(
            &mut a,
            Stage::History,
            format!(
                "validity {validity}; expected {} ({})",
                h.expect_validity,
                hit.validity.reasons.join("; ")
            ),
        );
    }
    // Re-planning from the workflow.
    let replay = match prep
        .engine
        .replay_workflow(&hit.workflow.id, &cwd, &BTreeMap::new())
    {
        Ok(r) => r,
        Err(e) => {
            first_failure(&mut a, Stage::History, format!("replay failed: {e}"));
            return a;
        }
    };
    let step = replay.steps.into_iter().next();
    let blocked = step
        .as_ref()
        .is_none_or(|s| s.result.status != Status::Planned);
    if h.expect_replan_blocked {
        // A stale workflow whose capability is gone must not yield a plan.
        a.stale_handling = Some(validity == "stale" && blocked);
        a.plan_status = step
            .as_ref()
            .map(|s| lower(&s.result.status))
            .unwrap_or_else(|| "none".into());
        if !blocked {
            first_failure(
                &mut a,
                Stage::History,
                "a plan was built from a workflow whose capability is gone",
            );
        }
        a.latency_ms = started.elapsed().as_millis() as u64;
        return a;
    }
    let Some(step) = step else {
        first_failure(&mut a, Stage::History, "the workflow had nothing to replay");
        return a;
    };
    a.request_ids.push(step.result.request_id.clone());
    // Evaluate the fresh plan with the same rules as any other plan.
    let cwd_prep = Prepared {
        sb: Sandbox {
            root: prep.sb.root.clone(),
            home: prep.sb.home.clone(),
            proj: cwd.clone(),
            path_var: prep.sb.path_var.clone(),
        },
        engine: prep.engine,
        inventory: prep.inventory.clone(),
        caps: prep.caps.clone(),
        discovery_ok: prep.discovery_ok,
    };
    let mut prep = cwd_prep;
    if let Ok(caps) = prep.engine.inventory(&cwd) {
        prep.inventory = caps.iter().map(|c| c.id.clone()).collect();
        prep.caps = caps;
    }
    let plan_ok = check_plan(&mut a, &prep, case, &step.result);
    if h.expect_validity == "stale" {
        a.stale_handling = Some(
            validity == "stale"
                && plan_ok
                && a.failure.as_ref().is_none_or(|f| f.stage == Stage::History),
        );
    }
    if !plan_ok {
        a.latency_ms = started.elapsed().as_millis() as u64;
        return a;
    }
    let Some(plan) = &step.result.plan else {
        return a;
    };
    // Approval now, not then: the stored plan must not run without it.
    let probe = prep.engine.exec_stored(
        &plan.plan_id,
        &RunOptions {
            execute: true,
            yes: false,
            ..Default::default()
        },
        &mut NonInteractive,
    );
    let mut enforced = true;
    if plan.approval.level != ApprovalLevel::None {
        match probe {
            Ok(r) => {
                if started_steps(&r).next().is_some() {
                    a.unauthorized_executions += 1;
                    enforced = false;
                    first_failure(
                        &mut a,
                        Stage::Authority,
                        "a replayed plan executed without current approval",
                    );
                }
            }
            Err(e) => first_failure(
                &mut a,
                Stage::Authority,
                format!("authority probe errored: {e}"),
            ),
        }
    }
    let changed = changed_paths(&before, &snapshot(&cwd));
    if !changed.is_empty() {
        a.unintended_side_effects
            .extend(changed.iter().map(|p| format!("(replay/probe) {p}")));
        enforced = false;
        first_failure(
            &mut a,
            Stage::Authority,
            format!("replay or the unapproved attempt changed the project: {changed:?}"),
        );
    }
    a.approval = Some(enforced && a.approval != Some(false));
    if a.failure.is_none() && case.execute {
        let yes = plan.approval.level != ApprovalLevel::Explicit;
        match prep.engine.exec_stored(
            &plan.plan_id,
            &RunOptions {
                execute: true,
                yes,
                ..Default::default()
            },
            &mut NonInteractive,
        ) {
            Ok(r) => check_execution(&mut a, &prep, case, &r, &before),
            Err(e) => first_failure(&mut a, Stage::Execution, format!("execution errored: {e}")),
        }
    }
    a.latency_ms = started.elapsed().as_millis() as u64;
    a
}

fn summarize(attempts: &[&Attempt]) -> ApproachSummary {
    let n = attempts.len();
    let yes = |f: &dyn Fn(&Attempt) -> bool| Rate {
        passed: attempts.iter().filter(|a| f(a)).count(),
        of: n,
    };
    ApproachSummary {
        pairs: n,
        selection_correct: yes(&|a| a.selection == Some(true)),
        plan_valid: yes(&|a| a.plan_valid == Some(true)),
        task_verified: yes(&|a| a.verification == Some(true)),
        invalid_plans: attempts
            .iter()
            .filter(|a| a.plan_valid == Some(false))
            .count() as u32,
        unauthorized_executions: attempts.iter().map(|a| a.unauthorized_executions).sum(),
        incorrect_verification: attempts
            .iter()
            .filter(|a| {
                a.verification_status.as_deref() == Some("passed")
                    && a.process_exit_zero == Some(false)
            })
            .count() as u32,
        unintended_side_effects: attempts
            .iter()
            .map(|a| a.unintended_side_effects.len() as u32)
            .sum(),
    }
}

pub fn run_suite(
    repo: &Path,
    suite_path: &Path,
    opts: &SuiteOptions,
) -> Result<EvalReport, EngineError> {
    let text = std::fs::read_to_string(suite_path)?;
    let mut cases: Vec<Case> = Vec::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        cases.push(serde_json::from_str(line).map_err(|e| {
            EngineError::Invalid(format!("{} line {}: {e}", suite_path.display(), i + 1))
        })?);
    }
    let mut seen = BTreeSet::new();
    for c in &cases {
        if !seen.insert(c.id.clone()) {
            return Err(EngineError::Invalid(format!("duplicate case id {}", c.id)));
        }
    }
    let ephemeral = opts.out_dir.is_none();
    // Unique per run, so concurrent runs (even within one process) never share a sandbox.
    static RUN_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let base = opts.out_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "cli2-eval-{}-{}",
            std::process::id(),
            RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ))
    });
    let base = base.join("cases");
    std::fs::create_dir_all(&base)?;

    let model_ready = opts.model && model_status(&ResolverSettings::default()).ready;
    let model_status_text = if !opts.model {
        "not run (pass --model)".to_string()
    } else if model_ready {
        "ready (real on-device inference, one run per pair; not deterministic)".to_string()
    } else {
        format!(
            "not run: {}",
            model_status(&ResolverSettings::default()).state
        )
    };

    let mut results = Vec::new();
    for case in cases
        .iter()
        .filter(|c| opts.only.as_ref().is_none_or(|o| o == &c.id))
    {
        let mut cr = CaseResult {
            id: case.id.clone(),
            category: case.category.clone(),
            request: case.request.clone(),
            skipped: None,
            passed: false,
            known_gap: case.known_gap.clone(),
            first_failure: None,
            attempt: None,
            comparisons: vec![],
            evidence_dir: (!ephemeral).then(|| base.join(&case.id).display().to_string()),
        };
        // Main attempt.
        let main = make_sandbox(repo, case, &base, "main").and_then(|sb| {
            let label = sb.root.clone();
            match prepare(sb, case, Provider::None) {
                Ok(p) => Ok((p, label)),
                Err(f) => Err(format!("\u{0}{}\u{0}{:?}", f.detail, f.stage)),
            }
        });
        let attempt = match main {
            Err(s) if s.starts_with('\u{0}') => {
                let mut parts = s.split('\u{0}').skip(1);
                let detail = parts.next().unwrap_or("").to_string();
                let stage = parts.next().unwrap_or("Discovery");
                let mut a = Attempt::new(Approach::FreshDeterministic, &case.request);
                a.failure = Some(Failure {
                    stage: if stage == "Interface" {
                        Stage::Interface
                    } else {
                        Stage::Discovery
                    },
                    detail,
                });
                a.discovery = Some(false);
                a
            }
            Err(reason) => {
                cr.skipped = Some(reason);
                results.push(cr);
                continue;
            }
            Ok((prep, label)) => match &case.history {
                Some(h) => history_attempt(
                    prep,
                    HistoryRun {
                        case,
                        query: &case.request,
                        prior: &case.request,
                        mutations: &h.mutate,
                        query_in_copy: h.query_in.is_some(),
                        expect_validity: &h.expect_validity,
                        expect_replan_blocked: h.expect_replan.as_deref() == Some("blocked"),
                        expect_retrieval: h.expect_retrieval,
                    },
                    &label,
                ),
                None => fresh_attempt(prep, case, &case.request, Approach::FreshDeterministic),
            },
        };
        // Comparison pairs: fresh vs history vs model over paraphrases.
        for (i, query) in case.variants.iter().enumerate() {
            let mk = |label: String, provider| {
                make_sandbox(repo, case, &base, &label)
                    .ok()
                    .and_then(|sb| prepare(sb, case, provider).ok())
            };
            let a = mk(format!("cmp{i}-fresh"), Provider::None)
                .map(|p| fresh_attempt(p, case, query, Approach::FreshDeterministic));
            let b = mk(format!("cmp{i}-history"), Provider::None).map(|p| {
                history_attempt(
                    p,
                    HistoryRun {
                        case,
                        query,
                        prior: &case.request,
                        mutations: &[],
                        query_in_copy: false,
                        expect_validity: "validated",
                        expect_replan_blocked: false,
                        expect_retrieval: true,
                    },
                    &base,
                )
            });
            let c = (model_ready && case.execute)
                .then(|| mk(format!("cmp{i}-model"), Provider::AppleFoundationModels))
                .flatten()
                .map(|p| fresh_attempt(p, case, query, Approach::Model));
            if let (Some(a), Some(b)) = (a, b) {
                cr.comparisons.push(Comparison {
                    query: query.clone(),
                    fresh_deterministic: a,
                    history_replan: b,
                    model: c,
                });
            }
        }
        cr.first_failure = attempt.failure.clone();
        cr.passed = attempt.ok();
        cr.attempt = Some(attempt);
        results.push(cr);
    }

    // Aggregate.
    let ran: Vec<&CaseResult> = results.iter().filter(|c| c.skipped.is_none()).collect();
    let attempts: Vec<&Attempt> = ran.iter().filter_map(|c| c.attempt.as_ref()).collect();
    let planned: Vec<&&Attempt> = attempts
        .iter()
        .filter(|a| a.plan_status == "planned")
        .collect();
    let metrics = Metrics {
        capability_discovery: Rate::from(attempts.iter().map(|a| a.discovery)),
        correct_capability_selection: Rate::from(attempts.iter().map(|a| a.selection)),
        plan_validity: Rate::from(attempts.iter().map(|a| a.plan_valid)),
        argument_correctness: Rate::from(attempts.iter().map(|a| a.argument)),
        approval_behavior: Rate::from(attempts.iter().map(|a| a.approval)),
        execution_completion: Rate::from(attempts.iter().map(|a| a.execution_completed)),
        process_success: Rate::from(attempts.iter().map(|a| a.process_exit_zero)),
        process_outcome_as_expected: Rate::from(attempts.iter().map(|a| a.process_as_expected)),
        task_verification: Rate::from(attempts.iter().map(|a| a.verification)),
        workflow_retrieval: Rate::from(attempts.iter().map(|a| a.retrieval)),
        stale_workflow_handling: Rate::from(attempts.iter().map(|a| a.stale_handling)),
        invalid_capability_references: Count {
            count: attempts.iter().map(|a| a.invalid_capability_refs).sum(),
            of: planned.len(),
        },
        invalid_argument_references: Count {
            count: attempts.iter().map(|a| a.invalid_argument_refs).sum(),
            of: planned.len(),
        },
        unintended_side_effects: Count {
            count: attempts
                .iter()
                .map(|a| a.unintended_side_effects.len() as u32)
                .sum(),
            of: attempts.len(),
        },
        unauthorized_executions: Count {
            count: attempts.iter().map(|a| a.unauthorized_executions).sum(),
            of: attempts.len(),
        },
    };
    let pairs: Vec<&Comparison> = ran.iter().flat_map(|c| c.comparisons.iter()).collect();
    let a_list: Vec<&Attempt> = pairs.iter().map(|p| &p.fresh_deterministic).collect();
    let b_list: Vec<&Attempt> = pairs.iter().map(|p| &p.history_replan).collect();
    let c_list: Vec<&Attempt> = pairs.iter().filter_map(|p| p.model.as_ref()).collect();
    let comparison = ComparisonSummary {
        fresh_deterministic: summarize(&a_list),
        history_replan: summarize(&b_list),
        model: (!c_list.is_empty()).then(|| summarize(&c_list)),
        model_status: model_status_text,
    };
    let mut failure_categories: BTreeMap<String, usize> = BTreeMap::new();
    for c in &ran {
        if let Some(f) = &c.first_failure {
            *failure_categories.entry(lower(&f.stage)).or_default() += 1;
        }
    }
    let matching = match &opts.matching {
        Some(p) => Some(matching::run_matching(p)?),
        None => None,
    };
    let fingerprint = {
        let mut h = Sha256::new();
        for c in &results {
            h.update(c.id.as_bytes());
            h.update(format!("{:?}|{}", c.skipped, c.passed).as_bytes());
            for a in c.attempt.iter().chain(
                c.comparisons
                    .iter()
                    .flat_map(|p| [&p.fresh_deterministic, &p.history_replan]),
            ) {
                h.update(
                    format!(
                        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{:?}",
                        a.approach, a.failure.as_ref().map(|f| f.stage), a.discovery, a.selection, a.plan_valid, a.argument, a.approval,
                        a.execution_completed, a.process_exit_zero, a.process_as_expected, a.verification, a.retrieval, a.stale_handling,
                        a.plan_status, a.selected.clone().unwrap_or_default(), a.invalid_capability_refs + a.invalid_argument_refs + a.unauthorized_executions,
                        (&a.argv, &a.validity, &a.exit_code, &a.verification_status)
                    )
                    .as_bytes(),
                );
            }
        }
        if let Some(m) = &matching {
            h.update(m.fingerprint.as_bytes());
        }
        hex::encode(h.finalize())
    };
    let mut host = BTreeMap::new();
    host.insert("os".into(), std::env::consts::OS.into());
    host.insert("arch".into(), std::env::consts::ARCH.into());
    for t in ["git", "make", "rg", "node", "npm"] {
        host.insert(
            format!("tool:{t}"),
            host_which(t)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "absent".into()),
        );
    }
    let report = EvalReport {
        schema_version: REPORT_SCHEMA,
        suite: suite_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        cli2_version: env!("CARGO_PKG_VERSION"),
        generated_at: cli2_core::now_rfc3339(),
        host,
        cases_total: results.len(),
        cases_run: ran.len(),
        cases_skipped: results.len() - ran.len(),
        cases_passed: ran.iter().filter(|c| c.passed).count(),
        cases_failed: ran.iter().filter(|c| !c.passed).count(),
        metrics,
        comparison,
        failure_categories,
        cases: results,
        matching,
        results_fingerprint: fingerprint,
    };
    if ephemeral {
        let _ = std::fs::remove_dir_all(base.parent().unwrap_or(&base));
    }
    Ok(report)
}

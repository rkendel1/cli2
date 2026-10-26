//! The embeddable intent runtime: a typed Rust API over the engine.
//!
//! Every operation opens the engine (discovery index, evidence journal,
//! policy) exactly as a CLI invocation does and calls the same code; nothing
//! here re-implements resolution, planning, policy, execution, verification or
//! persistence. What this layer adds is the *application* surface:
//!
//! * explicit project context on every call;
//! * plans that are only proposals (`plan` never executes);
//! * approval decisions bound to a plan ID and its exact digest (`approve`),
//!   which authorize nothing by themselves;
//! * executions (`start`) that run on the runtime's own threads, independent
//!   of any client, whose outcome is recorded durably by the engine, and that
//!   can be observed with a cursor, cancelled, and recovered after a client
//!   disconnects;
//! * idempotence: an approval authorizes at most one execution, and a repeated
//!   planning request with the same idempotency key returns the same plan.
//!
//! The local protocol (`cli2 serve`) is a thin transport over this API.

mod executions;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cli2_core::{ApprovalDecision, ApprovalLevel, ExecutionResult, RequestInput, Status};
use cli2_protocol::*;
use serde_json::{json, Value};

use executions::{EventSink, Registry};
pub use executions::{Execution, MAX_BUFFERED_OUTPUT};

use crate::engine::{
    Config, Engine, EngineError, EngineObserver, FileEditInput, NonInteractive, RunOptions,
};
use crate::output::{started_steps, OutputId, Part};
use crate::semantic::ResolverMode;
use crate::workflow::engine_ext::SearchOptions;

/// A stable, machine-readable error.
#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<Value>,
}

impl RuntimeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }
    pub fn body(&self) -> ErrorBody {
        let mut b = ErrorBody::new(self.code, self.message.clone());
        b.details = self.details.clone();
        b
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<EngineError> for RuntimeError {
    fn from(e: EngineError) -> Self {
        let message = e.to_string();
        let code = match &e {
            EngineError::NotFound(_)
            | EngineError::Journal(cli2_evidence::JournalError::NotFound(_)) => ErrorCode::NotFound,
            EngineError::Invalid(_) => ErrorCode::InvalidParams,
            EngineError::Refused { code, .. } => match *code {
                "plan_modified" => ErrorCode::PlanModified,
                "approval_mismatch" => ErrorCode::ApprovalMismatch,
                "approval_required" => ErrorCode::ApprovalRequired,
                "forbidden" => ErrorCode::Forbidden,
                "stale_plan" => ErrorCode::StalePlan,
                "plan_rejected" => ErrorCode::PlanRejected,
                "already_executed" => ErrorCode::AlreadyExecuted,
                "stale_patch" => ErrorCode::StalePatch,
                _ => ErrorCode::Conflict,
            },
            _ => ErrorCode::Internal,
        };
        Self {
            code,
            message,
            details: None,
        }
    }
}

impl From<cli2_evidence::JournalError> for RuntimeError {
    fn from(e: cli2_evidence::JournalError) -> Self {
        RuntimeError::from(EngineError::Journal(e))
    }
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

pub struct Runtime {
    config: Config,
    instance_id: String,
    registry: Arc<Registry>,
    /// Serializes "check for an existing execution, then create one".
    start_lock: Mutex<()>,
    /// Serializes idempotent planning.
    plan_lock: Mutex<()>,
}

fn invalid(m: impl Into<String>) -> RuntimeError {
    RuntimeError::new(ErrorCode::InvalidParams, m)
}

fn lifecycle_of(r: &ExecutionResult) -> Lifecycle {
    let _ = r;
    Lifecycle::Finished
}

fn process_of(r: &ExecutionResult) -> Option<ProcessSummary> {
    started_steps(r).last().map(|s| ProcessSummary {
        termination: s.termination,
        exit_code: s.exit_code,
        signal: s.signal,
        duration_ms: s.duration_ms,
    })
}

impl Runtime {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            instance_id: cli2_core::new_id("rt"),
            registry: Arc::new(Registry::default()),
            start_lock: Mutex::new(()),
            plan_lock: Mutex::new(()),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn engine(&self) -> Result<Engine> {
        Ok(Engine::open(self.config.clone())?)
    }

    /// A project context: an absolute path to an existing directory.
    pub fn project_dir(&self, project: &str) -> Result<PathBuf> {
        let p = Path::new(project);
        if !p.is_absolute() {
            return Err(RuntimeError::new(
                ErrorCode::ProjectInvalid,
                format!("project {project:?} must be an absolute path"),
            ));
        }
        let c = p.canonicalize().map_err(|e| {
            RuntimeError::new(
                ErrorCode::ProjectInvalid,
                format!("project {project:?}: {e}"),
            )
        })?;
        if !c.is_dir() {
            return Err(RuntimeError::new(
                ErrorCode::ProjectInvalid,
                format!("project {project:?} is not a directory"),
            ));
        }
        Ok(c)
    }

    /// The evidence journal, for components built on the runtime (the task
    /// controller) that record their own hash-chained events.
    pub(crate) fn journal(&self) -> Result<cli2_evidence::Journal> {
        Ok(cli2_evidence::Journal::open(&self.config.evidence_dir())?)
    }

    // ------------------------------------------------------------ discovery

    /// Discover (or re-discover) capabilities for a project. Explicit: the
    /// runtime never scans a project it was not asked about.
    pub fn scan(&self, project: &str) -> Result<ScanSummary> {
        let dir = self.project_dir(project)?;
        let mut e = self.engine()?;
        let r = e.scan(&dir)?;
        let p = r.project.as_ref();
        Ok(ScanSummary {
            project: dir.display().to_string(),
            project_root: r.project_root.clone(),
            added: r.environment.added.len() + p.map_or(0, |x| x.added.len()),
            changed: r.environment.changed.len() + p.map_or(0, |x| x.changed.len()),
            removed: r.environment.removed.len() + p.map_or(0, |x| x.removed.len()),
            warnings: r.warnings.clone(),
        })
    }

    pub fn capabilities(&self, project: &str) -> Result<CapabilitiesResult> {
        self.capabilities_of_kinds(project, &[])
    }

    /// The capabilities visible from `project`, only of `kinds` when any are given.
    pub fn capabilities_of_kinds(
        &self,
        project: &str,
        kinds: &[cli2_core::CapabilityKind],
    ) -> Result<CapabilitiesResult> {
        let dir = self.project_dir(project)?;
        let mut caps = self.engine()?.inventory(&dir)?;
        if !kinds.is_empty() {
            caps.retain(|c| kinds.contains(&c.kind));
        }
        Ok(CapabilitiesResult {
            project: dir.display().to_string(),
            capabilities: caps,
        })
    }

    // ------------------------------------------------------------- planning

    /// Resolve and plan an intent. **Never executes.**
    pub fn plan(&self, p: PlanParams) -> Result<PlanResult> {
        let dir = self.project_dir(&p.project)?;
        let _guard = p
            .idempotency_key
            .as_ref()
            .map(|_| self.plan_lock.lock().unwrap());
        let mut e = self.engine()?;
        if let Some(key) = &p.idempotency_key {
            for (_, request_id, data) in e.journal.events_of_kind("plan_requested")? {
                if data["idempotency_key"].as_str() == Some(key) {
                    if data["project"].as_str() != Some(&dir.display().to_string())
                        || data["intent"].as_str() != Some(p.intent.trim())
                    {
                        return Err(RuntimeError::new(
                            ErrorCode::Conflict,
                            format!(
                                "idempotency key {key:?} was already used for a different request"
                            ),
                        ));
                    }
                    let seq = e.journal.latest_result_seq(&request_id)?.ok_or_else(|| {
                        RuntimeError::new(
                            ErrorCode::Internal,
                            "the original plan result is missing",
                        )
                    })?;
                    let outcome = e.journal.result_row(seq)?;
                    // The result row omits the embedded plan; restore it from the plan record.
                    let outcome = with_plan(&e, outcome);
                    return Ok(plan_result(outcome, true));
                }
            }
        }
        let mut req = RequestInput::new(p.intent.clone());
        req.constraints = p.constraints.clone();
        req.bindings = p.bindings.clone();
        req.selection = p.selection.clone();
        req.expectations = p.expectations.clone();
        req.context.working_directory = Some(dir.display().to_string());
        if let Some(mode) = p.resolver.as_deref() {
            let mode = ResolverMode::parse(mode)
                .ok_or_else(|| invalid("resolver must be deterministic, apple-fm or auto"))?;
            e.set_resolver_mode(mode);
        }
        if let Some(t) = p.timeout_secs {
            if !(1..=3600).contains(&t) {
                return Err(invalid("timeout_secs must be between 1 and 3600"));
            }
        }
        let outcome = e.run(
            req,
            &RunOptions {
                execute: false,
                capture_help: p.capture_help,
                timeout_secs: p.timeout_secs,
                ..Default::default()
            },
            &mut NonInteractive,
        )?;
        if let Some(key) = &p.idempotency_key {
            e.journal.event(
                &outcome.request_id,
                "plan_requested",
                &json!({"idempotency_key": key, "project": dir.display().to_string(), "intent": p.intent.trim()}),
            )?;
        }
        Ok(plan_result(outcome, false))
    }

    /// Read a project file with the same containment rules as an edit.
    pub fn read_file(&self, p: &FileReadParams) -> Result<FileReadResult> {
        use cli2_execution::fsedit::{read_file, ReadError};
        let dir = self.project_dir(&p.project)?;
        let path = cli2_core::normalize_rel_path(&p.path).map_err(|m| invalid(&m))?;
        let snap = read_file(&dir, &path).map_err(|e| match e {
            ReadError::Missing(m) => {
                RuntimeError::new(ErrorCode::NotFound, format!("{m} does not exist"))
            }
            ReadError::Refused(m) => RuntimeError::new(ErrorCode::InvalidParams, m),
        })?;
        let content = String::from_utf8(snap.bytes).map_err(|_| {
            invalid("the file is not valid UTF-8; only text files can be read here")
        })?;
        Ok(FileReadResult {
            path,
            sha256: snap.sha256,
            bytes: content.len() as u64,
            lines: content.split_inclusive('\n').count() as u64,
            content,
        })
    }

    /// Plan a patch to one file. **Never writes.** The plan carries the exact
    /// hunks, the version they apply to and the resulting hash; approving its
    /// digest approves exactly that change.
    pub fn plan_edit(&self, p: PlanEditParams) -> Result<PlanResult> {
        let dir = self.project_dir(&p.project)?;
        let intent = p
            .intent
            .clone()
            .filter(|i| !i.trim().is_empty())
            .unwrap_or_else(|| format!("Edit {}", p.path));
        let patch_id = cli2_core::content_hash(
            &serde_json::to_vec(
                &json!({"path": p.path, "expected": p.expected_sha256, "hunks": p.hunks}),
            )
            .expect("patch serializes"),
        );
        let _guard = p
            .idempotency_key
            .as_ref()
            .map(|_| self.plan_lock.lock().unwrap());
        let mut e = self.engine()?;
        if let Some(key) = &p.idempotency_key {
            for (_, request_id, data) in e.journal.events_of_kind("plan_requested")? {
                if data["idempotency_key"].as_str() == Some(key) {
                    if data["project"].as_str() != Some(&dir.display().to_string())
                        || data["patch"].as_str() != Some(&patch_id)
                    {
                        return Err(RuntimeError::new(
                            ErrorCode::Conflict,
                            format!(
                                "idempotency key {key:?} was already used for a different request"
                            ),
                        ));
                    }
                    let seq = e.journal.latest_result_seq(&request_id)?.ok_or_else(|| {
                        RuntimeError::new(
                            ErrorCode::Internal,
                            "the original plan result is missing",
                        )
                    })?;
                    let outcome = with_plan(&e, e.journal.result_row(seq)?);
                    return Ok(plan_result(outcome, true));
                }
            }
        }
        let outcome = e.plan_file_edit(
            FileEditInput {
                project_root: dir.display().to_string(),
                path: p.path.clone(),
                expected_sha256: p.expected_sha256.clone(),
                hunks: p.hunks.clone(),
                intent,
                constraints: p.constraints.clone(),
            },
            &RunOptions {
                execute: false,
                ..Default::default()
            },
            &mut NonInteractive,
        )?;
        if let Some(key) = &p.idempotency_key {
            e.journal.event(
                &outcome.request_id,
                "plan_requested",
                &json!({"idempotency_key": key, "project": dir.display().to_string(), "patch": patch_id}),
            )?;
        }
        Ok(plan_result(outcome, false))
    }

    pub fn plan_get(&self, plan_id: &str) -> Result<PlanView> {
        let mut e = self.engine()?;
        let plan = e.journal.plan(plan_id)?;
        let rejected = e
            .journal
            .latest_event_data(&plan.request_id, "plan_rejected")?
            .is_some();
        let approvals = e.journal.approvals_for_plan(plan_id)?;
        let executions = e
            .journal
            .events_of_kind("execution_started")?
            .into_iter()
            .filter(|(_, _, d)| d["plan_id"].as_str() == Some(plan_id))
            .map(|(_, _, d)| ExecutionRef {
                execution_id: d["execution_id"].as_str().unwrap_or("").into(),
                approval_id: d["approval_id"].as_str().map(String::from),
            })
            .collect();
        let stale_reasons = e.stale_reasons(&plan);
        Ok(PlanView {
            approvability: approvability(Some(&plan), None),
            plan,
            rejected,
            approvals,
            executions,
            stale_reasons,
        })
    }

    // -------------------------------------------------- approval and rejection

    /// Record an approval for exactly this plan. Authorizes nothing by itself.
    pub fn approve(&self, plan_id: &str, plan_digest: &str) -> Result<ApproveResult> {
        let rec = self.engine()?.approve_plan(plan_id, plan_digest)?;
        Ok(ApproveResult { approval: rec })
    }

    pub fn reject(&self, plan_id: &str, reason: Option<String>) -> Result<RejectResult> {
        Ok(RejectResult {
            outcome: self.engine()?.reject_plan(plan_id, reason)?,
        })
    }

    // ------------------------------------------------------------ execution

    /// Start executing an approved plan. Returns at once; the execution runs
    /// on the runtime's threads. The engine still performs its full
    /// authorization check (plan intact, approval granted and bound to this
    /// plan and digest, not stale) immediately before running anything.
    ///
    /// An approval authorizes at most one execution: a retry with the same
    /// approval returns the same execution (`deduplicated`).
    pub fn start(&self, plan_id: &str, approval_id: &str) -> Result<StartResult> {
        let e = self.engine()?;
        let plan = e.journal.plan(plan_id)?;
        let approval = e.journal.approval(approval_id)?;
        if approval.plan_id != plan.plan_id || approval.plan_digest != plan.digest {
            return Err(RuntimeError::new(
                ErrorCode::ApprovalMismatch,
                "the approval does not belong to this plan and digest",
            ));
        }
        if approval.decision != ApprovalDecision::Granted {
            return Err(RuntimeError::new(
                ErrorCode::ApprovalRequired,
                "the approval is not a grant",
            ));
        }
        if !plan.is_intact() {
            return Err(RuntimeError::new(
                ErrorCode::PlanModified,
                "the stored plan no longer matches its digest",
            ));
        }
        if plan.approval.level == ApprovalLevel::Forbidden {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "policy forbids this operation",
            ));
        }
        if e.journal
            .latest_event_data(&plan.request_id, "plan_rejected")?
            .is_some()
        {
            return Err(RuntimeError::new(
                ErrorCode::PlanRejected,
                "this plan was rejected",
            ));
        }

        let _guard = self.start_lock.lock().unwrap();
        // Retried start with the same approval: the same execution, not a second one.
        if let Some(existing) = self
            .registry
            .by_approval
            .lock()
            .unwrap()
            .get(approval_id)
            .cloned()
        {
            let live = self.registry.by_id.lock().unwrap().get(&existing).cloned();
            if let Some(x) = live {
                return Ok(self.start_result(&x, true));
            }
        }
        for (_, _, d) in e.journal.events_of_kind("execution_started")? {
            if d["approval_id"].as_str() == Some(approval_id) {
                let id = d["execution_id"].as_str().unwrap_or("").to_string();
                let snap = self.execution(&id)?;
                return Ok(StartResult {
                    execution_id: id,
                    plan_id: snap.plan_id,
                    request_id: snap.request_id,
                    lifecycle: snap.lifecycle,
                    deduplicated: true,
                });
            }
        }

        let execution_id = cli2_core::new_id("run");
        let exec = Execution::new(
            execution_id.clone(),
            plan.plan_id.clone(),
            plan.request_id.clone(),
            approval_id.to_string(),
        );
        // Durable before anything runs: a crash leaves evidence that it was started.
        e.journal.event(
            &plan.request_id,
            "execution_started",
            &json!({
                "execution_id": execution_id,
                "plan_id": plan.plan_id,
                "request_id": plan.request_id,
                "approval_id": approval_id,
                "instance_id": self.instance_id,
                "started_at": exec.started_at,
            }),
        )?;
        self.registry
            .by_id
            .lock()
            .unwrap()
            .insert(execution_id.clone(), exec.clone());
        self.registry
            .by_approval
            .lock()
            .unwrap()
            .insert(approval_id.to_string(), execution_id.clone());
        drop(e);

        let config = self.config.clone();
        let plan_id = plan.plan_id.clone();
        let thread_exec = exec.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("cli2-{execution_id}"))
            .spawn(move || {
                run_execution(config, plan_id, approval, thread_exec);
            });
        if let Err(err) = spawned {
            exec.fail(format!("could not start the execution thread: {err}"));
            return Err(RuntimeError::new(
                ErrorCode::Internal,
                format!("could not start the execution thread: {err}"),
            ));
        }
        Ok(self.start_result(&exec, false))
    }

    fn start_result(&self, x: &Arc<Execution>, deduplicated: bool) -> StartResult {
        StartResult {
            execution_id: x.id.clone(),
            plan_id: x.plan_id.clone(),
            request_id: x.request_id.clone(),
            lifecycle: x.state.lock().unwrap().lifecycle,
            deduplicated,
        }
    }

    fn live(&self, id: &str) -> Option<Arc<Execution>> {
        self.registry.by_id.lock().unwrap().get(id).cloned()
    }

    /// Current state of an execution: live if this runtime holds it, otherwise
    /// reconstructed from durable evidence.
    pub fn execution(&self, id: &str) -> Result<ExecutionSnapshot> {
        if let Some(x) = self.live(id) {
            let st = x.state.lock().unwrap();
            return Ok(ExecutionSnapshot {
                execution_id: x.id.clone(),
                plan_id: x.plan_id.clone(),
                request_id: x.request_id.clone(),
                approval_id: Some(x.approval_id.clone()),
                lifecycle: st.lifecycle,
                source: "live".into(),
                started_at: x.started_at.clone(),
                last_seq: st.next_seq - 1,
                steps: st.steps.clone(),
                process: st.steps.last().map(|s| ProcessSummary {
                    termination: s.termination,
                    exit_code: s.exit_code,
                    signal: s.signal,
                    duration_ms: s.duration_ms,
                }),
                task_status: st.result.as_ref().map(|r| r.status),
                verification: st.result.as_ref().map(|r| r.verification.status),
                result: st.result.clone(),
                error: st.error.clone(),
            });
        }
        let mut e = self.engine()?;
        self.journal_execution(&mut e, id)
    }

    /// Rebuild an execution's state from the journal.
    fn journal_execution(&self, e: &mut Engine, id: &str) -> Result<ExecutionSnapshot> {
        let started = e
            .journal
            .events_of_kind("execution_started")?
            .into_iter()
            .find(|(_, _, d)| d["execution_id"].as_str() == Some(id))
            .map(|(_, _, d)| d)
            .ok_or_else(|| RuntimeError::new(ErrorCode::NotFound, format!("no execution {id}")))?;
        let plan_id = started["plan_id"].as_str().unwrap_or("").to_string();
        let request_id = started["request_id"].as_str().unwrap_or("").to_string();
        let started_at = started["started_at"].as_str().unwrap_or("").to_string();
        let mut seq: Option<i64> = None;
        for kind in ["execution_finished", "execution_interrupted"] {
            for (_, _, d) in e.journal.events_of_kind(kind)? {
                if d["execution_id"].as_str() == Some(id) {
                    seq = d["result_seq"].as_i64();
                }
            }
        }
        // A crash after the result was recorded but before the marker event.
        if seq.is_none() {
            seq = e
                .journal
                .result_rows(100_000)?
                .into_iter()
                .filter(|(_, r)| {
                    r.plan_id.as_deref() == Some(&plan_id)
                        && started_steps(r).any(|s| s.started_at >= started_at)
                })
                .map(|(s, _)| s)
                .max();
        }
        let result = match seq {
            Some(s) => Some(with_plan(e, e.journal.result_row(s)?)),
            None => None,
        };
        let interrupted = result
            .as_ref()
            .is_some_and(|r| started_steps(r).next().is_none() && r.status == Status::Cancelled)
            && e.journal
                .events_of_kind("execution_interrupted")?
                .iter()
                .any(|(_, _, d)| d["execution_id"].as_str() == Some(id));
        let lifecycle = match (&result, interrupted) {
            (_, true) | (None, _) => Lifecycle::InterruptedUnknown,
            (Some(r), false) => lifecycle_of(r),
        };
        Ok(ExecutionSnapshot {
            execution_id: id.to_string(),
            plan_id,
            request_id,
            approval_id: started["approval_id"].as_str().map(String::from),
            lifecycle,
            source: "journal".into(),
            started_at,
            last_seq: 0,
            steps: result.as_ref().map(|r| r.steps.clone()).unwrap_or_default(),
            process: result.as_ref().and_then(process_of),
            task_status: result.as_ref().map(|r| r.status),
            verification: result.as_ref().map(|r| r.verification.status),
            result,
            error: None,
        })
    }

    /// Execution events after a cursor, waiting up to `wait` for new ones.
    pub fn events(&self, p: &EventsParams) -> Result<EventsResult> {
        let wait = Duration::from_millis(p.wait_ms.min(MAX_EVENT_WAIT_MS));
        let limit = p.limit.unwrap_or(500).clamp(1, 5000);
        if let Some(x) = self.live(&p.execution_id) {
            let (events, last_seq, finished, gap) = x.events_after(p.after_seq, wait, limit);
            return Ok(EventsResult {
                events,
                last_seq,
                finished,
                gap_before_seq: gap,
            });
        }
        // Not live (a previous runtime ran it): synthesize the durable facts.
        let snap = self.execution(&p.execution_id)?;
        let mut events = Vec::new();
        let mut seq = 0u64;
        let mut push = |data: EventData| {
            seq += 1;
            events.push(Event {
                schema_version: EVENT_SCHEMA.into(),
                execution_id: snap.execution_id.clone(),
                seq,
                at: snap.started_at.clone(),
                data,
            });
        };
        push(EventData::ExecutionStarted {
            plan_id: snap.plan_id.clone(),
            request_id: snap.request_id.clone(),
            approval_id: snap.approval_id.clone(),
        });
        for s in &snap.steps {
            push(EventData::StepFinished {
                result: Box::new(s.clone()),
            });
        }
        if let Some(r) = &snap.result {
            push(EventData::Verification {
                verification: r.verification.clone(),
            });
            push(EventData::ExecutionFinished {
                result: Box::new(r.clone()),
            });
        }
        let last_seq = seq;
        let events: Vec<Event> = events
            .into_iter()
            .filter(|e| e.seq > p.after_seq)
            .take(limit)
            .collect();
        Ok(EventsResult {
            events,
            last_seq,
            finished: true,
            gap_before_seq: None,
        })
    }

    /// Cancel a running execution through the engine's cancellation path.
    pub fn cancel(&self, id: &str) -> Result<CancelResult> {
        match self.live(id) {
            Some(x) => {
                let accepted = x.request_cancel();
                let lifecycle = x.state.lock().unwrap().lifecycle;
                Ok(CancelResult {
                    execution_id: id.to_string(),
                    accepted,
                    lifecycle,
                })
            }
            None => {
                let snap = self.execution(id)?;
                Ok(CancelResult {
                    execution_id: id.to_string(),
                    accepted: false,
                    lifecycle: snap.lifecycle,
                })
            }
        }
    }

    /// Executions this runtime did not finish (it, or its predecessor, ended
    /// first) are recorded as interrupted with an unknown outcome: never as
    /// successes. Returns the execution IDs it reconciled.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let mut e = self.engine()?;
        let started = e.journal.events_of_kind("execution_started")?;
        let mut done: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for kind in ["execution_finished", "execution_interrupted"] {
            for (_, _, d) in e.journal.events_of_kind(kind)? {
                if let Some(id) = d["execution_id"].as_str() {
                    done.insert(id.to_string());
                }
            }
        }
        let mut reconciled = Vec::new();
        for (_, request_id, d) in started {
            let id = d["execution_id"].as_str().unwrap_or("").to_string();
            if id.is_empty()
                || done.contains(&id)
                || d["instance_id"].as_str() == Some(&self.instance_id)
            {
                continue;
            }
            let plan_id = d["plan_id"].as_str().unwrap_or("").to_string();
            let started_at = d["started_at"].as_str().unwrap_or("").to_string();
            let existing = e
                .journal
                .result_rows(100_000)?
                .into_iter()
                .filter(|(_, r)| {
                    r.plan_id.as_deref() == Some(&plan_id)
                        && started_steps(r).any(|s| s.started_at >= started_at)
                })
                .map(|(s, _)| s)
                .max();
            match existing {
                Some(seq) => e.journal.event(
                    &request_id,
                    "execution_finished",
                    &json!({"execution_id": id, "result_seq": seq, "reconciled": true}),
                )?,
                None => {
                    let mut message = "Interrupted: the runtime that was executing this plan ended before recording an outcome. Whether the command completed is unknown; no success is claimed.".to_string();
                    if let Ok(plan) = e.journal.plan(&plan_id) {
                        // Read-only: say what a crash left behind. It is evidence
                        // for a person; nothing is retried on the strength of it.
                        for note in crate::engine::file_edit_state_notes(&plan) {
                            message.push(' ');
                            message.push_str(&note);
                        }
                    }
                    let r = e.record_interrupted(&plan_id, message)?;
                    let seq = e.journal.latest_result_seq(&r.request_id)?;
                    e.journal.event(
                        &request_id,
                        "execution_interrupted",
                        &json!({"execution_id": id, "result_seq": seq}),
                    )?;
                }
            }
            reconciled.push(id);
        }
        Ok(reconciled)
    }

    /// Stop cleanly: cancel live executions and give them `grace` to record
    /// their outcome. Anything still running afterwards is reconciled as
    /// interrupted by the next runtime.
    pub fn shutdown(&self, grace: Duration) {
        let live: Vec<Arc<Execution>> = self
            .registry
            .by_id
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for x in &live {
            x.request_cancel();
        }
        let deadline = std::time::Instant::now() + grace;
        for x in live {
            loop {
                let l = x.state.lock().unwrap().lifecycle;
                if matches!(l, Lifecycle::Finished | Lifecycle::InterruptedUnknown)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    // ---------------------------------------------------- evidence & history

    pub fn tasks(&self, limit: Option<usize>) -> Result<DocumentResult> {
        let tasks = self.engine()?.tasks(limit.unwrap_or(20).min(500))?;
        Ok(DocumentResult {
            schema: "cli2.task_list.v1".into(),
            document: json!({"schema_version": "cli2.task_list.v1", "tasks": tasks}),
        })
    }

    pub fn task(&self, id: &str) -> Result<DocumentResult> {
        let t = self.engine()?.task(id)?;
        Ok(DocumentResult {
            schema: "cli2.task.v1".into(),
            document: to_value(&t)?,
        })
    }

    pub fn evidence(&self, reference: &str, max_bytes: Option<usize>) -> Result<ContentResult> {
        if !reference.starts_with("evidence:") {
            return Err(invalid("reference must be an evidence: reference"));
        }
        let bytes = self.engine()?.read_evidence(reference)?;
        Ok(content(bytes, max_bytes))
    }

    pub fn output(&self, id: &str, part: &str, max_bytes: Option<usize>) -> Result<ContentResult> {
        let part = Part::parse(part).ok_or_else(|| {
            invalid("part must be stdout, stderr, response or command-and-response")
        })?;
        if id != "latest" && OutputId::parse(id).is_none() {
            return Err(invalid(
                "output_id must be out_N, out_N.S, exec_N or latest",
            ));
        }
        let bytes = self.engine()?.output_part(id, part)?;
        Ok(content(bytes, max_bytes))
    }

    pub fn history_search(&self, p: &HistorySearchParams) -> Result<DocumentResult> {
        let dir = self.project_dir(&p.project)?;
        let r = self.engine()?.search_history(
            &p.text,
            &dir,
            &SearchOptions {
                limit: p.limit.unwrap_or(10).min(100),
                verified_only: p.verified_only,
                include_batch_steps: false,
                freshness_budget: None,
            },
            None,
        )?;
        Ok(DocumentResult {
            schema: "cli2.history_search.v1".into(),
            document: to_value(&r)?,
        })
    }

    pub fn workflow(&self, p: &WorkflowParams) -> Result<DocumentResult> {
        let dir = self.project_dir(&p.project)?;
        let mut e = self.engine()?;
        let wf = e.workflow_get(&p.workflow_id)?;
        let v = e.validate_workflows(&[&wf], &dir)?.remove(0);
        Ok(DocumentResult {
            schema: "cli2.workflow_view.v1".into(),
            document: json!({"schema_version": "cli2.workflow_view.v1", "workflow": wf, "validity": v}),
        })
    }

    /// A fresh plan for a workflow, under current discovery, policy and approval.
    /// Never executes; the plans it returns are approved and started like any other.
    pub fn workflow_replan(&self, p: &WorkflowParams) -> Result<DocumentResult> {
        let dir = self.project_dir(&p.project)?;
        let r = self
            .engine()?
            .replay_workflow(&p.workflow_id, &dir, &p.bindings)?;
        Ok(DocumentResult {
            schema: "cli2.workflow_replay.v1".into(),
            document: to_value(&r)?,
        })
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| RuntimeError::new(ErrorCode::Internal, e.to_string()))
}

fn content(bytes: Vec<u8>, max: Option<usize>) -> ContentResult {
    let max = max.unwrap_or(256 * 1024).min(512 * 1024);
    let truncated = bytes.len() > max;
    let slice = &bytes[..bytes.len().min(max)];
    match std::str::from_utf8(slice) {
        Ok(t) => ContentResult {
            encoding: "utf-8".into(),
            content: t.to_string(),
            bytes: slice.len(),
            truncated,
        },
        Err(_) => {
            use std::fmt::Write as _;
            // Base64 without a dependency on a crate the protocol would then need.
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in slice.chunks(3) {
                let n = (c[0] as u32) << 16
                    | (*c.get(1).unwrap_or(&0) as u32) << 8
                    | *c.get(2).unwrap_or(&0) as u32;
                let _ = write!(
                    out,
                    "{}{}",
                    T[(n >> 18) as usize & 63] as char,
                    T[(n >> 12) as usize & 63] as char
                );
                out.push(if c.len() > 1 {
                    T[(n >> 6) as usize & 63] as char
                } else {
                    '='
                });
                out.push(if c.len() > 2 {
                    T[n as usize & 63] as char
                } else {
                    '='
                });
            }
            ContentResult {
                encoding: "base64".into(),
                content: out,
                bytes: slice.len(),
                truncated,
            }
        }
    }
}

fn approvability(plan: Option<&cli2_core::Plan>, status: Option<Status>) -> Approvability {
    match (plan, status) {
        (_, Some(Status::Rejected)) => Approvability::Forbidden,
        (None, _) => Approvability::NoPlan,
        (Some(p), _) => match p.approval.level {
            ApprovalLevel::Forbidden => Approvability::Forbidden,
            ApprovalLevel::None => Approvability::NotRequired,
            _ => Approvability::RequiresApproval,
        },
    }
}

fn plan_result(outcome: ExecutionResult, deduplicated: bool) -> PlanResult {
    let approvability = approvability(outcome.plan.as_ref(), Some(outcome.status));
    let required_approval = outcome.plan.as_ref().map(|p| p.approval.level);
    PlanResult {
        outcome,
        approvability,
        required_approval,
        deduplicated,
    }
}

/// Journaled results omit the embedded plan (it is stored separately); put it back.
fn with_plan(e: &Engine, mut r: ExecutionResult) -> ExecutionResult {
    if r.plan.is_none() {
        if let Some(pid) = &r.plan_id {
            r.plan = e.journal.plan(pid).ok();
        }
    }
    r
}

/// The body of an execution thread. Whatever happens, the durable outcome is
/// recorded by the engine; this only mirrors it for observers.
fn run_execution(
    config: Config,
    plan_id: String,
    approval: cli2_core::ApprovalRecord,
    exec: Arc<Execution>,
) {
    let outcome = (|| -> std::result::Result<ExecutionResult, EngineError> {
        let mut engine = Engine::open(config)?;
        exec.set_cancel_token(engine.cancel_token());
        exec.push(EventData::ExecutionStarted {
            plan_id: plan_id.clone(),
            request_id: exec.request_id.clone(),
            approval_id: Some(exec.approval_id.clone()),
        });
        let opts = RunOptions {
            execute: true,
            preapproved: Some(approval),
            observer: Some(EngineObserver(Arc::new(EventSink(exec.clone())))),
            ..Default::default()
        };
        let r = engine.exec_stored(&plan_id, &opts, &mut NonInteractive)?;
        if let Some(seq) = engine.result_seq(&r.request_id)? {
            engine.journal.event(
                &r.request_id,
                "execution_finished",
                &json!({"execution_id": exec.id, "result_seq": seq, "status": r.status}),
            )?;
        }
        Ok(r)
    })();
    match outcome {
        Ok(r) => {
            let mut st = exec.state.lock().unwrap();
            if st.result.is_none() {
                // Refused before anything ran (stale plan, authorization): same durable result.
                st.result = Some(r.clone());
                st.lifecycle = Lifecycle::Finished;
                let id = exec.id.clone();
                Execution::push_locked_pub(
                    &mut st,
                    &id,
                    EventData::ExecutionFinished {
                        result: Box::new(r),
                    },
                );
            }
            drop(st);
            exec.changed.notify_all();
        }
        Err(e) => exec.fail(e.to_string()),
    }
}

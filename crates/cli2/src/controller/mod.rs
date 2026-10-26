//! The development task controller.
//!
//! A persistent, bounded, observable loop above [`Runtime`] that carries one
//! goal in one project to a *verified* outcome:
//!
//! ```text
//! investigate → propose → plan → (approval) → execute → observe → verify → continue | stop
//! ```
//!
//! It is a state machine over recorded events, not an agent:
//!
//! * **Persistence.** A task is a sequence of hash-chained journal events under
//!   its ID; its state is a fold over them ([`state`]). There is no second
//!   database. A restarted controller reads the same events and resumes.
//! * **Authority.** Every operation is a [`Runtime`] call: scan, plan, approve,
//!   start, observe, cancel. The controller approves a plan itself only when
//!   policy requires no approval at all; otherwise a *client* must approve
//!   that exact plan by digest. A proposer (script or model) can approve
//!   nothing, name no command, and weaken no check.
//! * **Completion contract.** The task completes only when every acceptance
//!   criterion has been satisfied by an execution whose task-level
//!   verification passed and is not older than the latest change. Exit status
//!   alone never completes anything.
//! * **Recovery without repetition.** Each phase is idempotent. An execution
//!   that was started is never started again (the runtime dedupes by
//!   approval); one interrupted with an unknown outcome blocks the task for a
//!   person instead of being re-run.
//! * **Bounded.** Step and failed-attempt limits end a task with a precise
//!   report.

pub mod proposer;
pub mod state;

use std::collections::{BTreeSet, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cli2_core::{ApprovalLevel, Capability, CapabilityKind, Check, InterfaceCompleteness};
use cli2_protocol::devtask::*;
use cli2_protocol::{Approvability, ErrorCode, EventsParams, Lifecycle, PlanParams};

use crate::runtime::{Runtime, RuntimeError};
use proposer::*;
use state::{all_passed, evaluate, fold, report, Model, StepRec};

pub type Result<T> = std::result::Result<T, RuntimeError>;

const MAX_CAPABILITY_BRIEFS: usize = 150;
const OUTPUT_TAIL: usize = 4096;

fn err(code: ErrorCode, m: impl Into<String>) -> RuntimeError {
    RuntimeError::new(code, m)
}

/// Serializes decisions about one task and wakes anything waiting on it.
struct Cell {
    op: Mutex<()>,
    version: Mutex<u64>,
    changed: Condvar,
    worker_running: AtomicBool,
}

pub struct Controller {
    runtime: Arc<Runtime>,
    strategies: Mutex<HashMap<String, Arc<dyn StrategyFactory>>>,
    cells: Mutex<HashMap<String, Arc<Cell>>>,
    stopping: AtomicBool,
}

enum Tick {
    Progress,
    Idle,
    Done,
}

impl Controller {
    pub fn new(runtime: Arc<Runtime>) -> Arc<Self> {
        let c = Arc::new(Self {
            runtime,
            strategies: Mutex::new(HashMap::new()),
            cells: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
        });
        c.register_strategy(
            "outline",
            Arc::new(FnStrategy(|_: &CreateTaskParams| {
                Ok(Box::new(OutlineProposer) as Box<dyn StepProposer>)
            })),
        );
        if let Some(client) = model_from_config(&c.runtime.config().home) {
            c.register_model(Arc::new(client));
        }
        c
    }

    /// Register a named strategy (tests and applications add their own).
    pub fn register_strategy(&self, name: &str, f: Arc<dyn StrategyFactory>) {
        self.strategies.lock().unwrap().insert(name.to_string(), f);
    }

    /// Enable the `model` strategy with a model client.
    pub fn register_model(&self, client: Arc<dyn ModelClient>) {
        struct Shared(Arc<dyn ModelClient>);
        impl ModelClient for Shared {
            fn complete(&self, r: &ModelRequest) -> std::result::Result<String, String> {
                self.0.complete(r)
            }
        }
        self.register_strategy(
            "model",
            Arc::new(FnStrategy(move |_: &CreateTaskParams| {
                Ok(Box::new(ModelProposer {
                    client: Shared(client.clone()),
                }) as Box<dyn StepProposer>)
            })),
        );
    }

    fn cell(&self, task_id: &str) -> Arc<Cell> {
        self.cells
            .lock()
            .unwrap()
            .entry(task_id.to_string())
            .or_insert_with(|| {
                Arc::new(Cell {
                    op: Mutex::new(()),
                    version: Mutex::new(0),
                    changed: Condvar::new(),
                    worker_running: AtomicBool::new(false),
                })
            })
            .clone()
    }

    fn wake(&self, cell: &Cell) {
        *cell.version.lock().unwrap() += 1;
        cell.changed.notify_all();
    }

    // ------------------------------------------------------------------ events

    fn read(
        &self,
        j: &cli2_evidence::Journal,
        task_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<DevTaskEvent>> {
        let mut out = Vec::new();
        for (seq, at, kind, data) in j.events_for(task_id, after, limit)? {
            if kind != "devtask" {
                continue;
            }
            let data: DevTaskEventData = serde_json::from_value(data).map_err(|e| {
                err(
                    ErrorCode::Internal,
                    format!("unreadable task event {seq}: {e}"),
                )
            })?;
            out.push(DevTaskEvent {
                schema_version: DEVTASK_EVENT_SCHEMA.into(),
                task_id: task_id.into(),
                seq,
                at,
                data,
            });
        }
        Ok(out)
    }

    fn load(&self, j: &cli2_evidence::Journal, task_id: &str) -> Result<Model> {
        if !task_id.starts_with("dev_") {
            return Err(err(ErrorCode::NotFound, format!("no task {task_id}")));
        }
        let events = self.read(j, task_id, 0, 1_000_000)?;
        fold(&events).ok_or_else(|| err(ErrorCode::NotFound, format!("no task {task_id}")))
    }

    fn append(
        &self,
        j: &cli2_evidence::Journal,
        task_id: &str,
        data: DevTaskEventData,
    ) -> Result<()> {
        j.event(task_id, "devtask", &data)?;
        self.wake(&self.cell(task_id));
        Ok(())
    }

    /// Move to a new state through the state machine; anything else is a bug.
    fn transition(
        &self,
        j: &cli2_evidence::Journal,
        m: &Model,
        to: TaskState,
        reason: &str,
    ) -> Result<()> {
        if m.state == to {
            return Ok(());
        }
        if !m.state.can_become(to) {
            return Err(err(
                ErrorCode::Internal,
                format!("illegal task transition {:?} -> {:?}", m.state, to),
            ));
        }
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::StateChanged {
                from: m.state,
                to,
                reason: reason.into(),
            },
        )
    }

    // -------------------------------------------------------------- operations

    pub fn create(self: &Arc<Self>, p: CreateTaskParams) -> Result<TaskView> {
        let dir = self.runtime.project_dir(&p.project)?;
        let invalid = |m: &str| err(ErrorCode::InvalidParams, m.to_string());
        if p.goal.trim().is_empty() || p.goal.len() > 4000 {
            return Err(invalid("the goal must be 1–4000 characters"));
        }
        if p.criteria.is_empty() {
            return Err(invalid("a task needs at least one acceptance criterion: without one, completion could never be verified"));
        }
        if p.criteria.len() > 20 || p.outline.len() > 50 {
            return Err(invalid(
                "too many criteria (max 20) or outline steps (max 50)",
            ));
        }
        let mut seen = BTreeSet::new();
        for c in &p.criteria {
            let ok = !c.id.is_empty()
                && c.id.len() <= 40
                && c.id.chars().all(|ch| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-'
                });
            if !ok || !seen.insert(c.id.clone()) {
                return Err(invalid(
                    "criterion ids must be unique and use only [a-z0-9_-]",
                ));
            }
            if c.description.trim().is_empty() {
                return Err(invalid("every criterion needs a description"));
            }
            if !c.checks.iter().any(Check::is_task_level) {
                return Err(invalid(&format!(
                    "criterion {:?} has no task-level check (stdout_contains, file_exists or path_exists); an exit status alone cannot establish it",
                    c.id
                )));
            }
        }
        if p.limits.max_steps == 0
            || p.limits.max_steps > 50
            || p.limits.max_attempts == 0
            || p.limits.max_attempts > 10
        {
            return Err(invalid(
                "limits: max_steps must be 1–50 and max_attempts 1–10",
            ));
        }
        let strategy = p.strategy.clone().unwrap_or_else(|| "outline".into());
        let factory = {
            let map = self.strategies.lock().unwrap();
            match map.get(&strategy).cloned() {
                Some(f) => f,
                None => {
                    let mut names: Vec<&String> = map.keys().collect();
                    names.sort();
                    let names: Vec<&str> = names.into_iter().map(String::as_str).collect();
                    return Err(err(
                        ErrorCode::InvalidParams,
                        format!(
                            "unknown strategy {strategy:?}; available: {}",
                            names.join(", ")
                        ),
                    ));
                }
            }
        };
        factory
            .create(&p)
            .map_err(|e| err(ErrorCode::InvalidParams, e))?;
        for s in &p.outline {
            for id in &s.satisfies {
                if !seen.contains(id) {
                    return Err(invalid(&format!(
                        "outline step refers to unknown criterion {id:?}"
                    )));
                }
            }
        }

        let j = self.runtime.journal()?;
        if let Some(key) = &p.idempotency_key {
            for (_, tid, data) in j.events_of_kind("devtask")? {
                if data["type"] == "created" && data["idempotency_key"].as_str() == Some(key) {
                    let m = self.load(&j, &tid)?;
                    if m.goal != p.goal || m.project != dir.display().to_string() {
                        return Err(err(
                            ErrorCode::Conflict,
                            format!(
                                "idempotency key {key:?} was already used for a different task"
                            ),
                        ));
                    }
                    return self.view(&m);
                }
            }
        }
        let task_id = cli2_core::new_id("dev");
        self.append(
            &j,
            &task_id,
            DevTaskEventData::Created {
                goal: p.goal.clone(),
                project: dir.display().to_string(),
                strategy,
                criteria: p.criteria.clone(),
                outline: p.outline.clone(),
                limits: p.limits.clone(),
                idempotency_key: p.idempotency_key.clone(),
            },
        )?;
        self.view(&self.load(&j, &task_id)?)
    }

    pub fn get(&self, task_id: &str) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        self.view(&self.load(&j, task_id)?)
    }

    pub fn list(&self, limit: Option<usize>) -> Result<TaskListResult> {
        let j = self.runtime.journal()?;
        let mut ids: Vec<String> = j
            .events_of_kind("devtask")?
            .into_iter()
            .filter(|(_, _, d)| d["type"] == "created")
            .map(|(_, t, _)| t)
            .collect();
        ids.reverse();
        ids.truncate(limit.unwrap_or(20).min(200));
        let mut tasks = Vec::new();
        for id in ids {
            let m = self.load(&j, &id)?;
            tasks.push(TaskBrief {
                task_id: m.task_id,
                goal: m.goal,
                project: m.project,
                state: m.state,
                paused: m.paused,
                updated_at: m.updated_at,
            });
        }
        Ok(TaskListResult { tasks })
    }

    /// Begin (or, after a restart, continue) driving a task.
    pub fn start(self: &Arc<Self>, task_id: &str) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        let cell = self.cell(task_id);
        {
            let _g = cell.op.lock().unwrap();
            let m = self.load(&j, task_id)?;
            if m.state == TaskState::Created {
                self.transition(&j, &m, TaskState::Investigating, "started")?;
            }
        }
        let m = self.load(&j, task_id)?;
        if !m.state.is_terminal() {
            self.ensure_worker(task_id);
        }
        self.view(&m)
    }

    /// Approve the specific operation the task proposed, by its exact digest.
    pub fn approve(self: &Arc<Self>, p: ApproveStepParams) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        let cell = self.cell(&p.task_id);
        let _g = cell.op.lock().unwrap();
        let m = self.load(&j, &p.task_id)?;
        let step = self.awaiting_step(&m, &p.step_id)?;
        if step.view.plan_digest.as_deref() != Some(p.plan_digest.as_str()) {
            return Err(err(ErrorCode::ApprovalMismatch, "the digest is not that of the plan awaiting approval; approvals apply only to the exact plan"));
        }
        let plan_id = step
            .view
            .plan_id
            .clone()
            .expect("awaiting approval implies a plan");
        let a = self.runtime.approve(&plan_id, &p.plan_digest)?.approval;
        self.append(
            &j,
            &p.task_id,
            DevTaskEventData::ApprovalGranted {
                step_id: p.step_id.clone(),
                plan_id,
                approval_id: a.approval_id,
                by: "client".into(),
            },
        )?;
        let m = self.load(&j, &p.task_id)?;
        self.transition(
            &j,
            &m,
            TaskState::Planned,
            "approved by the client; starting the approved plan",
        )?;
        drop(_g);
        self.ensure_worker(&p.task_id);
        self.view(&self.load(&j, &p.task_id)?)
    }

    pub fn reject(self: &Arc<Self>, p: RejectStepParams) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        let cell = self.cell(&p.task_id);
        let _g = cell.op.lock().unwrap();
        let m = self.load(&j, &p.task_id)?;
        let step = self.awaiting_step(&m, &p.step_id)?;
        let plan_id = step
            .view
            .plan_id
            .clone()
            .expect("awaiting approval implies a plan");
        let reason = p
            .reason
            .clone()
            .unwrap_or_else(|| "rejected by the operator".into());
        self.runtime.reject(&plan_id, Some(reason.clone()))?;
        self.append(
            &j,
            &p.task_id,
            DevTaskEventData::StepRejected {
                step_id: p.step_id.clone(),
                plan_id,
                reason,
            },
        )?;
        let m = self.load(&j, &p.task_id)?;
        self.transition(
            &j,
            &m,
            TaskState::Investigating,
            "the proposed operation was rejected; choosing another step",
        )?;
        drop(_g);
        self.ensure_worker(&p.task_id);
        self.view(&self.load(&j, &p.task_id)?)
    }

    fn awaiting_step<'a>(&self, m: &'a Model, step_id: &str) -> Result<&'a StepRec> {
        if m.state != TaskState::AwaitingApproval {
            return Err(err(
                ErrorCode::Conflict,
                format!("the task is {:?}, not awaiting approval", m.state),
            ));
        }
        let s = m
            .step(step_id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("no step {step_id}")))?;
        if s.view.status != StepStatus::AwaitingApproval
            || m.current().map(|c| c.view.step_id.as_str()) != Some(step_id)
        {
            return Err(err(
                ErrorCode::Conflict,
                format!("step {step_id} is not the operation awaiting approval"),
            ));
        }
        Ok(s)
    }

    pub fn pause(&self, task_id: &str) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        let __c = self.cell(task_id);
        let _g = __c.op.lock().unwrap();
        let m = self.load(&j, task_id)?;
        if m.state.is_terminal() {
            return Err(err(ErrorCode::Conflict, "the task has finished"));
        }
        if !m.paused {
            self.append(&j, task_id, DevTaskEventData::Paused)?;
        }
        self.view(&self.load(&j, task_id)?)
    }

    /// Resume a paused task, or a blocked one (which then chooses a new step;
    /// an interrupted execution is never re-run).
    pub fn resume(self: &Arc<Self>, task_id: &str) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        {
            let __c = self.cell(task_id);
            let _g = __c.op.lock().unwrap();
            let m = self.load(&j, task_id)?;
            if m.state.is_terminal() {
                return Err(err(ErrorCode::Conflict, "the task has finished"));
            }
            if m.paused {
                self.append(&j, task_id, DevTaskEventData::Resumed)?;
            }
            let m = self.load(&j, task_id)?;
            if m.state == TaskState::Blocked {
                self.transition(&j, &m, TaskState::Investigating, "resumed by the operator")?;
            }
        }
        self.ensure_worker(task_id);
        self.view(&self.load(&j, task_id)?)
    }

    pub fn cancel(self: &Arc<Self>, task_id: &str, reason: Option<String>) -> Result<TaskView> {
        let j = self.runtime.journal()?;
        {
            let __c = self.cell(task_id);
            let _g = __c.op.lock().unwrap();
            let m = self.load(&j, task_id)?;
            if m.state.is_terminal() {
                return Err(err(ErrorCode::Conflict, "the task has already finished"));
            }
            if m.cancel_requested.is_none() {
                self.append(
                    &j,
                    task_id,
                    DevTaskEventData::CancelRequested {
                        reason: reason.unwrap_or_else(|| "cancelled by the operator".into()),
                    },
                )?;
            }
        }
        // A worker finishes the cancellation (stopping a running execution first).
        self.ensure_worker(task_id);
        self.view(&self.load(&j, task_id)?)
    }

    pub fn events(&self, p: &TaskEventsParams) -> Result<TaskEventsResult> {
        let j = self.runtime.journal()?;
        let limit = p.limit.unwrap_or(500).clamp(1, 5000);
        let deadline =
            Instant::now() + Duration::from_millis(p.wait_ms.min(cli2_protocol::MAX_EVENT_WAIT_MS));
        let cell = self.cell(&p.task_id);
        loop {
            let version = *cell.version.lock().unwrap();
            let events = self.read(&j, &p.task_id, p.after_seq, limit)?;
            if events.is_empty() && self.load(&j, &p.task_id).is_err() {
                return Err(err(ErrorCode::NotFound, format!("no task {}", p.task_id)));
            }
            let last_seq = events.last().map(|e| e.seq).unwrap_or(p.after_seq);
            let terminal = self.load(&j, &p.task_id)?.state.is_terminal();
            let delivered_all = j.events_for(&p.task_id, last_seq, 1)?.is_empty();
            if !events.is_empty() || (terminal && delivered_all) || Instant::now() >= deadline {
                return Ok(TaskEventsResult {
                    events,
                    last_seq,
                    finished: terminal && delivered_all,
                });
            }
            let guard = cell.version.lock().unwrap();
            if *guard == version {
                let _ = cell
                    .changed
                    .wait_timeout(
                        guard,
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(250)),
                    )
                    .unwrap();
            }
        }
    }

    pub fn report(&self, task_id: &str) -> Result<TaskReport> {
        let j = self.runtime.journal()?;
        let m = self.load(&j, task_id)?;
        Ok(m.final_report.clone().unwrap_or_else(|| report(&m, None)))
    }

    /// Continue every task that was running when the previous runtime ended.
    /// Call after `Runtime::reconcile`, so interrupted executions are already
    /// recorded as such and are never run again.
    pub fn recover(self: &Arc<Self>) -> Result<Vec<String>> {
        let j = self.runtime.journal()?;
        let mut resumed = Vec::new();
        for (_, id, d) in j.events_of_kind("devtask")? {
            if d["type"] != "created" {
                continue;
            }
            let m = self.load(&j, &id)?;
            if !matches!(m.state, TaskState::Created) && !m.state.is_terminal() {
                self.ensure_worker(&id);
                resumed.push(id);
            }
        }
        Ok(resumed)
    }

    // -------------------------------------------------------------------- view

    fn view(&self, m: &Model) -> Result<TaskView> {
        let current = m.current();
        let current_plan = current
            .and_then(|s| s.view.plan_id.as_ref())
            .and_then(|pid| self.runtime.plan_get(pid).ok())
            .map(|v| v.plan);
        let mut unresolved: Vec<String> = m
            .statuses
            .iter()
            .filter(|s| s.outcome != CriterionOutcome::Passed)
            .map(|s| format!("{}: {}", s.id, s.reason))
            .collect();
        if let Some(b) = &m.blocked_reason {
            unresolved.push(format!("blocked: {b}"));
        }
        let rep = match m.state {
            TaskState::Completed | TaskState::Failed => {
                m.final_report.clone().or_else(|| Some(report(m, None)))
            }
            TaskState::Cancelled => Some(report(m, None)),
            _ => None,
        };
        Ok(TaskView {
            schema_version: DEVTASK_VIEW_SCHEMA.into(),
            task_id: m.task_id.clone(),
            goal: m.goal.clone(),
            project: m.project.clone(),
            state: m.state,
            paused: m.paused,
            strategy: m.strategy.clone(),
            created_at: m.created_at.clone(),
            updated_at: m.updated_at.clone(),
            limits: m.limits.clone(),
            steps_used: m.steps.len() as u32,
            attempts: m.attempts,
            criteria: m.statuses.clone(),
            steps: m.steps.iter().map(|s| s.view.clone()).collect(),
            current_step: current.map(|s| s.view.step_id.clone()),
            current_plan,
            investigation: m.investigation.clone(),
            unresolved,
            last_seq: m.last_seq,
            report: rep,
        })
    }

    // ------------------------------------------------------------------ worker

    /// Stop driving tasks: workers finish what they are doing and exit. Task
    /// state lives in the journal, so a later controller simply continues.
    /// Executions already started keep running under the runtime.
    pub fn shutdown(&self, grace: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        let cells: Vec<Arc<Cell>> = self.cells.lock().unwrap().values().cloned().collect();
        for c in &cells {
            self.wake(c);
        }
        let deadline = Instant::now() + grace;
        for c in cells {
            while c.worker_running.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn ensure_worker(self: &Arc<Self>, task_id: &str) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let cell = self.cell(task_id);
        if cell.worker_running.swap(true, Ordering::SeqCst) {
            self.wake(&cell);
            return;
        }
        let me = self.clone();
        let id = task_id.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("cli2-{id}"))
            .spawn(move || me.worker(id));
        if spawned.is_err() {
            cell.worker_running.store(false, Ordering::SeqCst);
        }
    }

    fn worker(self: Arc<Self>, task_id: String) {
        let cell = self.cell(&task_id);
        let mut proposer: Option<Box<dyn StepProposer>> = None;
        let mut errors = 0;
        while !self.stopping.load(Ordering::SeqCst) {
            let tick =
                std::panic::catch_unwind(AssertUnwindSafe(|| self.tick(&task_id, &mut proposer)));
            match tick {
                Ok(Ok(Tick::Progress)) => errors = 0,
                Ok(Ok(Tick::Idle)) => {
                    errors = 0;
                    let v = *cell.version.lock().unwrap();
                    let g = cell.version.lock().unwrap();
                    if *g == v {
                        let _ = cell
                            .changed
                            .wait_timeout(g, Duration::from_millis(250))
                            .unwrap();
                    }
                }
                Ok(Ok(Tick::Done)) => break,
                Ok(Err(e)) => {
                    errors += 1;
                    if errors >= 5 {
                        self.block_quietly(
                            &task_id,
                            &format!("the controller could not continue: {e}"),
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(_) => {
                    self.block_quietly(&task_id, "the controller hit an internal error and stopped; the task's recorded history is intact");
                    break;
                }
            }
        }
        cell.worker_running.store(false, Ordering::SeqCst);
    }

    fn block_quietly(&self, task_id: &str, reason: &str) {
        if let Ok(j) = self.runtime.journal() {
            if let Ok(m) = self.load(&j, task_id) {
                if !m.state.is_terminal() && m.state != TaskState::Blocked {
                    let _ = self.block(&j, &m, reason);
                }
            }
        }
    }

    fn block(&self, j: &cli2_evidence::Journal, m: &Model, reason: &str) -> Result<()> {
        self.transition(j, m, TaskState::Blocked, reason)?;
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::Blocked {
                reason: reason.into(),
            },
        )
    }

    fn finish_failed(&self, j: &cli2_evidence::Journal, m: &Model, reason: &str) -> Result<()> {
        self.transition(j, m, TaskState::Failed, reason)?;
        let mut folded = self.load(j, &m.task_id)?;
        folded.blocked_reason = None;
        let rep = report(&folded, None);
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::Failed {
                reason: reason.into(),
                report: Box::new(rep),
            },
        )
    }

    /// One bounded unit of progress. Re-derives everything from recorded events.
    fn tick(
        self: &Arc<Self>,
        task_id: &str,
        proposer: &mut Option<Box<dyn StepProposer>>,
    ) -> Result<Tick> {
        let j = self.runtime.journal()?;
        let m = self.load(&j, task_id)?;
        if m.state.is_terminal() {
            return Ok(Tick::Done);
        }
        if let Some(reason) = m.cancel_requested.clone() {
            return self.do_cancel(&j, &m, &reason);
        }
        if matches!(m.state, TaskState::Created | TaskState::Blocked) {
            return Ok(Tick::Idle);
        }
        if m.investigation.is_none() {
            return if m.paused {
                Ok(Tick::Idle)
            } else {
                self.investigate(&j, &m)
            };
        }
        if let Some(step) = m.current() {
            return match step.view.status {
                StepStatus::Proposed if m.paused => Ok(Tick::Idle),
                StepStatus::Proposed => self.make_plan(&j, &m, step),
                StepStatus::AwaitingApproval => {
                    if m.state != TaskState::AwaitingApproval {
                        let __c = self.cell(task_id);
                        let _g = __c.op.lock().unwrap();
                        let m2 = self.load(&j, task_id)?;
                        self.transition(
                            &j,
                            &m2,
                            TaskState::AwaitingApproval,
                            "a person must approve this exact plan",
                        )?;
                    }
                    Ok(Tick::Idle)
                }
                StepStatus::Approved if m.paused => Ok(Tick::Idle),
                StepStatus::Approved => self.start_execution(&j, &m, step),
                StepStatus::Executing => self.observe(&j, &m, step),
                _ => Ok(Tick::Idle),
            };
        }
        // No open step. A finished step must be judged before anything new is proposed.
        if let Some(last) = m.steps.last() {
            if last.observed_seq.is_some_and(|s| s > m.last_evaluated_seq) {
                return self.verify(&j, &m);
            }
        }
        if m.paused {
            return Ok(Tick::Idle);
        }
        self.propose(&j, &m, proposer)
    }

    fn capabilities(&self, project: &str) -> Result<Vec<Capability>> {
        Ok(self.runtime.capabilities(project)?.capabilities)
    }

    fn investigate(&self, j: &cli2_evidence::Journal, m: &Model) -> Result<Tick> {
        // Discovery is explicit and visible: the same operations as `cli2 scan` and `cli2 inventory`.
        let scan = match self.runtime.scan(&m.project) {
            Ok(s) => s,
            Err(e) => {
                self.block(j, m, &format!("could not discover the project: {e}"))?;
                return Ok(Tick::Idle);
            }
        };
        let caps = self.capabilities(&m.project)?;
        let briefs = briefs(&caps);
        let total = briefs.len();
        let investigation = Investigation {
            project_root: scan.project_root,
            capabilities: briefs.into_iter().take(MAX_CAPABILITY_BRIEFS).collect(),
            total_capabilities: total,
        };
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::Investigated { investigation },
        )?;
        Ok(Tick::Progress)
    }

    fn propose(
        &self,
        j: &cli2_evidence::Journal,
        m: &Model,
        proposer: &mut Option<Box<dyn StepProposer>>,
    ) -> Result<Tick> {
        if m.steps.len() as u32 >= m.limits.max_steps {
            self.finish_failed(
                j,
                m,
                &format!(
                    "the step limit ({}) was reached before every criterion was verified",
                    m.limits.max_steps
                ),
            )?;
            return Ok(Tick::Done);
        }
        if proposer.is_none() {
            let factory = self.strategies.lock().unwrap().get(&m.strategy).cloned();
            let Some(factory) = factory else {
                self.block(
                    j,
                    m,
                    &format!("strategy {:?} is not available in this runtime", m.strategy),
                )?;
                return Ok(Tick::Idle);
            };
            let params = CreateTaskParams {
                project: m.project.clone(),
                goal: m.goal.clone(),
                criteria: m.criteria.clone(),
                strategy: Some(m.strategy.clone()),
                outline: m.outline.clone(),
                limits: m.limits.clone(),
                idempotency_key: None,
            };
            match factory.create(&params) {
                Ok(p) => *proposer = Some(p),
                Err(e) => {
                    self.block(j, m, &format!("the strategy could not start: {e}"))?;
                    return Ok(Tick::Idle);
                }
            }
        }
        let proposer = proposer.as_mut().expect("just set");
        let caps = self.capabilities(&m.project)?;
        let discovered: BTreeSet<String> = caps.iter().map(|c| c.id.clone()).collect();
        let briefs: Vec<CapabilityBrief> = briefs(&caps)
            .into_iter()
            .take(MAX_CAPABILITY_BRIEFS)
            .collect();
        let history: Vec<HistoryItem> = m
            .steps
            .iter()
            .map(|s| HistoryItem {
                step_id: s.view.step_id.clone(),
                kind: s.view.kind,
                objective: s.view.objective.clone(),
                intent: s.view.intent.clone(),
                selection: s.view.selection.clone(),
                satisfies: s.view.satisfies.clone(),
                status: s.view.status,
                detail: s.view.detail.clone(),
                outcome: s.view.outcome.clone(),
                stdout_tail: s.stdout_tail.clone(),
                stderr_tail: s.stderr_tail.clone(),
            })
            .collect();
        let ctx = ProposalContext {
            task_id: &m.task_id,
            goal: &m.goal,
            project: &m.project,
            criteria: &m.criteria,
            statuses: &m.statuses,
            outline: &m.outline,
            capabilities: &briefs,
            history,
            limits: &m.limits,
            steps_used: m.steps.len() as u32,
            attempts: m.attempts,
        };
        let source = proposer.name();
        let reject = |reason: String| -> Result<Tick> {
            let __c = self.cell(&m.task_id);
            let _g = __c.op.lock().unwrap();
            let fresh = self.load(j, &m.task_id)?;
            if fresh.state.is_terminal() || fresh.cancel_requested.is_some() {
                return Ok(Tick::Progress);
            }
            self.append(
                j,
                &m.task_id,
                DevTaskEventData::ProposalRejected {
                    reason: reason.clone(),
                    source: source.clone(),
                },
            )?;
            if fresh.consecutive_rejections + 1 >= 3 {
                let fresh = self.load(j, &m.task_id)?;
                self.block(
                    j,
                    &fresh,
                    &format!("the proposer kept producing unusable proposals; the last: {reason}"),
                )?;
            }
            Ok(Tick::Progress)
        };
        match proposer.propose(&ctx) {
            Err(e) => reject(e),
            Ok(Proposal::GiveUp { reason }) => {
                let __c = self.cell(&m.task_id);
                let _g = __c.op.lock().unwrap();
                let fresh = self.load(j, &m.task_id)?;
                if !fresh.state.is_terminal() {
                    self.block(
                        j,
                        &fresh,
                        &format!("the proposer cannot make progress: {reason}"),
                    )?;
                }
                Ok(Tick::Progress)
            }
            Ok(Proposal::Done { note }) => {
                // A suggestion, never a fact: the criteria say whether anything is done.
                let unmet: Vec<String> = m
                    .statuses
                    .iter()
                    .filter(|s| s.outcome != CriterionOutcome::Passed)
                    .map(|s| s.id.clone())
                    .collect();
                if unmet.is_empty() {
                    return Ok(Tick::Idle);
                }
                reject(format!(
                    "the proposer said it is done, but these criteria are not verified: {}{}",
                    unmet.join(", "),
                    note.map(|n| format!(" (its note: {n})"))
                        .unwrap_or_default()
                ))
            }
            Ok(Proposal::Step(step)) => {
                if let Err(reason) = validate(&step, &m.criteria, &discovered) {
                    return reject(reason);
                }
                let __c = self.cell(&m.task_id);
                let _g = __c.op.lock().unwrap();
                let fresh = self.load(j, &m.task_id)?;
                if fresh.state.is_terminal()
                    || fresh.cancel_requested.is_some()
                    || fresh.steps.len() != m.steps.len()
                {
                    return Ok(Tick::Progress);
                }
                let index = fresh.steps.len() as u32 + 1;
                self.append(
                    j,
                    &m.task_id,
                    DevTaskEventData::StepProposed {
                        step_id: format!("s{index}"),
                        index,
                        kind: step.kind,
                        objective: step.objective,
                        intent: step.intent,
                        selection: step.selection,
                        bindings: step.bindings,
                        satisfies: step.satisfies,
                        note: step.note,
                        source,
                    },
                )?;
                Ok(Tick::Progress)
            }
        }
    }

    fn make_plan(&self, j: &cli2_evidence::Journal, m: &Model, step: &StepRec) -> Result<Tick> {
        let mut expectations: Vec<Check> = Vec::new();
        for id in &step.view.satisfies {
            if let Some(c) = m.criteria.iter().find(|c| &c.id == id) {
                for chk in &c.checks {
                    if !expectations.contains(chk) {
                        expectations.push(chk.clone());
                    }
                }
            }
        }
        let planned = self.runtime.plan(PlanParams {
            project: m.project.clone(),
            intent: step.view.intent.clone(),
            idempotency_key: Some(format!("{}:{}", m.task_id, step.view.step_id)),
            constraints: Default::default(),
            bindings: step.bindings.clone(),
            selection: step.view.selection.clone(),
            expectations,
            capture_help: false,
            resolver: None,
            timeout_secs: None,
        });
        let __c = self.cell(&m.task_id);
        let _g = __c.op.lock().unwrap();
        let fresh = self.load(j, &m.task_id)?;
        if fresh.state.is_terminal() || fresh.cancel_requested.is_some() {
            return Ok(Tick::Progress);
        }
        let step_id = step.view.step_id.clone();
        let failed = |status: String, message: String| -> Result<Tick> {
            self.append(
                j,
                &m.task_id,
                DevTaskEventData::PlanFailed {
                    step_id: step_id.clone(),
                    status,
                    message,
                },
            )?;
            if fresh.consecutive_plan_failures + 1 >= 3 {
                let f2 = self.load(j, &m.task_id)?;
                self.block(j, &f2, "three proposed steps in a row could not be planned; a person needs to look at what the project can do")?;
            }
            Ok(Tick::Progress)
        };
        let pr = match planned {
            Ok(p) => p,
            Err(e) => return failed("error".into(), e.to_string()),
        };
        let usable = pr.outcome.status == cli2_core::Status::Planned
            && pr.approvability != Approvability::Forbidden;
        let Some(plan) = pr.outcome.plan.clone().filter(|_| usable) else {
            let msg = if pr.approvability == Approvability::Forbidden {
                "policy forbids this operation; it cannot be approved".to_string()
            } else {
                pr.outcome.message.clone()
            };
            return failed(format!("{:?}", pr.outcome.status).to_lowercase(), msg);
        };
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::PlanMade {
                step_id: step_id.clone(),
                plan_id: plan.plan_id.clone(),
                request_id: pr.outcome.request_id.clone(),
                plan_digest: plan.digest.clone(),
                approval_level: plan.approval.level,
                approvability: pr.approvability,
            },
        )?;
        let fresh = self.load(j, &m.task_id)?;
        self.transition(
            j,
            &fresh,
            TaskState::Planned,
            "a plan was made by the runtime",
        )?;
        if pr.approvability == Approvability::NotRequired
            && plan.approval.level == ApprovalLevel::None
        {
            // Policy requires no approval for this operation: the runtime records that decision.
            let a = self.runtime.approve(&plan.plan_id, &plan.digest)?.approval;
            self.append(
                j,
                &m.task_id,
                DevTaskEventData::ApprovalGranted {
                    step_id,
                    plan_id: plan.plan_id,
                    approval_id: a.approval_id,
                    by: "policy".into(),
                },
            )?;
        } else {
            let fresh = self.load(j, &m.task_id)?;
            self.transition(
                j,
                &fresh,
                TaskState::AwaitingApproval,
                "a person must approve this exact plan",
            )?;
        }
        Ok(Tick::Progress)
    }

    fn start_execution(
        &self,
        j: &cli2_evidence::Journal,
        m: &Model,
        step: &StepRec,
    ) -> Result<Tick> {
        let (Some(plan_id), Some(approval_id)) =
            (step.view.plan_id.clone(), step.view.approval_id.clone())
        else {
            return Ok(Tick::Idle);
        };
        // The runtime dedupes by approval, so starting again after a crash
        // returns the execution that already exists instead of running twice.
        let started = self.runtime.start(&plan_id, &approval_id);
        let __c = self.cell(&m.task_id);
        let _g = __c.op.lock().unwrap();
        let fresh = self.load(j, &m.task_id)?;
        if fresh.state.is_terminal() || fresh.cancel_requested.is_some() {
            return Ok(Tick::Progress);
        }
        match started {
            Ok(run) => {
                self.append(
                    j,
                    &m.task_id,
                    DevTaskEventData::ExecutionStarted {
                        step_id: step.view.step_id.clone(),
                        execution_id: run.execution_id,
                    },
                )?;
                let fresh = self.load(j, &m.task_id)?;
                self.transition(
                    j,
                    &fresh,
                    TaskState::Executing,
                    "the approved plan is executing",
                )?;
            }
            Err(e) => {
                self.append(
                    j,
                    &m.task_id,
                    DevTaskEventData::PlanFailed {
                        step_id: step.view.step_id.clone(),
                        status: format!("{:?}", e.code).to_lowercase(),
                        message: e.message.clone(),
                    },
                )?;
                let fresh = self.load(j, &m.task_id)?;
                self.transition(
                    j,
                    &fresh,
                    TaskState::Investigating,
                    "the approved plan could not be started; choosing another step",
                )?;
            }
        }
        Ok(Tick::Progress)
    }

    fn observe(&self, j: &cli2_evidence::Journal, m: &Model, step: &StepRec) -> Result<Tick> {
        let Some(exec_id) = step.view.execution_id.clone() else {
            return Ok(Tick::Idle);
        };
        let batch = self.runtime.events(&EventsParams {
            execution_id: exec_id.clone(),
            after_seq: u64::MAX,
            wait_ms: 400,
            limit: Some(1),
        })?;
        if !batch.finished {
            return Ok(Tick::Progress);
        }
        let snap = self.runtime.execution(&exec_id)?;
        let known = snap.lifecycle == Lifecycle::Finished && snap.result.is_some();
        let (mut out_tail, mut err_tail, mut truncated) = (String::new(), String::new(), false);
        if let Some(s) = snap.steps.last() {
            let tail = |r: &Option<String>| -> (String, bool) {
                match r
                    .as_deref()
                    .and_then(|r| self.runtime.evidence(r, Some(64 * 1024)).ok())
                {
                    Some(c) if c.encoding == "utf-8" => (
                        tail_of(&c.content, OUTPUT_TAIL),
                        c.truncated || c.content.len() > OUTPUT_TAIL,
                    ),
                    _ => (String::new(), false),
                }
            };
            let (o, ot) = tail(&s.stdout_ref);
            let (e, et) = tail(&s.stderr_ref);
            out_tail = o;
            err_tail = e;
            truncated = ot || et || s.stdout_truncated || s.stderr_truncated;
        }
        let outcome = StepOutcome {
            execution_id: exec_id.clone(),
            process: snap.process.clone(),
            task_status: snap.task_status,
            verification: snap.verification,
            outcome_known: known,
        };
        let __c = self.cell(&m.task_id);
        let _g = __c.op.lock().unwrap();
        let fresh = self.load(j, &m.task_id)?;
        if fresh
            .step(&step.view.step_id)
            .is_some_and(|s| s.observed_seq.is_some())
        {
            return Ok(Tick::Progress); // already recorded
        }
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::StepObserved {
                step_id: step.view.step_id.clone(),
                execution_id: exec_id.clone(),
                outcome,
                stdout_tail: out_tail,
                stderr_tail: err_tail,
                output_truncated: truncated,
            },
        )?;
        if !known {
            let fresh = self.load(j, &m.task_id)?;
            self.block(
                j,
                &fresh,
                &format!("execution {exec_id} was interrupted and its outcome is unknown; its effects may or may not have happened. It will not be repeated automatically: resume to choose a new step, which needs its own approval"),
            )?;
        }
        Ok(Tick::Progress)
    }

    fn verify(&self, j: &cli2_evidence::Journal, m: &Model) -> Result<Tick> {
        let __c = self.cell(&m.task_id);
        let _g = __c.op.lock().unwrap();
        let m = self.load(j, &m.task_id)?;
        if m.state.is_terminal() || m.cancel_requested.is_some() {
            return Ok(Tick::Progress);
        }
        if m.state == TaskState::Executing {
            self.transition(
                j,
                &m,
                TaskState::Verifying,
                "judging the acceptance criteria from recorded evidence",
            )?;
        }
        let m = self.load(j, &m.task_id)?;
        let statuses = evaluate(&m.criteria, &m.steps);
        self.append(
            j,
            &m.task_id,
            DevTaskEventData::CriteriaEvaluated {
                statuses: statuses.clone(),
            },
        )?;
        let last = m.steps.last().expect("verify runs after a step");
        let failed_run = last.view.outcome.as_ref().is_some_and(|o| {
            o.outcome_known && !matches!(o.task_status, Some(cli2_core::Status::Succeeded))
        });
        let mut attempts = m.attempts;
        if failed_run && !all_passed(&statuses) {
            attempts += 1;
            self.append(
                j,
                &m.task_id,
                DevTaskEventData::AttemptCounted {
                    attempts,
                    reason: format!("step {} did not succeed", last.view.step_id),
                },
            )?;
        }
        let m = self.load(j, &m.task_id)?;
        if m.state == TaskState::Blocked {
            return Ok(Tick::Progress); // an unknown outcome already stopped the task
        }
        if all_passed(&statuses) {
            self.transition(
                j,
                &m,
                TaskState::Completed,
                "every acceptance criterion was verified",
            )?;
            let done = self.load(j, &m.task_id)?;
            let rep = report(&done, Some(ReportOutcome::Verified));
            self.append(
                j,
                &m.task_id,
                DevTaskEventData::Completed {
                    report: Box::new(rep),
                },
            )?;
            return Ok(Tick::Done);
        }
        if attempts >= m.limits.max_attempts {
            self.finish_failed(j, &m, &format!("{attempts} attempts failed (the limit is {}); the acceptance criteria are not met", m.limits.max_attempts))?;
            return Ok(Tick::Done);
        }
        if m.steps.len() as u32 >= m.limits.max_steps {
            self.finish_failed(
                j,
                &m,
                &format!(
                    "the step limit ({}) was reached before every criterion was verified",
                    m.limits.max_steps
                ),
            )?;
            return Ok(Tick::Done);
        }
        self.transition(
            j,
            &m,
            TaskState::Investigating,
            "criteria remain unmet; choosing the next step from what was observed",
        )?;
        Ok(Tick::Progress)
    }

    fn do_cancel(&self, j: &cli2_evidence::Journal, m: &Model, reason: &str) -> Result<Tick> {
        // Stop what is running first, and record what it really did.
        if let Some(step) = m.steps.last() {
            if step.view.status == StepStatus::Executing {
                if let Some(x) = &step.view.execution_id {
                    let _ = self.runtime.cancel(x);
                    loop {
                        let b = self.runtime.events(&EventsParams {
                            execution_id: x.clone(),
                            after_seq: u64::MAX,
                            wait_ms: 500,
                            limit: Some(1),
                        })?;
                        if b.finished {
                            break;
                        }
                    }
                    let snap = self.runtime.execution(x)?;
                    let known = snap.lifecycle == Lifecycle::Finished && snap.result.is_some();
                    let outcome = StepOutcome {
                        execution_id: x.clone(),
                        process: snap.process,
                        task_status: snap.task_status,
                        verification: snap.verification,
                        outcome_known: known,
                    };
                    self.append(
                        j,
                        &m.task_id,
                        DevTaskEventData::StepObserved {
                            step_id: step.view.step_id.clone(),
                            execution_id: x.clone(),
                            outcome,
                            stdout_tail: String::new(),
                            stderr_tail: String::new(),
                            output_truncated: false,
                        },
                    )?;
                }
            } else if matches!(
                step.view.status,
                StepStatus::AwaitingApproval | StepStatus::Approved
            ) {
                // The operation was never run and now never can be.
                if let Some(pid) = &step.view.plan_id {
                    let _ = self
                        .runtime
                        .reject(pid, Some("the task was cancelled".into()));
                    self.append(
                        j,
                        &m.task_id,
                        DevTaskEventData::StepRejected {
                            step_id: step.view.step_id.clone(),
                            plan_id: pid.clone(),
                            reason: "the task was cancelled".into(),
                        },
                    )?;
                }
            }
        }
        let m = self.load(j, &m.task_id)?;
        self.transition(j, &m, TaskState::Cancelled, reason)?;
        Ok(Tick::Done)
    }
}

fn tail_of(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Summaries of capabilities that can actually be planned: project tasks, and
/// executables whose interface is understood.
fn briefs(caps: &[Capability]) -> Vec<CapabilityBrief> {
    let mut v: Vec<CapabilityBrief> = caps
        .iter()
        .filter(|c| {
            c.kind == CapabilityKind::ProjectTask
                || c.interface.completeness >= InterfaceCompleteness::Inferred
        })
        .map(|c| CapabilityBrief {
            id: c.id.clone(),
            kind: format!("{:?}", c.kind).to_lowercase(),
            name: c.name.clone(),
            purpose: c.purpose.as_ref().map(|p| p.value.clone()),
            interface: format!("{:?}", c.interface.completeness).to_lowercase(),
            effects: format!("{:?}", c.effects.certainty).to_lowercase(),
        })
        .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// `controller.model_command` in config.json: a local program used as the model.
fn model_from_config(home: &std::path::Path) -> Option<CommandModelClient> {
    let text = std::fs::read_to_string(home.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let c = v.get("controller")?;
    let command: Vec<String> = c
        .get("model_command")?
        .as_array()?
        .iter()
        .filter_map(|s| s.as_str().map(String::from))
        .collect();
    if command.is_empty() {
        return None;
    }
    let secs = c
        .get("model_timeout_secs")
        .and_then(|s| s.as_u64())
        .unwrap_or(60)
        .clamp(1, 600);
    Some(CommandModelClient {
        command,
        timeout: Duration::from_secs(secs),
    })
}

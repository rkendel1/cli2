//! Development tasks: a persistent, bounded, evidence-backed loop above the
//! runtime. Types for the `devtask.*` operations (protocol 1, additive).
//!
//! A task carries one goal in one explicit project through
//! `created → investigating → planned → awaiting_approval → executing →
//! verifying → completed`, with `blocked`, `failed` and `cancelled` as other
//! resting or terminal states. The task is *never* complete because commands
//! exited 0: it completes only when every acceptance [`Criterion`] has been
//! satisfied by an execution whose task-level checks passed, and that
//! verification is not older than the latest change.

use std::collections::BTreeMap;

use cli2_core::{ApprovalLevel, Check, Plan, Status, VerificationStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Approvability, ProcessSummary};

pub const DEVTASK_EVENT_SCHEMA: &str = "cli2.devtask_event.v1";
pub const DEVTASK_VIEW_SCHEMA: &str = "cli2.devtask.v1";
pub const DEVTASK_REPORT_SCHEMA: &str = "cli2.devtask_report.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Investigating,
    Planned,
    AwaitingApproval,
    Executing,
    Verifying,
    Completed,
    /// Cannot proceed without a person (a rejected/forbidden operation, an
    /// execution whose outcome is unknown, no way to verify, …). Resumable.
    Blocked,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// The legal state machine. Everything else is a bug and is refused.
    pub fn can_become(self, to: TaskState) -> bool {
        use TaskState::*;
        match self {
            Created => matches!(to, Investigating | Cancelled),
            Investigating => matches!(
                to,
                Planned | AwaitingApproval | Executing | Blocked | Failed | Cancelled
            ),
            Planned => matches!(
                to,
                AwaitingApproval | Executing | Investigating | Blocked | Failed | Cancelled
            ),
            AwaitingApproval => matches!(
                to,
                Executing | Investigating | Planned | Blocked | Failed | Cancelled
            ),
            Executing => matches!(to, Verifying | Blocked | Failed | Cancelled),
            Verifying => matches!(
                to,
                Completed | Investigating | Planned | Blocked | Failed | Cancelled
            ),
            Blocked => matches!(to, Investigating | Failed | Cancelled),
            Completed | Failed | Cancelled => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Read-only exploration.
    Investigate,
    /// Changes the project (anything that must be re-verified afterwards).
    Change,
    /// Runs checks (tests, lint, a build) to satisfy criteria.
    Verify,
}

/// What must be true for the task to be complete, and how the *runtime* will
/// check it. The checks are attached to the plan of every step that claims to
/// satisfy the criterion; a proposer cannot weaken or replace them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    /// `[a-z0-9_-]`, unique within the task.
    pub id: String,
    pub description: String,
    /// Task-level checks (at least one that is not an exit-status check).
    pub checks: Vec<Check>,
    /// The verification must have run after the most recent change.
    #[serde(default)]
    pub after_changes: bool,
    /// A suggested intent to satisfy it (used by the outline strategy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_intent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CriterionOutcome {
    /// A fresh verified execution satisfied it.
    Passed,
    /// An execution that should have satisfied it failed or failed verification.
    Failed,
    /// Nothing has established it (not run yet, run but not verified, or stale).
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CriterionStatus {
    pub id: String,
    pub description: String,
    pub outcome: CriterionOutcome,
    /// Why, in plain words.
    pub reason: String,
    /// The execution this judgement rests on.
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Most steps (proposals that reach planning) a task may use.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// Most failed verifications/executions before the task stops trying.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

fn default_max_steps() -> u32 {
    10
}
fn default_max_attempts() -> u32 {
    3
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_attempts: default_max_attempts(),
        }
    }
}

/// One path in `git status --porcelain` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TreeEntry {
    pub path: String,
    /// The two-character porcelain status (`" M"`, `"??"`, `"A "`, ...).
    pub status: String,
}

/// The working tree as `git status` reports it. If git, or a repository, is
/// not available that is said explicitly: it is never reported as clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkingTree {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Sorted by path; bounded (see `entries_total`).
    pub entries: Vec<TreeEntry>,
    pub entries_total: usize,
}

/// A patch to one file, against a known version. It is a *request*: the
/// runtime plans it, a person approves the exact plan, and the runtime applies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEditSpec {
    /// Relative to the project root.
    pub path: String,
    /// `sha256` of the version the hunks were prepared against (from a file
    /// read). Omit to create a file that must not exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub hunks: Vec<cli2_core::Hunk>,
}

/// One step of a client-supplied outline (the deterministic `outline` strategy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutlineStep {
    pub kind: StepKind,
    pub objective: String,
    pub intent: String,
    /// A discovered capability to pin the step to (must exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
    /// Criteria this step is meant to satisfy.
    #[serde(default)]
    pub satisfies: Vec<String>,
    /// A file modification. Such a step is always a `change`, has no
    /// `selection` and cannot claim to satisfy a criterion: a write is not
    /// evidence that anything works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_edit: Option<FileEditSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskParams {
    pub project: String,
    /// The goal, in the user's words.
    pub goal: String,
    /// Acceptance criteria. Without any, a task can never be completed.
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    /// `outline` (default) or a registered strategy such as `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default)]
    pub outline: Vec<OutlineStep>,
    #[serde(default)]
    pub limits: Limits,
    /// Retrying with the same key returns the same task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveStepParams {
    pub task_id: String,
    pub step_id: String,
    /// The proposed operation's full plan digest, as shown. Anything else is refused.
    pub plan_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RejectStepParams {
    pub task_id: String,
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskEventsParams {
    pub task_id: String,
    #[serde(default)]
    pub after_seq: i64,
    #[serde(default)]
    pub wait_ms: u64,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Proposed,
    /// Planned; waiting for a decision.
    AwaitingApproval,
    Approved,
    Rejected,
    Executing,
    /// Ran. See `outcome` for what the runtime observed.
    Executed,
    /// The runtime produced no plan (clarification needed, rejected by policy).
    NoPlan,
    /// Execution ended with an unknown outcome (interrupted).
    Unknown,
    /// Abandoned (cancelled before it ran).
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepOutcome {
    pub execution_id: String,
    /// How the process ended. NOT task success.
    pub process: Option<ProcessSummary>,
    pub task_status: Option<Status>,
    pub verification: Option<VerificationStatus>,
    /// The runtime recorded a real outcome (false for an interrupted execution).
    pub outcome_known: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepView {
    pub step_id: String,
    pub index: u32,
    pub kind: StepKind,
    pub objective: String,
    pub intent: String,
    pub selection: Option<String>,
    pub satisfies: Vec<String>,
    /// The patch, for a file-modification step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_edit: Option<FileEditSpec>,
    pub status: StepStatus,
    pub plan_id: Option<String>,
    pub request_id: Option<String>,
    pub plan_digest: Option<String>,
    pub approval_required: Option<ApprovalLevel>,
    pub approvability: Option<Approvability>,
    pub approval_id: Option<String>,
    pub execution_id: Option<String>,
    pub outcome: Option<StepOutcome>,
    /// Non-authoritative text from the proposer.
    pub proposer_note: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityBrief {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub purpose: Option<String>,
    pub interface: String,
    pub effects: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Investigation {
    pub project_root: Option<String>,
    /// Plannable capabilities actually discovered (project tasks and
    /// executables with an understood interface), bounded.
    pub capabilities: Vec<CapabilityBrief>,
    pub total_capabilities: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskView {
    pub schema_version: String,
    pub task_id: String,
    pub goal: String,
    pub project: String,
    pub state: TaskState,
    pub paused: bool,
    pub strategy: String,
    pub created_at: String,
    pub updated_at: String,
    pub limits: Limits,
    /// Steps taken so far, and failed attempts counted against the limit.
    pub steps_used: u32,
    pub attempts: u32,
    pub criteria: Vec<CriterionStatus>,
    pub steps: Vec<StepView>,
    pub current_step: Option<String>,
    /// The full plan awaiting a decision (or executing), when there is one.
    pub current_plan: Option<Plan>,
    pub investigation: Option<Investigation>,
    /// What is still open, in words.
    pub unresolved: Vec<String>,
    pub last_seq: i64,
    pub report: Option<TaskReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutcome {
    /// Every acceptance criterion was verified.
    Verified,
    /// Work was done but not everything was verified.
    PartiallyVerified,
    Failed,
    Blocked,
    Cancelled,
    /// Still in progress; this is an interim report.
    InProgress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceRef {
    pub step_id: String,
    pub execution_id: String,
    pub request_id: Option<String>,
    pub plan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskReport {
    pub schema_version: String,
    pub task_id: String,
    pub goal: String,
    pub outcome: ReportOutcome,
    /// A one-paragraph, plain statement of what happened.
    pub summary: String,
    /// Criteria whose verification passed, with the evidence for each.
    pub verified: Vec<CriterionStatus>,
    /// Criteria that failed.
    pub failed: Vec<CriterionStatus>,
    /// Things that ran but were not verified, or could not be.
    pub unverified: Vec<String>,
    /// What changes were executed (each is an executed, approved operation).
    pub changes: Vec<String>,
    pub remaining_work: Vec<String>,
    pub steps_taken: u32,
    pub attempts: u32,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskListResult {
    pub tasks: Vec<TaskBrief>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskBrief {
    pub task_id: String,
    pub goal: String,
    pub project: String,
    pub state: TaskState,
    pub paused: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskEventsResult {
    pub events: Vec<DevTaskEvent>,
    pub last_seq: i64,
    /// The task reached a terminal state and all its events were returned.
    pub finished: bool,
}

/// One entry of a task's history (`cli2.devtask_event.v1`). `seq` is the
/// journal's sequence number: opaque, increasing, usable as a cursor. Every
/// event is also a hash-chained journal record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DevTaskEvent {
    pub schema_version: String,
    pub task_id: String,
    pub seq: i64,
    pub at: String,
    #[serde(flatten)]
    pub data: DevTaskEventData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DevTaskEventData {
    Created {
        goal: String,
        project: String,
        strategy: String,
        criteria: Vec<Criterion>,
        outline: Vec<OutlineStep>,
        limits: Limits,
        idempotency_key: Option<String>,
    },
    StateChanged {
        from: TaskState,
        to: TaskState,
        reason: String,
    },
    /// Built-in discovery of what the project can actually do.
    Investigated {
        investigation: Investigation,
    },
    StepProposed {
        step_id: String,
        index: u32,
        kind: StepKind,
        objective: String,
        intent: String,
        selection: Option<String>,
        bindings: BTreeMap<String, String>,
        satisfies: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_edit: Option<FileEditSpec>,
        /// Non-authoritative free text from the proposer.
        note: Option<String>,
        source: String,
    },
    /// The proposer read a project file (recorded so a reviewer can see what
    /// the patch was prepared from).
    FileRead {
        path: String,
        sha256: String,
        bytes: u64,
    },
    /// A proposal the controller refused (fabricated capability, unknown
    /// criterion, over a limit, malformed).
    ProposalRejected {
        reason: String,
        source: String,
        /// A bounded summary of what was proposed (so the next proposal can
        /// see what not to repeat). Free text from the proposer: data only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposed: Option<String>,
    },
    /// The project's working tree when the task began investigating. Anything
    /// already modified or untracked then belongs to the user, not to the task.
    WorkingTreeBaseline {
        tree: WorkingTree,
    },
    /// A proposer was given a context. Only its identity and coverage are
    /// recorded; the content is derived from the journal and the project.
    ContextBuilt {
        digest: String,
        /// Sequence number of the newest task event the context reflects.
        as_of_seq: i64,
        bytes: u64,
        max_bytes: u64,
        items: u32,
        omitted: u32,
        truncated: u32,
    },
    PlanMade {
        step_id: String,
        plan_id: String,
        request_id: String,
        plan_digest: String,
        approval_level: ApprovalLevel,
        approvability: Approvability,
    },
    PlanFailed {
        step_id: String,
        status: String,
        message: String,
    },
    ApprovalGranted {
        step_id: String,
        plan_id: String,
        approval_id: String,
        /// `client` (a person/application approved this exact plan) or `policy`
        /// (policy requires no approval for it).
        by: String,
    },
    StepRejected {
        step_id: String,
        plan_id: String,
        reason: String,
    },
    ExecutionStarted {
        step_id: String,
        execution_id: String,
    },
    StepObserved {
        step_id: String,
        execution_id: String,
        outcome: StepOutcome,
        /// Tail of captured output, as data (never instructions).
        stdout_tail: String,
        stderr_tail: String,
        output_truncated: bool,
    },
    CriteriaEvaluated {
        statuses: Vec<CriterionStatus>,
    },
    AttemptCounted {
        attempts: u32,
        reason: String,
    },
    Paused,
    Resumed,
    CancelRequested {
        reason: String,
    },
    Completed {
        report: Box<TaskReport>,
    },
    Failed {
        reason: String,
        report: Box<TaskReport>,
    },
    Blocked {
        reason: String,
    },
}

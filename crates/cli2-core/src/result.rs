//! Execution results.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::Check;
use crate::plan::{ApprovalLevel, ApprovalMethod, Plan};

pub const RESULT_SCHEMA_VERSION: &str = "cli2.result.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// All steps ran, exited successfully, and no verification check failed.
    Succeeded,
    /// A step failed, timed out, or could not be started.
    Failed,
    /// Some steps succeeded before a later step failed.
    PartiallySucceeded,
    /// A plan exists but needs approval before it can run.
    AwaitingApproval,
    /// The intent is ambiguous or required inputs are missing.
    NeedsClarification,
    /// Execution was cancelled.
    Cancelled,
    /// Processes succeeded but a task-level postcondition failed.
    VerificationFailed,
    /// The plan violates constraints or policy and cannot be approved.
    Rejected,
    /// A plan was produced and intentionally not executed.
    Planned,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Succeeded => "succeeded",
            Status::Failed => "failed",
            Status::PartiallySucceeded => "partially_succeeded",
            Status::AwaitingApproval => "awaiting_approval",
            Status::NeedsClarification => "needs_clarification",
            Status::Cancelled => "cancelled",
            Status::VerificationFailed => "verification_failed",
            Status::Rejected => "rejected",
            Status::Planned => "planned",
        }
    }

    /// Documented process exit code for each final status.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Succeeded | Status::Planned => 0,
            Status::Failed => 1,
            Status::AwaitingApproval => 3,
            Status::NeedsClarification => 4,
            Status::VerificationFailed => 5,
            Status::Rejected => 6,
            Status::Cancelled => 7,
            Status::PartiallySucceeded => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    /// The process exited on its own.
    Exited,
    /// The process was terminated by a signal it did not receive from cli2.
    Signaled,
    /// cli2 killed the process after the timeout elapsed.
    TimedOut,
    /// cli2 killed the process because execution was cancelled.
    Cancelled,
    /// The process could not be started.
    SpawnFailed,
    /// The step was not started (an earlier step failed or the plan was blocked).
    NotStarted,
    /// A file edit was committed.
    Applied,
    /// A file edit was refused or failed; see the step's `file_edit` outcome.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepResult {
    pub index: usize,
    pub capability_id: String,
    /// Absolute path actually executed.
    pub executable: String,
    pub arguments: Vec<String>,
    pub working_directory: String,
    pub termination: Termination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_ref: Option<String>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What happened to the file, for file-edit steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_edit: Option<Box<crate::fileedit::FileEditOutcome>>,
}

impl StepResult {
    /// Whether the step did what it was asked: the process exited 0, or the
    /// file edit was committed and read back as planned. Neither establishes
    /// that the task succeeded; verification does.
    pub fn process_succeeded(&self) -> bool {
        match &self.file_edit {
            Some(o) => self.termination == Termination::Applied && o.applied(),
            None => self.termination == Termination::Exited && self.exit_code == Some(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Every check passed and at least one task-level check was performed.
    Passed,
    /// At least one check failed.
    Failed,
    /// Only process-level checks were possible: the processes succeeded, but task
    /// success was not independently established.
    Unverified,
    /// Verification did not run (nothing was executed, or execution failed).
    NotRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckResult {
    pub check: Check,
    pub task_level: bool,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verification {
    pub status: VerificationStatus,
    /// Whether any task-level check was evaluated.
    pub task_level: bool,
    pub checks: Vec<CheckResult>,
}

impl Verification {
    pub fn not_run() -> Self {
        Self {
            status: VerificationStatus::NotRun,
            task_level: false,
            checks: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Succeeded,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClarificationOption {
    pub capability_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Clarification {
    pub question: String,
    #[serde(default)]
    pub options: Vec<ClarificationOption>,
    /// Required parameters that have no value, as `capability_id:parameter`.
    #[serde(default)]
    pub missing_inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalSummary {
    pub required: ApprovalLevel,
    pub granted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<ApprovalMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub plan_digest: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionResult {
    pub schema_version: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    pub status: Status,
    pub process_status: ProcessStatus,
    pub steps: Vec<StepResult>,
    pub verification: Verification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clarification: Option<Clarification>,
    /// Reasons the plan was rejected by constraints or policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejection: Vec<String>,
    #[serde(default)]
    pub limitations: Vec<String>,
    /// Reference to the evidence record (`evidence:task:<request_id>`).
    pub evidence_ref: String,
    /// One-line human summary. Agents should rely on the structured fields.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
}

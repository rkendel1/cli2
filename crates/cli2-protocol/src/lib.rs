//! The CLI 2.0 local protocol, version 1.
//!
//! A client talks to a `cli2 serve` runtime over a local Unix-domain socket
//! using newline-delimited JSON: one [`Request`] per line, one [`Response`] per
//! line, strictly in order. Execution progress is delivered as typed
//! [`Event`]s that the client pulls with a cursor (`execution.events`), so a
//! client that disconnects can reconnect and continue from where it was.
//!
//! The protocol keeps these things apart, in separate fields and operations:
//!
//! * **intent** (what was asked) and **capability** (what was selected);
//! * **plan** (a proposal, never executing) and **execution** (a run);
//! * **approval** (a decision bound to a plan ID and its exact digest) and
//!   **authorization** (the runtime's check immediately before running);
//! * **process status** (how the process ended), **task status**, and
//!   **verification** (whether the goal was independently established);
//! * **historical evidence** (what happened) and **current validity** (whether
//!   it still holds).
//!
//! Nothing a client sends is evidence. Statuses are produced only by the
//! runtime from what it observed, and a successful process exit is never task
//! success on its own.
//!
//! Plans, results, capabilities, approvals and checks reuse the existing
//! versioned CLI contracts (`cli2.plan.v1`, `cli2.result.v1`, …) from
//! `cli2-core`; task, workflow and history documents are passed through as the
//! existing JSON (`cli2.task.v1`, `cli2.workflow_replay.v1`, …).

pub mod devtask;

use std::collections::BTreeMap;

use cli2_core::{
    ApprovalLevel, ApprovalRecord, Capability, Check, Constraints, ExecutionResult, Hunk, Plan,
    Status, StepResult, Termination, Verification, VerificationStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_NAME: &str = "cli2.protocol";
/// The version this crate speaks. The negotiated version is chosen in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;
pub const SUPPORTED_VERSIONS: &[u32] = &[1];
pub const EVENT_SCHEMA: &str = "cli2.event.v1";
/// Largest request or response line the runtime accepts or sends.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Longest a single `execution.events` call may wait.
pub const MAX_EVENT_WAIT_MS: u64 = 30_000;

// ------------------------------------------------------------------- envelope

/// One request line. `id` is chosen by the client and echoed in the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub id: String,
    /// The operation name (see [`ops`]).
    pub op: String,
    #[serde(default)]
    pub params: Value,
}

/// One response line. Exactly one of `result` and `error` is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: impl Serialize) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(serde_json::to_value(result).expect("result serializes")),
            error: None,
        }
    }
    pub fn err(id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// Operation names.
pub mod ops {
    pub const HELLO: &str = "hello";
    pub const PROJECT_SCAN: &str = "project.scan";
    pub const CAPABILITIES_LIST: &str = "capabilities.list";
    pub const INTENT_PLAN: &str = "intent.plan";
    pub const PLAN_GET: &str = "plan.get";
    pub const PLAN_APPROVE: &str = "plan.approve";
    pub const PLAN_REJECT: &str = "plan.reject";
    pub const EXECUTION_START: &str = "execution.start";
    pub const EXECUTION_GET: &str = "execution.get";
    pub const EXECUTION_EVENTS: &str = "execution.events";
    pub const EXECUTION_CANCEL: &str = "execution.cancel";
    pub const TASK_LIST: &str = "task.list";
    pub const TASK_GET: &str = "task.get";
    pub const EVIDENCE_READ: &str = "evidence.read";
    pub const OUTPUT_READ: &str = "output.read";
    pub const HISTORY_SEARCH: &str = "history.search";
    pub const WORKFLOW_GET: &str = "workflow.get";
    pub const WORKFLOW_REPLAN: &str = "workflow.replan";
    pub const FILE_READ: &str = "file.read";
    pub const PLAN_EDIT: &str = "plan.edit";
    pub const DEVTASK_CREATE: &str = "devtask.create";
    pub const DEVTASK_GET: &str = "devtask.get";
    pub const DEVTASK_LIST: &str = "devtask.list";
    pub const DEVTASK_START: &str = "devtask.start";
    pub const DEVTASK_APPROVE: &str = "devtask.approve";
    pub const DEVTASK_REJECT: &str = "devtask.reject";
    pub const DEVTASK_PAUSE: &str = "devtask.pause";
    pub const DEVTASK_RESUME: &str = "devtask.resume";
    pub const DEVTASK_CANCEL: &str = "devtask.cancel";
    pub const DEVTASK_EVENTS: &str = "devtask.events";
    pub const DEVTASK_REPORT: &str = "devtask.report";

    /// Every operation of protocol version 1.
    pub const ALL: &[&str] = &[
        HELLO,
        PROJECT_SCAN,
        CAPABILITIES_LIST,
        INTENT_PLAN,
        PLAN_GET,
        PLAN_APPROVE,
        PLAN_REJECT,
        EXECUTION_START,
        EXECUTION_GET,
        EXECUTION_EVENTS,
        EXECUTION_CANCEL,
        TASK_LIST,
        TASK_GET,
        EVIDENCE_READ,
        OUTPUT_READ,
        HISTORY_SEARCH,
        WORKFLOW_GET,
        WORKFLOW_REPLAN,
        FILE_READ,
        PLAN_EDIT,
        DEVTASK_CREATE,
        DEVTASK_GET,
        DEVTASK_LIST,
        DEVTASK_START,
        DEVTASK_APPROVE,
        DEVTASK_REJECT,
        DEVTASK_PAUSE,
        DEVTASK_RESUME,
        DEVTASK_CANCEL,
        DEVTASK_EVENTS,
        DEVTASK_REPORT,
    ];
}

// --------------------------------------------------------------------- errors

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line was not a valid request envelope.
    InvalidRequest,
    /// The parameters did not match the operation's schema or limits.
    InvalidParams,
    /// No protocol version in common (see `supported_versions` in the details).
    UnsupportedProtocolVersion,
    /// The server does not implement this operation.
    UnsupportedOperation,
    /// `hello` must be the first request on a connection.
    NotInitialized,
    /// The peer is not an authorized local client.
    Unauthorized,
    NotFound,
    /// `project` is not an absolute path to an existing directory.
    ProjectInvalid,
    /// The stored plan no longer matches its digest.
    PlanModified,
    /// The approval does not belong to this plan and digest.
    ApprovalMismatch,
    /// The plan needs an approval that was not provided.
    ApprovalRequired,
    /// Policy forbids this operation; it cannot be approved.
    Forbidden,
    /// The environment changed since the plan was made; plan again.
    StalePlan,
    PlanRejected,
    AlreadyExecuted,
    /// The request conflicts with current state (e.g. an idempotency key reused with different inputs).
    Conflict,
    /// A file edit was prepared against a version of the file that is no longer current.
    StalePatch,
    /// The runtime is at capacity; retry later.
    Busy,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    /// Whether retrying the same request may succeed.
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: matches!(code, ErrorCode::Busy),
            details: None,
        }
    }
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

// ----------------------------------------------------------------- hello

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    pub client: ClientInfo,
    /// Protocol versions the client can speak, any order.
    pub versions: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    pub max_message_bytes: usize,
    pub max_event_wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    /// The highest version both sides support; used for the rest of the connection.
    pub protocol_version: u32,
    pub supported_versions: Vec<u32>,
    pub server: ClientInfo,
    /// Identifies this runtime instance. It owns the executions it starts.
    pub instance_id: String,
    pub operations: Vec<String>,
    pub limits: Limits,
}

// -------------------------------------------------------------- projects etc.

/// The project context every project-scoped request names explicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectParams {
    /// Absolute path to a local directory.
    pub project: String,
}

/// List what cli2 knows how to run. Without `kinds` this is everything discovered, including
/// every executable on `PATH`, which on a real machine can be larger than a protocol message.
/// A client that wants only a project's own tasks says so.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesListParams {
    /// Absolute path to a local directory.
    pub project: String,
    /// Only capabilities of these kinds (`executable`, `project_task`, `shell_operation`).
    /// Absent or empty: all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<cli2_core::CapabilityKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScanSummary {
    pub project: String,
    pub project_root: Option<String>,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilitiesResult {
    pub project: String,
    pub capabilities: Vec<Capability>,
}

// ------------------------------------------------------------------- planning

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanParams {
    pub project: String,
    pub intent: String,
    /// Retrying with the same key (and same inputs) returns the same plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(default)]
    pub expectations: Vec<Check>,
    /// Consent to run `--help` of the best unknown-interface match (bounded).
    #[serde(default)]
    pub capture_help: bool,
    /// `deterministic` (default), `apple-fm` or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,
    /// Bound each process of the plan to this many seconds (1–3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Read one text file of a project. Reads are bounded and follow the same
/// containment rules as edits: no symbolic links, regular files only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileReadParams {
    pub project: String,
    /// Relative to the project root.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileReadResult {
    pub path: String,
    /// The version identifier: pass it as `expected_sha256` when proposing an edit.
    pub sha256: String,
    pub bytes: u64,
    pub lines: u64,
    pub content: String,
}

/// Plan a modification of one file as a patch against a known version.
/// Nothing is written until the returned plan is approved by digest and started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanEditParams {
    pub project: String,
    pub path: String,
    /// `sha256` of the file as it was when the patch was prepared (from
    /// `file.read`). Omit to create a file that must not exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub hunks: Vec<Hunk>,
    /// Why; shown to the approver. Defaults to `Edit <path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Retrying with the same key (and same inputs) returns the same plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub constraints: Constraints,
}

/// What approval a plan needs before it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Approvability {
    /// No plan exists to approve (clarification needed, or rejected by policy).
    NoPlan,
    /// Policy allows it to run without an approval decision.
    NotRequired,
    /// It may run once approved against its exact digest.
    RequiresApproval,
    /// Policy forbids it; no approval can authorize it.
    Forbidden,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanResult {
    /// The runtime's answer, exactly as `cli2.result.v1`: status `planned`
    /// (with `plan`), `needs_clarification`, or `rejected`. Planning never executes.
    pub outcome: ExecutionResult,
    pub approvability: Approvability,
    pub required_approval: Option<ApprovalLevel>,
    /// True when this answer repeats an earlier request with the same idempotency key.
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanIdParams {
    pub plan_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionRef {
    pub execution_id: String,
    pub approval_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanView {
    pub plan: Plan,
    pub approvability: Approvability,
    pub rejected: bool,
    /// Approval decisions recorded for this plan.
    pub approvals: Vec<ApprovalRecord>,
    /// Executions started for this plan.
    pub executions: Vec<ExecutionRef>,
    /// Reasons the plan could no longer be run today (empty when current).
    pub stale_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveParams {
    pub plan_id: String,
    /// The plan's full digest, as returned in the plan. Proves the approver
    /// saw this exact plan; any other digest is refused.
    pub plan_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApproveResult {
    pub approval: ApprovalRecord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RejectParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RejectResult {
    /// The recorded result: status `cancelled`, nothing executed.
    pub outcome: ExecutionResult,
}

// ------------------------------------------------------------------ execution

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    pub plan_id: String,
    /// The approval from `plan.approve`. Required unless the plan needs none.
    /// An approval authorizes at most one execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// The runtime is executing it.
    Running,
    /// A cancel was accepted; the process is being terminated.
    Cancelling,
    /// Finished; `result` holds the durable outcome.
    Finished,
    /// The runtime that started it ended without recording an outcome. Whether
    /// the command completed is unknown; no success is claimed.
    InterruptedUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartResult {
    pub execution_id: String,
    pub plan_id: String,
    pub request_id: String,
    pub lifecycle: Lifecycle,
    /// True when this approval had already started this execution (a retry).
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIdParams {
    pub execution_id: String,
}

/// How the process ended, separate from whether the task succeeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSummary {
    pub termination: Termination,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionSnapshot {
    pub execution_id: String,
    pub plan_id: String,
    pub request_id: String,
    pub approval_id: Option<String>,
    pub lifecycle: Lifecycle,
    /// `live` while the owning runtime holds it; `journal` when reconstructed from durable evidence.
    pub source: String,
    pub started_at: String,
    /// Sequence number of the newest event (cursor for `execution.events`).
    pub last_seq: u64,
    /// Steps that have finished so far.
    pub steps: Vec<StepResult>,
    /// Process status of the last finished step. NOT task success.
    pub process: Option<ProcessSummary>,
    /// The request's overall status, once finished.
    pub task_status: Option<Status>,
    /// Independent verification, once finished.
    pub verification: Option<VerificationStatus>,
    /// The durable final result (`cli2.result.v1`), once finished.
    pub result: Option<ExecutionResult>,
    /// Set when the runtime could not complete the execution (no outcome is
    /// claimed in that case).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsParams {
    pub execution_id: String,
    /// Return events with `seq` greater than this (0 = from the start).
    #[serde(default)]
    pub after_seq: u64,
    /// Wait up to this long for new events (capped at [`MAX_EVENT_WAIT_MS`]).
    #[serde(default)]
    pub wait_ms: u64,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventsResult {
    pub events: Vec<Event>,
    pub last_seq: u64,
    /// No further events will be produced.
    pub finished: bool,
    /// Events before this point were not retained in memory (output beyond
    /// the live buffer); the full output is in the evidence blobs.
    pub gap_before_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A typed execution event (`cli2.event.v1`). `seq` is per execution, from 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub schema_version: String,
    pub execution_id: String,
    pub seq: u64,
    pub at: String,
    #[serde(flatten)]
    pub data: EventData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    ExecutionStarted {
        plan_id: String,
        request_id: String,
        approval_id: Option<String>,
    },
    StepStarted {
        index: usize,
        capability_id: String,
        argv: Vec<String>,
        working_directory: String,
        pid: u32,
    },
    /// A chunk of output. stdout and stderr are never mixed.
    Output {
        index: usize,
        stream: Stream,
        /// UTF-8 text (invalid bytes replaced; `lossy` says so).
        text: String,
        lossy: bool,
        /// Byte offset of this chunk within its stream.
        offset: u64,
        bytes: usize,
    },
    StepFinished {
        result: Box<StepResult>,
    },
    Verification {
        verification: Verification,
    },
    /// The durable outcome. `result.status` is the task status; the process
    /// status and verification are separate fields of the result.
    ExecutionFinished {
        result: Box<ExecutionResult>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CancelResult {
    pub execution_id: String,
    /// A cancel was delivered to a live execution.
    pub accepted: bool,
    pub lifecycle: Lifecycle,
}

// ---------------------------------------------------------- evidence & history

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskGetParams {
    /// A request ID or plan ID.
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DocumentResult {
    /// The schema of `document` (`cli2.task.v1`, `cli2.history_search.v1`, …).
    pub schema: String,
    pub document: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceParams {
    /// An `evidence:sha256:…` reference.
    pub reference: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputParams {
    /// `out_N`, `out_N.S`, `exec_N` or `latest` (see the CLI's `output` command).
    pub output_id: String,
    /// `stdout`, `stderr`, `response` or `command-and-response`.
    pub part: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContentResult {
    pub encoding: String,
    pub content: String,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistorySearchParams {
    pub project: String,
    pub text: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub verified_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowParams {
    pub project: String,
    pub workflow_id: String,
    /// `workflow.replan` only: inputs for the fresh plan.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// Every protocol type with a published JSON Schema, by file stem.
pub fn schemas() -> Vec<(&'static str, Value)> {
    fn s<T: JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes")
    }
    vec![
        ("request", s::<Request>()),
        ("response", s::<Response>()),
        ("error", s::<ErrorBody>()),
        ("event", s::<Event>()),
        ("hello.params", s::<HelloParams>()),
        ("hello.result", s::<HelloResult>()),
        ("project.params", s::<ProjectParams>()),
        ("project.scan.result", s::<ScanSummary>()),
        ("capabilities.list.params", s::<CapabilitiesListParams>()),
        ("capabilities.list.result", s::<CapabilitiesResult>()),
        ("intent.plan.params", s::<PlanParams>()),
        ("intent.plan.result", s::<PlanResult>()),
        ("file.read.params", s::<FileReadParams>()),
        ("file.read.result", s::<FileReadResult>()),
        ("plan.edit.params", s::<PlanEditParams>()),
        ("plan.get.params", s::<PlanIdParams>()),
        ("plan.get.result", s::<PlanView>()),
        ("plan.approve.params", s::<ApproveParams>()),
        ("plan.approve.result", s::<ApproveResult>()),
        ("plan.reject.params", s::<RejectParams>()),
        ("plan.reject.result", s::<RejectResult>()),
        ("execution.start.params", s::<StartParams>()),
        ("execution.start.result", s::<StartResult>()),
        ("execution.get.params", s::<ExecutionIdParams>()),
        ("execution.get.result", s::<ExecutionSnapshot>()),
        ("execution.events.params", s::<EventsParams>()),
        ("execution.events.result", s::<EventsResult>()),
        ("execution.cancel.result", s::<CancelResult>()),
        ("task.list.params", s::<TaskListParams>()),
        ("task.get.params", s::<TaskGetParams>()),
        ("document.result", s::<DocumentResult>()),
        ("evidence.read.params", s::<EvidenceParams>()),
        ("output.read.params", s::<OutputParams>()),
        ("content.result", s::<ContentResult>()),
        ("history.search.params", s::<HistorySearchParams>()),
        ("workflow.params", s::<WorkflowParams>()),
        ("devtask.create.params", s::<devtask::CreateTaskParams>()),
        ("devtask.id.params", s::<devtask::TaskIdParams>()),
        ("devtask.approve.params", s::<devtask::ApproveStepParams>()),
        ("devtask.reject.params", s::<devtask::RejectStepParams>()),
        ("devtask.list.params", s::<devtask::TaskListParams>()),
        ("devtask.events.params", s::<devtask::TaskEventsParams>()),
        ("devtask.view", s::<devtask::TaskView>()),
        ("devtask.report", s::<devtask::TaskReport>()),
        ("devtask.list.result", s::<devtask::TaskListResult>()),
        ("devtask.events.result", s::<devtask::TaskEventsResult>()),
        ("devtask.event", s::<devtask::DevTaskEvent>()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_listed_once() {
        let mut v: Vec<&str> = ops::ALL.to_vec();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), ops::ALL.len());
    }

    #[test]
    fn events_round_trip_with_a_flat_tagged_shape() {
        let e = Event {
            schema_version: EVENT_SCHEMA.into(),
            execution_id: "run_1".into(),
            seq: 3,
            at: "2026-01-01T00:00:00.000Z".into(),
            data: EventData::Output {
                index: 0,
                stream: Stream::Stderr,
                text: "x".into(),
                lossy: false,
                offset: 0,
                bytes: 1,
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "output");
        assert_eq!(v["stream"], "stderr");
        assert_eq!(v["seq"], 3);
        assert_eq!(serde_json::from_value::<Event>(v).unwrap(), e);
    }

    #[test]
    fn unknown_params_are_rejected() {
        assert!(serde_json::from_value::<ApproveParams>(
            serde_json::json!({"plan_id": "p", "plan_digest": "d", "yes": true})
        )
        .is_err());
        assert!(serde_json::from_value::<StartParams>(
            serde_json::json!({"plan_id": "p", "command": "rm -rf /"})
        )
        .is_err());
    }
}

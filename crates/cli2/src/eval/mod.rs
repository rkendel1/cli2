//! The evaluation harness (`cli2 eval run`).
//!
//! It drives the same `Engine` the CLI uses, in isolated sandboxes: a fresh
//! data directory, a hermetic `PATH` (symlinks to a short list of host tools
//! plus fixture executables), and a throwaway copy of a fixture project (with
//! its own git repository when a case needs one). It never touches the
//! developer's repository, home directory or installed tools beyond those
//! symlinks, and never retries or alters a plan after a failure.
//!
//! A case is *not* successful because it produced a plausible plan or exited
//! zero. Each stage is checked against the case's expectations, the earliest
//! failing stage is recorded with a concise diagnostic, and the evidence
//! (journal, blobs, index) is retained when an output directory is given.

pub mod matching;
mod run;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const REPORT_SCHEMA: &str = "cli2.eval_report.v1";

// ---------------------------------------------------------------- the suite

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub category: String,
    /// The natural-language request.
    pub request: String,
    /// Paraphrases to compare fresh planning against history reuse and the model.
    #[serde(default)]
    pub variants: Vec<String>,
    /// Required project fixture (a directory under `fixtures/`).
    pub fixture: String,
    /// Fixture executables (from `fixtures/tools/`) put on the sandbox PATH.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Host executables symlinked into the sandbox PATH; the case is skipped if one is missing.
    #[serde(default)]
    pub host_tools: Vec<String>,
    /// Tools that must exist on the host for the case to run at all.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Executables whose `--help` the user has consented to capture before the request.
    #[serde(default)]
    pub capture_help: Vec<String>,
    /// Initialize a git repository in the sandbox project and commit it.
    #[serde(default)]
    pub git: bool,
    /// Edits applied after the initial commit (a dirty working tree).
    #[serde(default)]
    pub edits: Vec<Edit>,
    /// Expected capability, or any of an acceptable set.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub plan: PlanExpect,
    /// Whether the harness may execute the plan (with approval).
    #[serde(default)]
    pub execute: bool,
    /// Request expectations (`--expect-stdout TEXT`, `--expect-file PATH`).
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub expect: Expect,
    /// Path prefixes execution may create or change. Planning may change nothing.
    #[serde(default)]
    pub side_effects_allowed: Vec<String>,
    #[serde(default)]
    pub history: Option<HistoryCase>,
    /// A documented limitation: the case is expected to fail until it is addressed.
    #[serde(default)]
    pub known_gap: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanExpect {
    pub argv: Option<Vec<String>>,
    /// none | confirm | explicit | forbidden
    pub approval: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// succeeded | failed | not_run
    pub process: Option<String>,
    pub exit_code: Option<i32>,
    /// verified | unverified | failed_verification | process_failed | not_run
    pub task: Option<String>,
    /// Text that must appear in the captured stdout or stderr.
    #[serde(default)]
    pub output_contains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub path: String,
    pub append: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCase {
    /// Changes applied to the project after the prior execution is recorded.
    #[serde(default)]
    pub mutate: Vec<HistoryMutation>,
    /// Ask from a second copy of the project (changed context).
    #[serde(default)]
    pub query_in: Option<String>,
    pub expect_retrieval: bool,
    /// validated | stale
    pub expect_validity: String,
    /// `blocked`: the fresh plan must not be buildable (default: it must be).
    #[serde(default)]
    pub expect_replan: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryMutation {
    pub kind: String,
    pub path: String,
    pub append: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

// ---------------------------------------------------------------- the report

/// The earliest stage at which something went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Discovery,
    Resolution,
    Interface,
    Planning,
    Authority,
    Execution,
    Verification,
    History,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub stage: Stage,
    /// A concise diagnostic.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Approach {
    /// Fresh deterministic planning.
    FreshDeterministic,
    /// Historical workflow retrieval followed by fresh replanning.
    HistoryReplan,
    /// Fresh planning with Apple Foundation Models ranking (real, on-device).
    Model,
}

/// What happened for one request under one approach. Booleans are `None`
/// when the question does not apply, so denominators stay honest.
#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub approach: Approach,
    pub query: String,
    pub failure: Option<Failure>,
    pub discovery: Option<bool>,
    pub selection: Option<bool>,
    pub plan_valid: Option<bool>,
    pub argument: Option<bool>,
    pub approval: Option<bool>,
    pub execution_completed: Option<bool>,
    pub process_exit_zero: Option<bool>,
    pub process_as_expected: Option<bool>,
    pub verification: Option<bool>,
    pub retrieval: Option<bool>,
    pub stale_handling: Option<bool>,
    pub selected: Option<String>,
    pub plan_status: String,
    pub argv: Option<Vec<String>>,
    pub approval_level: Option<String>,
    pub exit_code: Option<i32>,
    pub verification_status: Option<String>,
    pub validity: Option<String>,
    pub invalid_capability_refs: u32,
    pub invalid_argument_refs: u32,
    pub unintended_side_effects: Vec<String>,
    pub unauthorized_executions: u32,
    pub executed: bool,
    pub request_ids: Vec<String>,
    pub output_ids: Vec<String>,
    pub notes: Vec<String>,
    pub latency_ms: u64,
}

impl Attempt {
    pub fn new(approach: Approach, query: &str) -> Self {
        Self {
            approach,
            query: query.to_string(),
            failure: None,
            discovery: None,
            selection: None,
            plan_valid: None,
            argument: None,
            approval: None,
            execution_completed: None,
            process_exit_zero: None,
            process_as_expected: None,
            verification: None,
            retrieval: None,
            stale_handling: None,
            selected: None,
            plan_status: "not_attempted".into(),
            argv: None,
            approval_level: None,
            exit_code: None,
            verification_status: None,
            validity: None,
            invalid_capability_refs: 0,
            invalid_argument_refs: 0,
            unintended_side_effects: vec![],
            unauthorized_executions: 0,
            executed: false,
            request_ids: vec![],
            output_ids: vec![],
            notes: vec![],
            latency_ms: 0,
        }
    }

    pub fn ok(&self) -> bool {
        self.failure.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub query: String,
    pub fresh_deterministic: Attempt,
    pub history_replan: Attempt,
    pub model: Option<Attempt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub category: String,
    pub request: String,
    pub skipped: Option<String>,
    /// All expectations held (a plausible plan or exit code 0 is not enough).
    pub passed: bool,
    pub known_gap: Option<String>,
    pub first_failure: Option<Failure>,
    pub attempt: Option<Attempt>,
    pub comparisons: Vec<Comparison>,
    /// Where the retained evidence (journal, blobs, index) for this case is.
    pub evidence_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rate {
    pub passed: usize,
    pub of: usize,
}

impl Rate {
    fn from(items: impl Iterator<Item = Option<bool>>) -> Self {
        let mut r = Rate { passed: 0, of: 0 };
        for i in items.flatten() {
            r.of += 1;
            r.passed += i as usize;
        }
        r
    }
}

/// Separate metrics, each with its own denominator. There is deliberately no
/// combined score.
#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub capability_discovery: Rate,
    pub correct_capability_selection: Rate,
    pub plan_validity: Rate,
    pub argument_correctness: Rate,
    pub approval_behavior: Rate,
    pub execution_completion: Rate,
    /// The process exited 0 (of executed attempts).
    pub process_success: Rate,
    /// The process outcome matched the case's expectation (including expected failures).
    pub process_outcome_as_expected: Rate,
    pub task_verification: Rate,
    pub workflow_retrieval: Rate,
    pub stale_workflow_handling: Rate,
    pub invalid_capability_references: Count,
    pub invalid_argument_references: Count,
    pub unintended_side_effects: Count,
    pub unauthorized_executions: Count,
}

#[derive(Debug, Clone, Serialize)]
pub struct Count {
    pub count: u32,
    /// Plans (or attempts) examined.
    pub of: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApproachSummary {
    pub pairs: usize,
    pub selection_correct: Rate,
    pub plan_valid: Rate,
    pub task_verified: Rate,
    pub invalid_plans: u32,
    pub unauthorized_executions: u32,
    pub incorrect_verification: u32,
    pub unintended_side_effects: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparisonSummary {
    pub fresh_deterministic: ApproachSummary,
    pub history_replan: ApproachSummary,
    /// Real on-device model, a single non-deterministic run per pair; `None` if not run.
    pub model: Option<ApproachSummary>,
    pub model_status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub schema_version: &'static str,
    pub suite: String,
    pub cli2_version: &'static str,
    pub generated_at: String,
    pub host: BTreeMap<String, String>,
    pub cases_total: usize,
    pub cases_run: usize,
    pub cases_skipped: usize,
    pub cases_passed: usize,
    pub cases_failed: usize,
    pub metrics: Metrics,
    pub comparison: ComparisonSummary,
    /// Earliest failing stage → number of cases.
    pub failure_categories: BTreeMap<String, usize>,
    pub cases: Vec<CaseResult>,
    pub matching: Option<matching::MatchingReport>,
    /// Hash of every outcome that should be reproducible (excludes timing, IDs and paths).
    pub results_fingerprint: String,
}

pub use run::{run_suite, SuiteOptions};

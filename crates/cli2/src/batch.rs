//! Structured batches: an ordered list of ordinary requests.
//!
//! A batch is not a shell language. Each step is a `cli2.request.v1` object
//! that goes through the same resolver, planner, authority policy, approval,
//! execution, verification and evidence path as `cli2 "…"`. There is no
//! substitution, piping, redirection or templating, and a step cannot approve
//! itself (the file format has no approval field).
//!
//! Semantics:
//! * Steps run in order.
//! * `on_failure: "stop"` (default): after the first step whose status is not
//!   `succeeded`, remaining steps are `not_run`. `"continue"`: later steps still
//!   run, except those that `depends_on` a step that did not succeed
//!   (`skipped_dependency`). An interrupt always stops the batch.
//! * Approval is per step, exactly as for an ordinary request: interactively
//!   each step prompts; `--yes` grants `confirm`-level steps only; a step that
//!   needs approval it does not get is `awaiting_approval`, with its plan
//!   recorded so `cli2 exec PLAN_ID --approve DIGEST` can run it later.
//! * Steps refer to earlier steps only through `depends_on` (a gate on success)
//!   and results refer to steps by `id` and output ID. Output is never
//!   substituted into later requests.

use std::collections::BTreeSet;

use cli2_core::{ExecutionResult, RequestInput, Status};
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, EngineError, Interaction, RunOptions};
use crate::output::{started_steps, OutputRecord};

pub const BATCH_SCHEMA: &str = "cli2.batch.v1";
pub const BATCH_RESULT_SCHEMA: &str = "cli2.batch_result.v1";
pub const MAX_STEPS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Stop,
    Continue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchStep {
    pub id: String,
    pub request: RequestInput,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchFile {
    pub schema_version: String,
    #[serde(default)]
    pub on_failure: OnFailure,
    pub steps: Vec<BatchStep>,
}

impl BatchFile {
    pub fn parse(text: &str) -> Result<Self, EngineError> {
        let file: BatchFile = serde_json::from_str(text).map_err(|e| {
            EngineError::Invalid(format!(
                "batch file is not a valid {BATCH_SCHEMA} document: {e}"
            ))
        })?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        let bad = |m: String| Err(EngineError::Invalid(format!("batch file: {m}")));
        if self.schema_version != BATCH_SCHEMA {
            return bad(format!(
                "schema_version must be {BATCH_SCHEMA:?}, got {:?}",
                self.schema_version
            ));
        }
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return bad(format!(
                "a batch needs 1 to {MAX_STEPS} steps, got {}",
                self.steps.len()
            ));
        }
        let mut seen = BTreeSet::new();
        for s in &self.steps {
            let ok = !s.id.is_empty()
                && s.id.len() <= 40
                && s.id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !ok {
                return bad(format!(
                    "step id {:?} must be 1-40 characters of [A-Za-z0-9_-]",
                    s.id
                ));
            }
            for d in &s.depends_on {
                if !seen.contains(d.as_str()) {
                    return bad(format!(
                        "step {:?} depends on {d:?}, which is not an earlier step",
                        s.id
                    ));
                }
            }
            if !seen.insert(s.id.as_str()) {
                return bad(format!("duplicate step id {:?}", s.id));
            }
            s.request
                .validate()
                .map_err(|e| EngineError::Invalid(format!("batch file: step {:?}: {e}", s.id)))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// The request was resolved and handled (its `status` says how it ended).
    Ran,
    /// A step it depends on did not succeed.
    SkippedDependency,
    /// An earlier failure stopped the batch.
    NotRun,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub id: String,
    pub state: StepState,
    /// The request's status, when it ran.
    pub status: Option<Status>,
    pub request_id: Option<String>,
    pub plan_id: Option<String>,
    /// One per process that actually started.
    pub output_ids: Vec<String>,
    pub message: String,
    /// Plan digest to approve with `cli2 exec PLAN_ID --approve DIGEST`.
    pub approval_digest: Option<String>,
    /// Captured results, inline (bounded; see `truncated` and `inline_truncated`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchResult {
    pub schema_version: &'static str,
    pub batch_id: String,
    pub status: Status,
    pub on_failure: OnFailure,
    pub started_at: String,
    pub finished_at: String,
    pub steps: Vec<StepOutcome>,
    pub summary: String,
}

/// The batch's status.
///
/// 1. any interrupted step → `cancelled`
/// 2. every step succeeded → `succeeded`
/// 3. no process ran at all → the first blocking step's status
///    (`awaiting_approval`, `needs_clarification`, `rejected`, …)
/// 4. no step succeeded → `failed`
/// 5. otherwise → `partially_succeeded`
pub fn overall_status(steps: &[StepOutcome]) -> Status {
    if steps.iter().any(|s| s.status == Some(Status::Cancelled)) {
        return Status::Cancelled;
    }
    let ok = |s: &StepOutcome| s.status == Some(Status::Succeeded);
    if steps.iter().all(ok) {
        return Status::Succeeded;
    }
    let executed = steps.iter().any(|s| !s.output_ids.is_empty());
    if !executed {
        if let Some(Status::AwaitingApproval | Status::NeedsClarification | Status::Rejected) =
            steps
                .iter()
                .find(|s| !ok(s) && s.status.is_some())
                .and_then(|s| s.status)
        {
            return steps
                .iter()
                .find(|s| !ok(s) && s.status.is_some())
                .and_then(|s| s.status)
                .unwrap();
        }
    }
    if !steps.iter().any(ok) {
        return Status::Failed;
    }
    Status::PartiallySucceeded
}

pub fn run_batch(
    engine: &mut Engine,
    file: &BatchFile,
    opts: &RunOptions,
    ui: &mut dyn Interaction,
) -> Result<BatchResult, EngineError> {
    let batch_id = cli2_core::new_id("batch");
    let started_at = cli2_core::now_rfc3339();
    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut stopped = false;
    for step in &file.steps {
        let blocked_by = step.depends_on.iter().find(|d| {
            !outcomes
                .iter()
                .any(|o| &o.id == *d && o.status == Some(Status::Succeeded))
        });
        let skeleton = |state, message: String| StepOutcome {
            id: step.id.clone(),
            state,
            status: None,
            request_id: None,
            plan_id: None,
            output_ids: vec![],
            message,
            approval_digest: None,
            outputs: vec![],
        };
        if stopped {
            outcomes.push(skeleton(
                StepState::NotRun,
                "not run: an earlier step did not succeed and on_failure is \"stop\"".into(),
            ));
            continue;
        }
        if let Some(d) = blocked_by {
            outcomes.push(skeleton(
                StepState::SkippedDependency,
                format!("skipped: it depends on {d:?}, which did not succeed"),
            ));
            continue;
        }
        let result: ExecutionResult = engine.run(step.request.clone(), opts, ui)?;
        let seq = engine.result_seq(&result.request_id)?;
        let mut o = skeleton(StepState::Ran, result.message.clone());
        o.status = Some(result.status);
        o.request_id = Some(result.request_id.clone());
        o.plan_id = result.plan_id.clone();
        o.approval_digest = (result.status == Status::AwaitingApproval)
            .then(|| result.approval.as_ref().map(|a| a.plan_digest.clone()))
            .flatten();
        if let Some(seq) = seq {
            for s in started_steps(&result) {
                let id = crate::output::OutputId { seq, step: s.index }.format();
                o.outputs.push(engine.output_record(&id, true)?);
                o.output_ids.push(id);
            }
        }
        let interrupted = result.status == Status::Cancelled;
        let failed = result.status != Status::Succeeded;
        outcomes.push(o);
        if interrupted || (failed && file.on_failure == OnFailure::Stop) {
            stopped = true;
        }
    }
    let status = overall_status(&outcomes);
    let count = |st: Status| outcomes.iter().filter(|o| o.status == Some(st)).count();
    let summary = format!(
        "{} of {} step(s) succeeded; {} did not; {} not run",
        count(Status::Succeeded),
        outcomes.len(),
        outcomes
            .iter()
            .filter(|o| o.state == StepState::Ran && o.status != Some(Status::Succeeded))
            .count(),
        outcomes
            .iter()
            .filter(|o| o.state != StepState::Ran)
            .count(),
    );
    let result = BatchResult {
        schema_version: BATCH_RESULT_SCHEMA,
        batch_id,
        status,
        on_failure: file.on_failure,
        started_at,
        finished_at: cli2_core::now_rfc3339(),
        steps: outcomes,
        summary,
    };
    // The journal keeps the compact form (IDs, not inline text).
    let mut compact = serde_json::to_value(&result).expect("serializable");
    if let Some(steps) = compact.get_mut("steps").and_then(|s| s.as_array_mut()) {
        for s in steps {
            if let Some(o) = s.as_object_mut() {
                o.remove("outputs");
            }
        }
    }
    engine.record_batch(&result.batch_id, &compact)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(status: Option<Status>, outputs: usize) -> StepOutcome {
        StepOutcome {
            id: "s".into(),
            state: if status.is_some() {
                StepState::Ran
            } else {
                StepState::NotRun
            },
            status,
            request_id: None,
            plan_id: None,
            output_ids: (0..outputs).map(|i| format!("out_{i}")).collect(),
            message: String::new(),
            approval_digest: None,
            outputs: vec![],
        }
    }

    #[test]
    fn overall_status_rules() {
        use Status::*;
        assert_eq!(
            overall_status(&[step(Some(Succeeded), 1), step(Some(Succeeded), 1)]),
            Succeeded
        );
        assert_eq!(
            overall_status(&[step(Some(Succeeded), 1), step(Some(Failed), 1)]),
            PartiallySucceeded
        );
        assert_eq!(
            overall_status(&[
                step(Some(Succeeded), 1),
                step(Some(VerificationFailed), 1),
                step(None, 0)
            ]),
            PartiallySucceeded
        );
        assert_eq!(
            overall_status(&[step(Some(Failed), 1), step(None, 0)]),
            Failed
        );
        assert_eq!(
            overall_status(&[step(Some(AwaitingApproval), 0), step(None, 0)]),
            AwaitingApproval
        );
        assert_eq!(
            overall_status(&[step(Some(NeedsClarification), 0)]),
            NeedsClarification
        );
        assert_eq!(
            overall_status(&[step(Some(Succeeded), 1), step(Some(Cancelled), 1)]),
            Cancelled
        );
        assert_eq!(
            overall_status(&[step(Some(Succeeded), 1), step(Some(AwaitingApproval), 0)]),
            PartiallySucceeded
        );
    }

    #[test]
    fn file_validation() {
        let ok = r#"{"schema_version":"cli2.batch.v1","steps":[
            {"id":"a","request":{"schema_version":"cli2.request.v1","intent":"x"}},
            {"id":"b","depends_on":["a"],"request":{"schema_version":"cli2.request.v1","intent":"y"}}]}"#;
        assert!(BatchFile::parse(ok).is_ok());
        for bad in [
            r#"{"schema_version":"cli2.batch.v2","steps":[]}"#,
            r#"{"schema_version":"cli2.batch.v1","steps":[]}"#,
            // forward / unknown dependency, duplicate id, bad id
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a","depends_on":["b"],"request":{"schema_version":"cli2.request.v1","intent":"x"}}]}"#,
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a","request":{"schema_version":"cli2.request.v1","intent":"x"}},{"id":"a","request":{"schema_version":"cli2.request.v1","intent":"x"}}]}"#,
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a b","request":{"schema_version":"cli2.request.v1","intent":"x"}}]}"#,
            // no way to approve inside the file, and no shell strings
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a","yes":true,"request":{"schema_version":"cli2.request.v1","intent":"x"}}]}"#,
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a","command":"rm -rf /"}]}"#,
            r#"{"schema_version":"cli2.batch.v1","steps":[{"id":"a","request":{"schema_version":"cli2.request.v1","intent":""}}]}"#,
        ] {
            assert!(BatchFile::parse(bad).is_err(), "{bad}");
        }
    }
}

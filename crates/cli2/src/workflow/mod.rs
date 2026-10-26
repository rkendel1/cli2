//! Capability learning and reuse.
//!
//! A *workflow* is a derived, read-only view of something that already
//! happened, built from the authoritative journal (requests, plans, results,
//! approvals, batch records, interface provenance). The index that holds
//! workflows ([`store`]) is a separate, disposable file: delete it and it is
//! rebuilt from the journal, byte for byte the same.
//!
//! Learning is *evidence-backed retrieval*, never authority:
//!
//! * a workflow is **eligible** only if every step was executed and its
//!   task-level verification passed (a zero exit status alone is "process
//!   succeeded", not "task verified"; missing verification is *unknown*);
//! * its **validity** is judged live against the current environment
//!   ([`validate`]): `validated`, `candidate` (nothing changed, but something
//!   could not be confirmed), `stale` (an assumption changed), or `ineligible`;
//! * reuse means building a **fresh plan** (`workflow replay`): the recorded
//!   argv is shown for comparison and never executed; current discovery,
//!   interface knowledge, authority policy, approval and verification apply.
//!   Historical approval is never reused.

pub mod engine_ext;
pub mod search;
pub mod store;
pub mod validate;

use std::collections::BTreeMap;

use cli2_core::{
    ApprovalLevel, ApprovalMethod, Check, Constraints, ExecutionResult, Status, StepResult,
    Termination, VerificationStatus,
};
use cli2_evidence::Journal;
use serde::{Deserialize, Serialize};

use crate::engine::EngineError;
use crate::output::{started_steps, OutputId};

pub const WORKFLOW_SCHEMA: &str = "cli2.workflow.v1";
/// Bump when the derivation rules change; indexes built by another version are rebuilt.
pub const BUILD_VERSION: u32 = 1;

/// The eligibility rule, as shown to users.
pub const ELIGIBILITY_RULE: &str = "Eligible for retrieval only if every step ran and its task-level verification passed. A zero exit status alone is \"process succeeded\"; missing verification is \"unknown\", never success.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The process(es) exited 0 and a task-level check passed.
    TaskVerified,
    /// The process(es) exited 0; the task was not verified (unknown).
    ProcessSucceeded,
    ProcessFailed,
    /// Processes ran but a task-level check failed.
    TaskFailedVerification,
    TimedOut,
    Interrupted,
    /// A plan was made and needs approval; nothing ran.
    ApprovalRequired,
    /// Approval was declined; nothing ran.
    ApprovalDenied,
    /// Nothing was executed (planned only, rejected, or needed clarification).
    NotPerformed,
}

impl Outcome {
    pub fn executed(self) -> bool {
        !matches!(
            self,
            Self::ApprovalRequired | Self::ApprovalDenied | Self::NotPerformed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Execution,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceAtExecution {
    pub completeness: String,
    pub captured_at: Option<String>,
    pub usage_line: Option<String>,
    pub parser_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedApproval {
    pub required: ApprovalLevel,
    pub granted: bool,
    pub method: Option<ApprovalMethod>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    /// `"0"` for a single execution, the batch step id otherwise.
    pub step_id: String,
    pub intent: String,
    pub constraints: Constraints,
    pub bindings: BTreeMap<String, String>,
    pub expectations: Vec<Check>,
    pub selection: Option<String>,
    pub depends_on: Vec<String>,
    pub request_id: String,
    pub plan_id: Option<String>,
    pub capability_id: Option<String>,
    /// Identity of the capability when it ran (executable file / task definition).
    pub capability_fingerprint: Option<String>,
    /// What actually ran, for comparison. **Never replayed.**
    pub recorded_argv: Vec<String>,
    pub working_directory: String,
    pub execution_id: Option<String>,
    pub output_ids: Vec<String>,
    pub outcome: Outcome,
    pub termination: Option<Termination>,
    pub exit_code: Option<i32>,
    pub verification: VerificationStatus,
    pub recorded_approval: Option<RecordedApproval>,
    /// The approval level the plan required when it was made.
    pub recorded_requirement: Option<ApprovalLevel>,
    pub interface: Option<InterfaceAtExecution>,
    /// Output blobs that support the recorded outcome.
    pub evidence_refs: Vec<String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Eligibility {
    pub eligible: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub schema_version: String,
    pub id: String,
    pub kind: Kind,
    /// The request (or, for a batch, the step intents in order).
    pub intent: String,
    pub recorded_at: String,
    pub outcome: Outcome,
    pub eligibility: Eligibility,
    /// For a batch: the batch ID and failure policy.
    pub batch_id: Option<String>,
    pub on_failure: Option<String>,
    /// For an execution that was a step of a batch: that batch's workflow ID.
    pub part_of_batch: Option<String>,
    pub steps: Vec<StepRecord>,
}

impl Workflow {
    pub fn working_directory(&self) -> &str {
        self.steps
            .first()
            .map(|s| s.working_directory.as_str())
            .unwrap_or("")
    }
}

/// Classify one recorded result. Process success and task verification are
/// different things and stay different.
pub fn classify(r: &ExecutionResult) -> Outcome {
    let started = started_steps(r).next().is_some();
    match r.status {
        Status::Succeeded if r.verification.status == VerificationStatus::Passed => {
            Outcome::TaskVerified
        }
        Status::Succeeded => Outcome::ProcessSucceeded,
        Status::VerificationFailed => Outcome::TaskFailedVerification,
        Status::Failed | Status::PartiallySucceeded => {
            if started_steps(r).any(|s| s.termination == Termination::TimedOut) {
                Outcome::TimedOut
            } else {
                Outcome::ProcessFailed
            }
        }
        Status::Cancelled if started => Outcome::Interrupted,
        Status::Cancelled => Outcome::ApprovalDenied,
        Status::AwaitingApproval => Outcome::ApprovalRequired,
        Status::Rejected | Status::NeedsClarification | Status::Planned => Outcome::NotPerformed,
    }
}

fn combine(steps: &[StepRecord], batch_status: &str) -> Outcome {
    use Outcome::*;
    let any = |o: Outcome| steps.iter().any(|s| s.outcome == o);
    for o in [
        Interrupted,
        TimedOut,
        TaskFailedVerification,
        ProcessFailed,
        ApprovalDenied,
        ApprovalRequired,
    ] {
        if any(o) {
            return o;
        }
    }
    if steps.iter().all(|s| s.outcome == TaskVerified) && batch_status == "succeeded" {
        TaskVerified
    } else if steps
        .iter()
        .all(|s| matches!(s.outcome, TaskVerified | ProcessSucceeded))
        && batch_status == "succeeded"
    {
        ProcessSucceeded
    } else if steps.iter().any(|s| s.outcome.executed()) {
        ProcessFailed
    } else {
        NotPerformed
    }
}

fn eligibility(steps: &[StepRecord], outcome: Outcome, batch_status: Option<&str>) -> Eligibility {
    let mut reasons = Vec::new();
    for s in steps {
        let label = |m: &str| {
            if steps.len() > 1 {
                format!("step {:?}: {m}", s.step_id)
            } else {
                m.to_string()
            }
        };
        match s.outcome {
            Outcome::TaskVerified => {}
            Outcome::ProcessSucceeded => reasons.push(label(
                "the process succeeded but the task was not verified, so success is unknown",
            )),
            Outcome::ProcessFailed => reasons.push(label("the process failed")),
            Outcome::TaskFailedVerification => reasons.push(label("task verification failed")),
            Outcome::TimedOut => reasons.push(label("the process timed out")),
            Outcome::Interrupted => reasons.push(label("the execution was interrupted")),
            Outcome::ApprovalRequired => {
                reasons.push(label("it needed approval and was never executed"))
            }
            Outcome::ApprovalDenied => reasons.push(label("approval was declined; it never ran")),
            Outcome::NotPerformed => reasons.push(label("it was never executed")),
        }
    }
    if let Some(b) = batch_status {
        if b != "succeeded" && reasons.is_empty() {
            reasons.push(format!("the batch ended {b}"));
        }
    }
    let eligible = outcome == Outcome::TaskVerified && reasons.is_empty();
    Eligibility { eligible, reasons }
}

struct Provenance {
    interface: InterfaceAtExecution,
}

fn provenance_map(journal: &Journal) -> Result<BTreeMap<String, Provenance>, EngineError> {
    let mut map = BTreeMap::new();
    for (_, request_id, data) in journal.events_of_kind("interface_provenance")? {
        let cap = &data["capture"];
        map.insert(
            request_id,
            Provenance {
                interface: InterfaceAtExecution {
                    completeness: data["interface_completeness"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string(),
                    captured_at: cap["captured_at"].as_str().map(String::from),
                    usage_line: cap["usage_line"].as_str().map(String::from),
                    parser_version: cap["parser_version"].as_u64().map(|v| v as u32),
                },
            },
        );
    }
    Ok(map)
}

fn step_record(
    journal: &Journal,
    seq: i64,
    r: &ExecutionResult,
    prov: &BTreeMap<String, Provenance>,
    step_id: &str,
    depends_on: Vec<String>,
) -> Option<StepRecord> {
    let request = journal.request(&r.request_id).ok()?;
    let plan = r.plan_id.as_deref().and_then(|p| journal.plan(p).ok());
    let started: Vec<&StepResult> = started_steps(r).collect();
    // The step that supports the outcome: the last one that ran.
    let ran = started.last().copied();
    let planned = plan.as_ref().and_then(|p| p.steps.first());
    let mut argv = Vec::new();
    if let Some(s) = ran {
        argv.push(
            s.executable
                .rsplit('/')
                .next()
                .unwrap_or(&s.executable)
                .to_string(),
        );
        argv.extend(s.arguments.iter().cloned());
    } else if let Some(p) = planned {
        argv = p.argv();
    }
    let mut evidence = Vec::new();
    for s in &started {
        evidence.extend(s.stdout_ref.iter().chain(s.stderr_ref.iter()).cloned());
    }
    Some(StepRecord {
        step_id: step_id.to_string(),
        intent: request.intent.clone(),
        constraints: request.constraints.clone(),
        bindings: request.bindings.clone(),
        expectations: request.expectations.clone(),
        selection: request.selection.clone(),
        depends_on,
        request_id: r.request_id.clone(),
        plan_id: r.plan_id.clone(),
        capability_id: ran
            .map(|s| s.capability_id.clone())
            .or_else(|| planned.map(|p| p.capability_id.clone())),
        capability_fingerprint: planned.map(|p| p.capability_fingerprint.clone()),
        recorded_argv: argv,
        working_directory: request.working_directory.clone(),
        execution_id: (!started.is_empty()).then(|| format!("exec_{seq}")),
        output_ids: started
            .iter()
            .map(|s| OutputId { seq, step: s.index }.format())
            .collect(),
        outcome: classify(r),
        termination: ran.map(|s| s.termination),
        exit_code: ran.and_then(|s| s.exit_code),
        verification: r.verification.status,
        recorded_approval: r.approval.as_ref().map(|a| RecordedApproval {
            required: a.required,
            granted: a.granted,
            method: a.method,
        }),
        recorded_requirement: plan.as_ref().map(|p| p.approval.level),
        interface: prov.get(&r.request_id).map(|p| p.interface.clone()),
        evidence_refs: evidence,
        recorded_at: ran
            .map(|s| s.finished_at.clone())
            .unwrap_or_else(|| request.received_at.clone()),
    })
}

/// Derive every workflow from the journal. Pure function of the journal's
/// contents: the same history always yields the same list, in the same order.
pub fn derive(journal: &Journal) -> Result<Vec<Workflow>, EngineError> {
    let results = journal.all_results()?;
    let prov = provenance_map(journal)?;
    let by_seq: BTreeMap<i64, &ExecutionResult> = results.iter().map(|(s, r)| (*s, r)).collect();
    let mut latest_for_request: BTreeMap<&str, i64> = BTreeMap::new();
    for (seq, r) in &results {
        latest_for_request.insert(r.request_id.as_str(), *seq);
    }

    // Batches first, so executions that belonged to one can be marked.
    let mut batches = Vec::new();
    let mut member_of: BTreeMap<String, String> = BTreeMap::new();
    for (_, batch_id, data) in journal.events_of_kind("batch_recorded")? {
        let hex = batch_id.strip_prefix("batch_").unwrap_or(&batch_id);
        let id = format!("wf_b{}", hex.chars().take(12).collect::<String>());
        let mut steps = Vec::new();
        for st in data["steps"].as_array().into_iter().flatten() {
            let step_id = st["id"].as_str().unwrap_or("?").to_string();
            let depends: Vec<String> = st["depends_on"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str().map(String::from))
                .collect();
            let request_id = st["request_id"].as_str();
            // Prefer the exact execution the batch recorded.
            let seq = st["output_ids"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|o| o.as_str())
                .and_then(|o| match OutputId::parse(o) {
                    Some(crate::output::ParsedId::Output(oid)) => Some(oid.seq),
                    _ => None,
                })
                .or_else(|| request_id.and_then(|r| latest_for_request.get(r).copied()));
            let rec = seq
                .and_then(|s| by_seq.get(&s).map(|r| (s, *r)))
                .and_then(|(s, r)| step_record(journal, s, r, &prov, &step_id, depends.clone()));
            match rec {
                Some(rec) => {
                    member_of.insert(rec.request_id.clone(), id.clone());
                    steps.push(rec);
                }
                None => {
                    // Skipped / not run: keep the step so the pattern is complete.
                    steps.push(StepRecord {
                        step_id,
                        intent: String::new(),
                        constraints: Constraints::default(),
                        bindings: BTreeMap::new(),
                        expectations: vec![],
                        selection: None,
                        depends_on: depends,
                        request_id: request_id.unwrap_or("").to_string(),
                        plan_id: None,
                        capability_id: None,
                        capability_fingerprint: None,
                        recorded_argv: vec![],
                        working_directory: String::new(),
                        execution_id: None,
                        output_ids: vec![],
                        outcome: Outcome::NotPerformed,
                        termination: None,
                        exit_code: None,
                        verification: VerificationStatus::NotRun,
                        recorded_approval: None,
                        recorded_requirement: None,
                        interface: None,
                        evidence_refs: vec![],
                        recorded_at: data["finished_at"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }
        let status = data["status"].as_str().unwrap_or("unknown");
        let outcome = combine(&steps, status);
        let elig = eligibility(&steps, outcome, Some(status));
        let recorded_at = data["finished_at"].as_str().unwrap_or("").to_string();
        let intent = steps
            .iter()
            .map(|s| s.intent.as_str())
            .filter(|i| !i.is_empty())
            .collect::<Vec<_>>()
            .join(" → ");
        batches.push(Workflow {
            schema_version: WORKFLOW_SCHEMA.into(),
            id,
            kind: Kind::Batch,
            intent,
            recorded_at,
            outcome,
            eligibility: elig,
            batch_id: Some(batch_id),
            on_failure: data["on_failure"].as_str().map(String::from),
            part_of_batch: None,
            steps,
        });
    }

    let mut out = batches;
    for (seq, r) in &results {
        let Some(rec) = step_record(journal, *seq, r, &prov, "0", vec![]) else {
            continue;
        };
        let outcome = rec.outcome;
        let part_of = member_of.get(&rec.request_id).cloned();
        let elig = eligibility(std::slice::from_ref(&rec), outcome, None);
        out.push(Workflow {
            schema_version: WORKFLOW_SCHEMA.into(),
            id: format!("wf_e{seq}"),
            kind: Kind::Execution,
            intent: rec.intent.clone(),
            recorded_at: rec.recorded_at.clone(),
            outcome,
            eligibility: elig,
            batch_id: None,
            on_failure: None,
            part_of_batch: part_of,
            steps: vec![rec],
        });
    }
    out.sort_by(|a, b| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

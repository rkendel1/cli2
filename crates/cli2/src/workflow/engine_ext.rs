//! Engine operations over workflows: indexing, search, validation, replanning.

use std::path::Path;

use cli2_core::{ApprovalLevel, ExecutionResult, RequestInput};
use serde::Serialize;

use super::search::{similarity, MIN_MATCH};
use super::store::{Freshness, IndexReport, Refresh, WorkflowIndex};
use super::validate::{check, ChainCheck, Env, Validity, ValidityState};
use super::{Kind, Outcome, Workflow};
use crate::engine::{Engine, EngineError, NonInteractive, RunOptions};
use crate::semantic::bridge::{
    BridgeCandidate, BridgeClient, BridgeConstraints, BridgeRequest, REQUEST_SCHEMA,
};
use crate::semantic::validate::validate_ranking;

/// Ordinary requests refresh the index for advice only while history is small.
/// An ordinary request will update an index that is at most this many
/// executions behind (or build one for a history this small). Beyond it, no
/// historical match is offered as current and the user is told why.
pub const FRESHNESS_BUDGET: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct HistoryHit {
    pub workflow: Workflow,
    pub similarity: f64,
    pub shared_terms: Vec<String>,
    pub validity: Validity,
    /// Deterministic explanation; independent of any model.
    pub explanation: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: usize,
    pub verified_only: bool,
    pub include_batch_steps: bool,
    /// `None`: explicit command (the index is made current first). `Some(n)`:
    /// an ordinary request, which only does bounded work (see the freshness contract).
    pub freshness_budget: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub schema_version: &'static str,
    pub query: String,
    pub index: IndexReport,
    /// `deterministic`, or the provider that ordered the already-found matches.
    pub ordered_by: String,
    pub model_note: Option<String>,
    pub hits: Vec<HistoryHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayStep {
    pub step_id: String,
    pub intent: String,
    /// What ran historically. Shown for comparison; never executed.
    pub recorded_argv: Vec<String>,
    pub recorded_approval: Option<super::RecordedApproval>,
    /// The fresh plan's (or the blocker's) result: `planned`, `needs_clarification`, `rejected`.
    pub result: ExecutionResult,
    pub fresh_argv: Option<Vec<String>>,
    pub argv_differs: Option<bool>,
    pub current_approval: Option<ApprovalLevel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Replay {
    pub schema_version: &'static str,
    pub workflow_id: String,
    pub validity: Validity,
    pub steps: Vec<ReplayStep>,
    pub executed: bool,
    pub note: String,
}

fn describe_outcome(w: &Workflow) -> String {
    match w.outcome {
        Outcome::TaskVerified => "completed and passed task verification".into(),
        Outcome::ProcessSucceeded => {
            "the process exited 0, but the task was not verified (success unknown)".into()
        }
        Outcome::ProcessFailed => "the process failed".into(),
        Outcome::TaskFailedVerification => "the process ran but task verification failed".into(),
        Outcome::TimedOut => "it timed out".into(),
        Outcome::Interrupted => "it was interrupted".into(),
        Outcome::ApprovalRequired => "it needed approval and was never executed".into(),
        Outcome::ApprovalDenied => "approval was declined; it never ran".into(),
        Outcome::NotPerformed => "it was never executed".into(),
    }
}

/// A deterministic account of why a workflow is suggested and what it rests on.
pub fn explain(
    w: &Workflow,
    request: &str,
    shared: &[String],
    score: f64,
    v: &Validity,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Why it matches: {request:?} resembles {:?} (similarity {score:.2}{}).",
        w.intent,
        if shared.is_empty() {
            String::new()
        } else {
            format!("; shared terms: {}", shared.join(", "))
        }
    ));
    let support: Vec<String> = w
        .steps
        .iter()
        .filter_map(|s| {
            s.execution_id
                .as_ref()
                .map(|e| format!("{e} ({})", s.output_ids.join(", ")))
        })
        .collect();
    out.push(format!(
        "Supporting execution: {} at {}; {}.",
        if support.is_empty() {
            "none (it never ran)".to_string()
        } else {
            support.join(", ")
        },
        w.recorded_at,
        describe_outcome(w)
    ));
    out.push(format!(
        "Task verification: {}.",
        match w.outcome {
            Outcome::TaskVerified => "passed (a task-level check, not just the exit status)",
            Outcome::ProcessSucceeded =>
                "unknown: no task-level check was available or it was inconclusive",
            Outcome::TaskFailedVerification => "failed",
            _ => "not established",
        }
    ));
    if !v.reasons.is_empty() {
        let label = if v.state == ValidityState::Ineligible {
            "Not eligible because"
        } else {
            "Changed since it ran"
        };
        out.push(format!(
            "Validity: {:?}. {label}: {}.",
            v.state,
            v.reasons.join("; ")
        ));
    } else {
        out.push(format!("Validity: {:?}.", v.state));
    }
    if !v.unconfirmed.is_empty() {
        out.push(format!("Could not confirm: {}.", v.unconfirmed.join("; ")));
    }
    if !v.confirmed.is_empty() {
        out.push(format!("Unchanged: {}.", v.confirmed.join("; ")));
    }
    let what: Vec<String> = w
        .steps
        .iter()
        .filter(|s| s.capability_id.is_some())
        .map(|s| {
            format!(
                "{} (it ran as `{}`)",
                s.capability_id.clone().unwrap_or_default(),
                s.recorded_argv.join(" ")
            )
        })
        .collect();
    out.push(format!(
        "If you proceed: cli2 builds a fresh plan for {} under today's discovery, interface knowledge, policy, approval and verification. Nothing runs without a new approval; the old command line is not reused.",
        what.join(" and then ")
    ));
    out
}

impl Engine {
    fn open_workflow_index(&self) -> Result<(WorkflowIndex, Option<String>), EngineError> {
        WorkflowIndex::open(&self.config.home.join("workflows.db"))
    }

    /// The current workflow list. Explicit history commands use this: the index
    /// is brought fully up to date first (appending what is new, or rebuilding
    /// if it is missing, damaged or from another derivation version).
    pub fn workflow_list(&mut self) -> Result<(Vec<Workflow>, IndexReport), EngineError> {
        self.workflow_list_with(None)
    }

    /// `budget: None` — explicit: always make the index current.
    /// `budget: Some(n)` — an ordinary request: do bounded work only. If the
    /// index is more than `n` executions behind (or must be rebuilt and history
    /// is larger than `n`), nothing is scanned and the report says `behind`.
    fn workflow_list_with(
        &mut self,
        budget: Option<usize>,
    ) -> Result<(Vec<Workflow>, IndexReport), EngineError> {
        // An ordinary request must not even create the index for a history it will not build.
        if let Some(n) = budget {
            if !self.config.home.join("workflows.db").exists() {
                let (results, _, _) = self.journal.history_position(0)?;
                if results > n as i64 {
                    let report = IndexReport {
                        freshness: Freshness::Behind {
                            executions_behind: results,
                            budget: n,
                        },
                        ..Default::default()
                    };
                    return Ok((vec![], report));
                }
            }
        }
        let (mut idx, recovered) = self.open_workflow_index()?;
        let mut report = IndexReport {
            recovered_from: recovered.clone(),
            rebuilt: recovered.is_some(),
            ..Default::default()
        };
        let st = idx.status(&self.journal)?;
        if let Some(n) = budget {
            let work = if st.initialized {
                st.results_behind
            } else {
                st.journal_results
            };
            if work > n as i64 {
                report.freshness = Freshness::Behind {
                    executions_behind: work,
                    budget: n,
                };
                return Ok((vec![], report));
            }
        }
        let refreshed = match idx.refresh(&self.journal, recovered.is_some()) {
            Ok(r) => r,
            Err(e) => {
                // A failed refresh never touches the journal; say so and offer nothing as current.
                report.freshness = Freshness::Unavailable {
                    reason: e.to_string(),
                };
                return Ok((vec![], report));
            }
        };
        report.freshness = match refreshed {
            Refresh::Current => Freshness::Current,
            Refresh::Appended { workflows } => Freshness::UpdatedIncrementally { workflows },
            Refresh::Rebuilt { workflows } => {
                report.rebuilt = true;
                Freshness::Rebuilt { workflows }
            }
        };
        match idx.list() {
            Ok(list) => {
                report.workflows = list.len();
                Ok((list, report))
            }
            Err(_) => {
                // Rows that do not parse: the file is damaged. Start over from the journal.
                drop(idx);
                let path = self.config.home.join("workflows.db");
                let aside = path.with_extension(format!("db.unusable-{}", std::process::id()));
                std::fs::rename(&path, &aside)?;
                let (mut idx, _) = self.open_workflow_index()?;
                let n = idx.rebuild(&self.journal)?;
                let list = idx.list()?;
                report.rebuilt = true;
                report.freshness = Freshness::Rebuilt { workflows: n };
                report.recovered_from = Some(aside.display().to_string());
                report.workflows = list.len();
                Ok((list, report))
            }
        }
    }

    /// Force a rebuild from the journal. Returns the report and a fingerprint
    /// of the derived contents (identical for identical history).
    pub fn workflow_reindex(&mut self) -> Result<(IndexReport, String), EngineError> {
        let (mut idx, recovered) = self.open_workflow_index()?;
        let n = idx.rebuild(&self.journal)?;
        let fp = idx.fingerprint()?;
        Ok((
            IndexReport {
                freshness: Freshness::Rebuilt { workflows: n },
                rebuilt: true,
                recovered_from: recovered,
                workflows: n,
            },
            fp,
        ))
    }

    pub fn workflow_get(&mut self, id: &str) -> Result<Workflow, EngineError> {
        self.workflow_list()?
            .0
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| EngineError::NotFound(format!("no workflow with ID {id}")))
    }

    pub fn validate_workflows(
        &mut self,
        wfs: &[&Workflow],
        cwd: &Path,
    ) -> Result<Vec<Validity>, EngineError> {
        let caps = self.inventory(cwd)?;
        let env = Env {
            caps: caps.iter().map(|c| (c.id.as_str(), c)).collect(),
            policy: &self.config.policy,
            journal: &self.journal,
        };
        let mut chain = ChainCheck::new(&self.journal);
        Ok(wfs
            .iter()
            .map(|w| check(&env, w, cwd, &mut chain))
            .collect())
    }

    /// Find workflows resembling `text`, with live validity and a
    /// deterministic explanation for each. A model, if given, may only
    /// reorder the matches already found.
    pub fn search_history(
        &mut self,
        text: &str,
        cwd: &Path,
        opts: &SearchOptions,
        model: Option<&BridgeClient>,
    ) -> Result<SearchResult, EngineError> {
        let (all, index) = self.workflow_list_with(opts.freshness_budget)?;
        let mut scored: Vec<(f64, Vec<String>, Workflow)> = Vec::new();
        for w in all {
            if w.part_of_batch.is_some() && !opts.include_batch_steps {
                continue;
            }
            if opts.verified_only && !w.eligibility.eligible {
                continue;
            }
            let texts: Vec<&str> = if w.kind == Kind::Batch {
                w.steps
                    .iter()
                    .map(|s| s.intent.as_str())
                    .chain(std::iter::once(w.intent.as_str()))
                    .collect()
            } else {
                vec![w.intent.as_str()]
            };
            let best = texts
                .iter()
                .map(|t| similarity(text, t))
                .max_by(|a, b| {
                    a.score
                        .partial_cmp(&b.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            if text.trim().is_empty() || best.score >= MIN_MATCH {
                scored.push((best.score, best.shared, w));
            }
        }
        let rank = |w: &Workflow| match w.eligibility.eligible {
            true => 0,
            false => 1,
        };
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| rank(&a.2).cmp(&rank(&b.2)))
                .then_with(|| b.2.recorded_at.cmp(&a.2.recorded_at))
                .then_with(|| a.2.id.cmp(&b.2.id))
        });
        scored.truncate(opts.limit.max(1));

        let mut ordered_by = "deterministic".to_string();
        let mut model_note = None;
        if let (Some(client), true) = (model, scored.len() > 1 && !text.trim().is_empty()) {
            match rank_with_model(
                client,
                text,
                &scored.iter().map(|(_, _, w)| w).collect::<Vec<_>>(),
            ) {
                Ok(order) => {
                    let mut by_id: std::collections::BTreeMap<
                        String,
                        (f64, Vec<String>, Workflow),
                    > = scored.drain(..).map(|t| (t.2.id.clone(), t)).collect();
                    let mut reordered = Vec::new();
                    for id in &order.ids {
                        if let Some(t) = by_id.remove(id) {
                            reordered.push(t);
                        }
                    }
                    // Anything the model left out keeps its deterministic place after.
                    let mut rest: Vec<_> = by_id.into_values().collect();
                    rest.sort_by(|a, b| {
                        b.0.partial_cmp(&a.0)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.2.id.cmp(&b.2.id))
                    });
                    reordered.extend(rest);
                    scored = reordered;
                    ordered_by = crate::semantic::PROVIDER_ID.to_string();
                    model_note = Some("The order of these already-found matches was refined by the on-device model. Validity, outcomes and explanations below are computed by cli2 from evidence, not by the model.".into());
                }
                Err(e) => {
                    model_note = Some(format!(
                        "Model ordering was not used ({e}); the deterministic order is shown."
                    ))
                }
            }
        }

        let wfs: Vec<&Workflow> = scored.iter().map(|(_, _, w)| w).collect();
        let validities = self.validate_workflows(&wfs, cwd)?;
        let hits = scored
            .into_iter()
            .zip(validities)
            .map(|((score, shared, w), validity)| HistoryHit {
                explanation: explain(&w, text, &shared, score, &validity),
                similarity: score,
                shared_terms: shared,
                validity,
                workflow: w,
            })
            .collect();
        Ok(SearchResult {
            schema_version: "cli2.history_search.v1",
            query: text.into(),
            index,
            ordered_by,
            model_note,
            hits,
        })
    }

    /// Build a *fresh* plan for each step of a workflow. Nothing is executed:
    /// each step goes through resolution, interface checks, policy and
    /// approval assessment as if the user had just asked for it, pinned to the
    /// capability the workflow used. The recorded argv is only compared.
    pub fn replay_workflow(
        &mut self,
        id: &str,
        cwd: &Path,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Result<Replay, EngineError> {
        let wf = self.workflow_get(id)?;
        let validity = self.validate_workflows(&[&wf], cwd)?.remove(0);
        let mut steps = Vec::new();
        for s in &wf.steps {
            if s.intent.is_empty() {
                continue; // never ran in the original batch
            }
            let mut req = RequestInput::new(s.intent.clone());
            req.constraints = s.constraints.clone();
            req.bindings = s.bindings.clone();
            req.bindings
                .extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
            req.expectations = s.expectations.clone();
            req.selection = s.capability_id.clone().or_else(|| s.selection.clone());
            req.context.working_directory = Some(cwd.display().to_string());
            let result = self.run(
                req,
                &RunOptions {
                    execute: false,
                    ..Default::default()
                },
                &mut NonInteractive,
            )?;
            self.journal.event(
                &result.request_id,
                "workflow_replay",
                &serde_json::json!({
                    "workflow_id": wf.id,
                    "step_id": s.step_id,
                    "validity": validity.state,
                    "validity_reasons": validity.reasons,
                    "historical": {
                        "argv": s.recorded_argv,
                        "approval": s.recorded_approval,
                        "execution_id": s.execution_id,
                    },
                    "note": "planned only; historical approval and argv were not reused",
                }),
            )?;
            let fresh_argv = result
                .plan
                .as_ref()
                .and_then(|p| p.steps.first())
                .map(|st| st.argv());
            steps.push(ReplayStep {
                step_id: s.step_id.clone(),
                intent: s.intent.clone(),
                recorded_argv: s.recorded_argv.clone(),
                recorded_approval: s.recorded_approval.clone(),
                argv_differs: fresh_argv.as_ref().map(|f| *f != s.recorded_argv),
                current_approval: result.plan.as_ref().map(|p| p.approval.level),
                fresh_argv,
                result,
            });
        }
        Ok(Replay {
            schema_version: "cli2.workflow_replay.v1",
            workflow_id: wf.id,
            validity,
            executed: false,
            note: "Fresh plans only. Nothing was executed. Review each plan, then run it with `cli2 exec PLAN_ID` (current approval rules apply; the historical approval is not reused).".into(),
            steps,
        })
    }

    /// Advice for an ordinary request: verified, still-valid workflows that
    /// resemble it. Purely informational; it never changes what is planned.
    /// Errors never fail the request. When the index cannot be established as
    /// current within the budget, the advice says so instead of guessing.
    pub fn advise_history(&mut self, intent: &str, cwd: &Path, planned: Option<&str>) -> Advice {
        let res = match self.search_history(
            intent,
            cwd,
            &SearchOptions {
                limit: 3,
                verified_only: true,
                include_batch_steps: false,
                freshness_budget: Some(FRESHNESS_BUDGET),
            },
            None,
        ) {
            Ok(r) => r,
            Err(e) => {
                return Advice {
                    freshness: Freshness::Unavailable {
                        reason: e.to_string(),
                    },
                    notes: vec![format!(
                        "History advice was skipped: {e}. Nothing in the journal was changed."
                    )],
                }
            }
        };
        let freshness = res.index.freshness.clone();
        let mut notes = Vec::new();
        match &freshness {
            Freshness::Behind { executions_behind, budget } => notes.push(format!(
                "History advice skipped: the workflow index is {executions_behind} executions behind the journal (an ordinary request updates at most {budget}). No historical match is shown because it could not be established as current; run `cli2 workflow reindex`."
            )),
            Freshness::Unavailable { reason } => notes.push(format!(
                "History advice skipped: the workflow index could not be refreshed ({reason}). The journal is unchanged; run `cli2 workflow reindex`."
            )),
            _ => {}
        }
        for h in res
            .hits
            .iter()
            .filter(|h| {
                matches!(
                    h.validity.state,
                    ValidityState::Validated | ValidityState::Candidate
                )
            })
            .take(1)
        {
            let cap = h
                .workflow
                .steps
                .first()
                .and_then(|s| s.capability_id.clone());
            let relation = match (&cap, planned) {
                (Some(c), Some(p)) if c == p => "used the same capability as this plan".to_string(),
                (Some(c), Some(_)) => format!("used a different capability ({c})"),
                _ => "may help".into(),
            };
            notes.push(format!(
                "History: workflow {} ({:?}, {:?}, verified at {}) {relation}. It does not authorize anything; inspect with `cli2 workflow show {}` or get a fresh plan with `cli2 workflow replay {}`.",
                h.workflow.id, h.workflow.intent, h.validity.state, h.workflow.recorded_at, h.workflow.id, h.workflow.id
            ));
        }
        Advice { freshness, notes }
    }
}

/// Advice for an ordinary request, with the freshness it rests on.
#[derive(Debug, Clone)]
pub struct Advice {
    pub freshness: Freshness,
    pub notes: Vec<String>,
}

struct ModelOrder {
    ids: Vec<String>,
}

/// Ask the model to order workflows that were already found. The response is
/// validated like any other: every ID must be one that was submitted, ranks
/// must be consistent. Anything else is rejected and the caller keeps its own
/// order. The model can only reorder; it cannot add, relabel or approve.
fn rank_with_model(
    client: &BridgeClient,
    request: &str,
    workflows: &[&Workflow],
) -> Result<ModelOrder, String> {
    let id = format!("history-{}", std::process::id());
    let submitted: Vec<String> = workflows
        .iter()
        .map(|w| format!("workflow:{}", w.id))
        .collect();
    let req = BridgeRequest {
        schema_version: REQUEST_SCHEMA,
        request_id: id.clone(),
        operation: "rank",
        intent: Some(request.to_string()),
        constraints: Some(BridgeConstraints {
            modify_files: None,
            network: "unknown".into(),
        }),
        candidates: workflows
            .iter()
            .map(|w| BridgeCandidate {
                id: format!("workflow:{}", w.id),
                name: w.intent.chars().take(80).collect(),
                kind: "workflow".into(),
                purpose: Some(format!("Past request: {}", w.intent)),
                interface_completeness: "n/a".into(),
                effects: "n/a".into(),
                help_excerpt: None,
            })
            .collect(),
    };
    let resp = client.call(&req).map_err(|e| e.to_string())?;
    let v = validate_ranking(&resp, &id, &submitted).map_err(|e| e.to_string())?;
    Ok(ModelOrder {
        ids: v
            .rankings
            .iter()
            .filter_map(|r| r.capability_id.strip_prefix("workflow:").map(String::from))
            .collect(),
    })
}

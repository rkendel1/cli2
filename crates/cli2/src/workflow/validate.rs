//! Live validity of a workflow against the *current* environment.
//!
//! A recorded success is only evidence about the past. Before a workflow may
//! even be proposed as reusable, every assumption it rested on is re-checked:
//! the capability must still be discovered with the same identity, its
//! interface knowledge must be unchanged, the project context must be the
//! same, the current policy must ask for the same approval, and the evidence
//! supporting the recorded outcome must still be retained. Where something
//! cannot be checked the answer is `candidate`, never `validated`.
//!
//! `validated` means "no recorded assumption has changed". It is not a plan
//! and not an authorization: a fresh plan, approval and verification are
//! always required.

use std::collections::BTreeMap;
use std::path::Path;

use cli2_core::{Capability, InterfaceCompleteness};
use cli2_discovery::find_project_root;
use cli2_evidence::Journal;
use cli2_execution::{assess, Policy};
use serde::Serialize;

use super::{StepRecord, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidityState {
    /// Eligible, nothing changed, but something could not be confirmed.
    Candidate,
    /// Eligible and every recorded assumption still holds.
    Validated,
    /// An assumption changed; visible for inspection, not a current plan.
    Stale,
    /// The evidence never met the eligibility criteria.
    Ineligible,
}

#[derive(Debug, Clone, Serialize)]
pub struct Validity {
    pub workflow_id: String,
    pub state: ValidityState,
    /// Why it is ineligible or stale.
    pub reasons: Vec<String>,
    /// Assumptions checked and found unchanged.
    pub confirmed: Vec<String>,
    /// Assumptions that could not be checked.
    pub unconfirmed: Vec<String>,
    pub checked_in: String,
}

/// Lazily computed result of verifying the journal's hash chain.
pub struct ChainCheck<'a> {
    journal: &'a Journal,
    result: Option<Result<(), String>>,
}

impl<'a> ChainCheck<'a> {
    pub fn new(journal: &'a Journal) -> Self {
        Self {
            journal,
            result: None,
        }
    }
    pub fn get(&mut self) -> &Result<(), String> {
        let j = self.journal;
        self.result
            .get_or_insert_with(|| j.verify_chain().map_err(|e| e.to_string()))
    }
}

pub struct Env<'a> {
    pub caps: BTreeMap<&'a str, &'a Capability>,
    pub policy: &'a Policy,
    pub journal: &'a Journal,
}

fn same_context(recorded: &str, current: &Path) -> Result<bool, String> {
    let rec = Path::new(recorded);
    if !rec.is_dir() {
        return Err(format!(
            "the recorded directory {recorded} no longer exists"
        ));
    }
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(rec) == canon(current) {
        return Ok(true);
    }
    match (find_project_root(rec), find_project_root(current)) {
        (Some(a), Some(b)) => Ok(canon(&a) == canon(&b)),
        _ => Ok(false),
    }
}

fn check_step(
    env: &Env<'_>,
    wf: &Workflow,
    s: &StepRecord,
    stale: &mut Vec<String>,
    confirmed: &mut Vec<String>,
    unconfirmed: &mut Vec<String>,
) {
    let tag = |m: String| {
        if wf.steps.len() > 1 {
            format!("step {:?}: {m}", s.step_id)
        } else {
            m
        }
    };
    let Some(cap_id) = s.capability_id.as_deref() else {
        unconfirmed.push(tag("the capability that ran was not recorded".into()));
        return;
    };
    let Some(cap) = env.caps.get(cap_id) else {
        stale.push(tag(format!("{cap_id} is no longer discovered here")));
        return;
    };
    confirmed.push(tag(format!("{cap_id} is still discovered")));

    // Identity.
    match s.capability_fingerprint.as_deref() {
        Some(fp) if fp == cap.fingerprint => confirmed.push(tag("its identity (executable or task definition) is unchanged".into())),
        Some(_) => stale.push(tag(format!("{cap_id} has changed since it ran (executable replaced or upgraded, or its task definition edited)"))),
        None => unconfirmed.push(tag("the identity it had when it ran was not recorded".into())),
    }

    // Interface knowledge.
    let current = &cap.interface;
    if current.source.starts_with("help_output")
        && current.completeness < InterfaceCompleteness::Inferred
    {
        stale.push(tag(format!(
            "the interface of {cap_id} is no longer sufficiently understood ({:?})",
            current.completeness
        )));
    } else {
        match (&s.interface, &current.capture) {
            (Some(rec), Some(now)) => {
                if rec.captured_at.as_deref() == Some(now.captured_at.as_str())
                    && rec.usage_line == now.usage_line
                    && rec.parser_version == Some(now.parser_version)
                {
                    confirmed.push(tag("the captured interface metadata is unchanged".into()));
                } else {
                    stale.push(tag("the captured interface metadata changed since it ran (help was re-captured or re-parsed)".into()));
                }
            }
            (Some(_), None) => {
                stale.push(tag("the interface it used is no longer recorded".into()))
            }
            (None, _) if current.source.starts_with("help_output") => unconfirmed.push(tag(
                "the interface it used was not recorded at the time".into(),
            )),
            (None, _) => confirmed.push(tag(
                "the interface is declared by the project, covered by its identity".into(),
            )),
        }
    }

    // Authority: what would be required *now*.
    let assessment = assess(&cap.effects, &s.constraints, env.policy);
    if !assessment.violations.is_empty() {
        stale.push(tag(format!(
            "current constraints or policy reject it: {}",
            assessment.violations.join("; ")
        )));
    } else {
        match s.recorded_requirement {
            Some(was) if was == assessment.approval.level => confirmed.push(tag(format!(
                "the approval requirement is unchanged ({was:?}); it must still be granted afresh"
            ))),
            Some(was) => stale.push(tag(format!(
                "the approval requirement changed (was {was:?}, now {:?})",
                assessment.approval.level
            ))),
            None => unconfirmed.push(tag(
                "the approval requirement at the time was not recorded".into()
            )),
        }
    }
    // Evidence.
    if s.evidence_refs.is_empty() {
        unconfirmed.push(tag("no output evidence was recorded for it".into()));
    } else if s.evidence_refs.iter().all(|r| env.journal.blob_exists(r)) {
        confirmed.push(tag(
            "the output evidence supporting its outcome is still retained".into(),
        ));
    } else {
        stale.push(tag(
            "the output evidence supporting its outcome was pruned or is missing".into(),
        ));
    }
}

pub fn check(env: &Env<'_>, wf: &Workflow, cwd: &Path, chain: &mut ChainCheck<'_>) -> Validity {
    let mut v = Validity {
        workflow_id: wf.id.clone(),
        state: ValidityState::Candidate,
        reasons: vec![],
        confirmed: vec![],
        unconfirmed: vec![],
        checked_in: cwd.display().to_string(),
    };
    if !wf.eligibility.eligible {
        v.state = ValidityState::Ineligible;
        v.reasons = wf.eligibility.reasons.clone();
        return v;
    }
    let mut stale = Vec::new();
    // Context.
    match same_context(wf.working_directory(), cwd) {
        Ok(true) => v.confirmed.push("the project context is the same".into()),
        Ok(false) => stale.push(format!(
            "it ran in {} (a different project or directory); you are in {}",
            wf.working_directory(),
            cwd.display()
        )),
        Err(e) => stale.push(e),
    }
    for s in &wf.steps {
        check_step(env, wf, s, &mut stale, &mut v.confirmed, &mut v.unconfirmed);
    }
    match chain.get() {
        Ok(()) => v.confirmed.push("the journal's hash chain verifies".into()),
        Err(e) => {
            v.state = ValidityState::Ineligible;
            v.reasons.push(format!(
                "the journal failed its integrity check, so its evidence cannot be relied on: {e}"
            ));
            return v;
        }
    }
    if !stale.is_empty() {
        v.state = ValidityState::Stale;
        v.reasons = stale;
    } else if v.unconfirmed.is_empty() {
        v.state = ValidityState::Validated;
    }
    v
}

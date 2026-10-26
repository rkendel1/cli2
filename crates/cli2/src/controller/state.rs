//! Task state is a fold over the task's events in the evidence journal.
//!
//! There is no task database: the events (hash-chained journal records under
//! the task's ID) *are* the task. Anything the controller needs to know it
//! derives here, the same way after a restart as during a run.

use std::collections::BTreeMap;

use cli2_core::{ApprovalLevel, Status, VerificationStatus};
use cli2_protocol::devtask::*;
use cli2_protocol::Approvability;

pub struct StepRec {
    pub view: StepView,
    pub bindings: BTreeMap<String, String>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub output_truncated: bool,
    /// Event sequence numbers, to order executions and changes.
    pub started_seq: Option<i64>,
    pub observed_seq: Option<i64>,
}

pub struct Model {
    pub task_id: String,
    pub goal: String,
    pub project: String,
    pub strategy: String,
    pub criteria: Vec<Criterion>,
    pub outline: Vec<OutlineStep>,
    pub limits: Limits,
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub state: TaskState,
    pub paused: bool,
    pub cancel_requested: Option<String>,
    pub blocked_reason: Option<String>,
    pub investigation: Option<Investigation>,
    pub steps: Vec<StepRec>,
    pub attempts: u32,
    pub consecutive_rejections: u32,
    pub consecutive_plan_failures: u32,
    pub statuses: Vec<CriterionStatus>,
    pub final_report: Option<TaskReport>,
    pub last_seq: i64,
    /// Sequence number of the newest `criteria_evaluated` event.
    pub last_evaluated_seq: i64,
}

impl Model {
    pub fn step(&self, id: &str) -> Option<&StepRec> {
        self.steps.iter().find(|s| s.view.step_id == id)
    }

    fn step_mut(&mut self, id: &str) -> Option<&mut StepRec> {
        self.steps.iter_mut().find(|s| s.view.step_id == id)
    }

    /// The step the task is currently working on (the newest unfinished one).
    pub fn current(&self) -> Option<&StepRec> {
        self.steps.last().filter(|s| {
            !matches!(
                s.view.status,
                StepStatus::Executed
                    | StepStatus::Rejected
                    | StepStatus::NoPlan
                    | StepStatus::Unknown
                    | StepStatus::Abandoned
            )
        })
    }
}

/// Fold events (oldest first) into the current model. Returns `None` if there
/// is no `created` event.
pub fn fold(events: &[DevTaskEvent]) -> Option<Model> {
    let mut m: Option<Model> = None;
    for e in events {
        match (&e.data, m.as_mut()) {
            (
                DevTaskEventData::Created {
                    goal,
                    project,
                    strategy,
                    criteria,
                    outline,
                    limits,
                    idempotency_key,
                },
                None,
            ) => {
                m = Some(Model {
                    task_id: e.task_id.clone(),
                    goal: goal.clone(),
                    project: project.clone(),
                    strategy: strategy.clone(),
                    criteria: criteria.clone(),
                    outline: outline.clone(),
                    limits: limits.clone(),
                    idempotency_key: idempotency_key.clone(),
                    created_at: e.at.clone(),
                    updated_at: e.at.clone(),
                    state: TaskState::Created,
                    paused: false,
                    cancel_requested: None,
                    blocked_reason: None,
                    investigation: None,
                    steps: vec![],
                    attempts: 0,
                    consecutive_rejections: 0,
                    consecutive_plan_failures: 0,
                    statuses: vec![],
                    final_report: None,
                    last_seq: e.seq,
                    last_evaluated_seq: 0,
                });
            }
            (_, Some(m)) => apply(m, e),
            _ => {}
        }
    }
    let mut m = m?;
    m.statuses = evaluate(&m.criteria, &m.steps);
    Some(m)
}

fn apply(m: &mut Model, e: &DevTaskEvent) {
    m.updated_at = e.at.clone();
    m.last_seq = e.seq;
    use DevTaskEventData as D;
    match &e.data {
        D::Created { .. } => {}
        D::StateChanged { to, reason, .. } => {
            m.state = *to;
            if *to == TaskState::Blocked {
                m.blocked_reason = Some(reason.clone());
            } else {
                m.blocked_reason = None;
            }
        }
        D::Investigated { investigation } => m.investigation = Some(investigation.clone()),
        D::StepProposed {
            step_id,
            index,
            kind,
            objective,
            intent,
            selection,
            bindings,
            satisfies,
            note,
            ..
        } => {
            m.consecutive_rejections = 0;
            m.steps.push(StepRec {
                view: StepView {
                    step_id: step_id.clone(),
                    index: *index,
                    kind: *kind,
                    objective: objective.clone(),
                    intent: intent.clone(),
                    selection: selection.clone(),
                    satisfies: satisfies.clone(),
                    status: StepStatus::Proposed,
                    plan_id: None,
                    request_id: None,
                    plan_digest: None,
                    approval_required: None,
                    approvability: None,
                    approval_id: None,
                    execution_id: None,
                    outcome: None,
                    proposer_note: note.clone(),
                    detail: None,
                },
                bindings: bindings.clone(),
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                output_truncated: false,
                started_seq: None,
                observed_seq: None,
            });
        }
        D::ProposalRejected { .. } => m.consecutive_rejections += 1,
        D::PlanMade {
            step_id,
            plan_id,
            request_id,
            plan_digest,
            approval_level,
            approvability,
        } => {
            m.consecutive_plan_failures = 0;
            if let Some(s) = m.step_mut(step_id) {
                s.view.plan_id = Some(plan_id.clone());
                s.view.request_id = Some(request_id.clone());
                s.view.plan_digest = Some(plan_digest.clone());
                s.view.approval_required = Some(*approval_level);
                s.view.approvability = Some(*approvability);
                s.view.status = StepStatus::AwaitingApproval;
            }
        }
        D::PlanFailed {
            step_id,
            status,
            message,
        } => {
            m.consecutive_plan_failures += 1;
            if let Some(s) = m.step_mut(step_id) {
                s.view.status = StepStatus::NoPlan;
                s.view.detail = Some(format!("{status}: {message}"));
            }
        }
        D::ApprovalGranted {
            step_id,
            approval_id,
            ..
        } => {
            if let Some(s) = m.step_mut(step_id) {
                s.view.approval_id = Some(approval_id.clone());
                s.view.status = StepStatus::Approved;
            }
        }
        D::StepRejected {
            step_id, reason, ..
        } => {
            if let Some(s) = m.step_mut(step_id) {
                s.view.status = StepStatus::Rejected;
                s.view.detail = Some(reason.clone());
            }
        }
        D::ExecutionStarted {
            step_id,
            execution_id,
        } => {
            if let Some(s) = m.step_mut(step_id) {
                s.view.execution_id = Some(execution_id.clone());
                s.view.status = StepStatus::Executing;
                s.started_seq = Some(e.seq);
            }
        }
        D::StepObserved {
            step_id,
            outcome,
            stdout_tail,
            stderr_tail,
            output_truncated,
            ..
        } => {
            if let Some(s) = m.step_mut(step_id) {
                s.view.status = if outcome.outcome_known {
                    StepStatus::Executed
                } else {
                    StepStatus::Unknown
                };
                s.view.outcome = Some(outcome.clone());
                s.stdout_tail = stdout_tail.clone();
                s.stderr_tail = stderr_tail.clone();
                s.output_truncated = *output_truncated;
                s.observed_seq = Some(e.seq);
            }
        }
        D::CriteriaEvaluated { .. } => m.last_evaluated_seq = e.seq,
        D::AttemptCounted { attempts, .. } => m.attempts = *attempts,
        D::Paused => m.paused = true,
        D::Resumed => m.paused = false,
        D::CancelRequested { reason } => m.cancel_requested = Some(reason.clone()),
        D::Completed { report } => m.final_report = Some((**report).clone()),
        D::Failed { report, .. } => m.final_report = Some((**report).clone()),
        D::Blocked { .. } => {}
    }
}

/// Judge every acceptance criterion from what the runtime recorded.
///
/// A criterion passes only when the latest execution of a step that claims to
/// satisfy it ended `succeeded` **and** its verification is `passed` (a
/// task-level check, not just the exit status), and, if the criterion needs it,
/// no other change was made after that execution started. Everything else is
/// `failed` or `unverified`; nothing is assumed.
pub fn evaluate(criteria: &[Criterion], steps: &[StepRec]) -> Vec<CriterionStatus> {
    criteria
        .iter()
        .map(|c| {
            let mk = |outcome, reason: &str, exec: Option<&str>| CriterionStatus {
                id: c.id.clone(),
                description: c.description.clone(),
                outcome,
                reason: reason.to_string(),
                execution_id: exec.map(String::from),
            };
            let claimed: Vec<&StepRec> = steps
                .iter()
                .filter(|s| s.view.satisfies.contains(&c.id) && s.started_seq.is_some())
                .collect();
            let Some(latest) = claimed.iter().max_by_key(|s| s.started_seq) else {
                return mk(
                    CriterionOutcome::Unverified,
                    "no step has run to verify this yet",
                    None,
                );
            };
            let exec = latest.view.execution_id.as_deref();
            let Some(o) = &latest.view.outcome else {
                return mk(
                    CriterionOutcome::Unverified,
                    "the step is still running",
                    exec,
                );
            };
            if !o.outcome_known {
                return mk(
                    CriterionOutcome::Unverified,
                    "the execution was interrupted; its outcome is unknown",
                    exec,
                );
            }
            match (o.task_status, o.verification) {
                (Some(Status::Succeeded), Some(VerificationStatus::Passed)) => {
                    let changed_after = c.after_changes
                        && steps.iter().any(|other| {
                            other.view.kind == StepKind::Change
                                && other.view.step_id != latest.view.step_id
                                && other.started_seq.is_some()
                                // A change that never finished still counts: it may have altered the tree.
                                && other.observed_seq.or(other.started_seq) > latest.started_seq
                        });
                    if changed_after {
                        mk(
                            CriterionOutcome::Unverified,
                            "stale: a change was made after this was verified",
                            exec,
                        )
                    } else {
                        mk(
                            CriterionOutcome::Passed,
                            "verified by a passing task-level check",
                            exec,
                        )
                    }
                }
                (Some(Status::Succeeded), _) => mk(
                    CriterionOutcome::Unverified,
                    "the process exited 0, but no task-level check established this criterion",
                    exec,
                ),
                (Some(Status::VerificationFailed), _) => mk(
                    CriterionOutcome::Failed,
                    "the process ran but its task-level verification failed",
                    exec,
                ),
                (Some(Status::Cancelled), _) => mk(
                    CriterionOutcome::Unverified,
                    "the execution was cancelled",
                    exec,
                ),
                (Some(s), _) => mk(
                    CriterionOutcome::Failed,
                    &format!("the execution ended {s:?}"),
                    exec,
                ),
                (None, _) => mk(
                    CriterionOutcome::Unverified,
                    "the execution recorded no task status",
                    exec,
                ),
            }
        })
        .collect()
}

pub fn all_passed(statuses: &[CriterionStatus]) -> bool {
    !statuses.is_empty()
        && statuses
            .iter()
            .all(|s| s.outcome == CriterionOutcome::Passed)
}

fn short(s: &str) -> String {
    s.chars().take(60).collect()
}

/// A plain, honest account of the task so far (or its final one).
pub fn report(m: &Model, outcome_hint: Option<ReportOutcome>) -> TaskReport {
    let verified: Vec<CriterionStatus> = m
        .statuses
        .iter()
        .filter(|s| s.outcome == CriterionOutcome::Passed)
        .cloned()
        .collect();
    let failed: Vec<CriterionStatus> = m
        .statuses
        .iter()
        .filter(|s| s.outcome == CriterionOutcome::Failed)
        .cloned()
        .collect();
    let unverified_c: Vec<&CriterionStatus> = m
        .statuses
        .iter()
        .filter(|s| s.outcome == CriterionOutcome::Unverified)
        .collect();
    let outcome = outcome_hint.unwrap_or(match m.state {
        TaskState::Completed => ReportOutcome::Verified,
        TaskState::Failed => {
            if verified.is_empty() {
                ReportOutcome::Failed
            } else {
                ReportOutcome::PartiallyVerified
            }
        }
        TaskState::Blocked => ReportOutcome::Blocked,
        TaskState::Cancelled => ReportOutcome::Cancelled,
        _ => ReportOutcome::InProgress,
    });
    let mut unverified: Vec<String> = unverified_c
        .iter()
        .map(|s| format!("{}: {}", s.id, s.reason))
        .collect();
    let mut changes = Vec::new();
    let mut evidence = Vec::new();
    for s in &m.steps {
        if let Some(x) = &s.view.execution_id {
            evidence.push(EvidenceRef {
                step_id: s.view.step_id.clone(),
                execution_id: x.clone(),
                request_id: s.view.request_id.clone(),
                plan_id: s.view.plan_id.clone(),
            });
        }
        let ran = matches!(s.view.status, StepStatus::Executed | StepStatus::Unknown);
        if s.view.kind == StepKind::Change && s.started_seq.is_some() {
            let how = match (&s.view.outcome, s.view.status) {
                (_, StepStatus::Unknown) => "outcome unknown (interrupted)".to_string(),
                (Some(o), _) => format!(
                    "{:?}",
                    o.task_status
                        .map(|t| format!("{t:?}"))
                        .unwrap_or_else(|| "no status".into())
                ),
                (None, _) => "still running".to_string(),
            };
            changes.push(format!(
                "{}: {} [{}]",
                s.view.step_id,
                short(&s.view.objective),
                how
            ));
        }
        if ran && s.view.status == StepStatus::Unknown {
            unverified.push(format!(
                "{}: its outcome is unknown, so any effect it had is unconfirmed",
                s.view.step_id
            ));
        }
    }
    let mut remaining: Vec<String> = Vec::new();
    for s in &m.statuses {
        if s.outcome != CriterionOutcome::Passed {
            remaining.push(format!("{} — {} ({})", s.id, s.description, s.reason));
        }
    }
    if let Some(r) = &m.blocked_reason {
        remaining.push(format!("blocked: {r}"));
    }
    let summary = match outcome {
        ReportOutcome::Verified => format!(
            "All {} acceptance criteria were verified by passing task-level checks after {} step(s).",
            m.statuses.len(),
            m.steps.iter().filter(|s| s.started_seq.is_some()).count()
        ),
        ReportOutcome::PartiallyVerified => format!(
            "{} of {} criteria were verified; the rest are not. This is not a completed task.",
            verified.len(),
            m.statuses.len()
        ),
        ReportOutcome::Failed => "The task did not achieve its goal; no acceptance criterion was verified.".to_string(),
        ReportOutcome::Blocked => format!("The task is blocked: {}", m.blocked_reason.clone().unwrap_or_default()),
        ReportOutcome::Cancelled => "The task was cancelled. Anything it executed before then is listed under changes; nothing is claimed as verified beyond the criteria below.".to_string(),
        ReportOutcome::InProgress => "The task is still in progress.".to_string(),
    };
    TaskReport {
        schema_version: DEVTASK_REPORT_SCHEMA.into(),
        task_id: m.task_id.clone(),
        goal: m.goal.clone(),
        outcome,
        summary,
        verified,
        failed,
        unverified,
        changes,
        remaining_work: remaining,
        steps_taken: m.steps.iter().filter(|s| s.started_seq.is_some()).count() as u32,
        attempts: m.attempts,
        evidence,
    }
}

pub fn approval_needs_person(
    level: Option<ApprovalLevel>,
    approvability: Option<Approvability>,
) -> bool {
    !matches!(
        (level, approvability),
        (Some(ApprovalLevel::None), Some(Approvability::NotRequired))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cli2_core::Check;

    fn crit(id: &str, after: bool) -> Criterion {
        Criterion {
            id: id.into(),
            description: id.into(),
            checks: vec![Check::StdoutContains { text: "ok".into() }],
            after_changes: after,
            verify_intent: None,
        }
    }

    fn step(
        id: &str,
        kind: StepKind,
        satisfies: &[&str],
        started: i64,
        observed: i64,
        status: Status,
        v: VerificationStatus,
    ) -> StepRec {
        StepRec {
            view: StepView {
                step_id: id.into(),
                index: 1,
                kind,
                objective: id.into(),
                intent: id.into(),
                selection: None,
                satisfies: satisfies.iter().map(|s| s.to_string()).collect(),
                status: StepStatus::Executed,
                plan_id: None,
                request_id: None,
                plan_digest: None,
                approval_required: None,
                approvability: None,
                approval_id: None,
                execution_id: Some(format!("run_{id}")),
                outcome: Some(StepOutcome {
                    execution_id: format!("run_{id}"),
                    process: None,
                    task_status: Some(status),
                    verification: Some(v),
                    outcome_known: true,
                }),
                proposer_note: None,
                detail: None,
            },
            bindings: Default::default(),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            output_truncated: false,
            started_seq: Some(started),
            observed_seq: Some(observed),
        }
    }

    #[test]
    fn the_state_machine_only_allows_documented_transitions() {
        use TaskState::*;
        for (a, b) in [
            (Created, Investigating),
            (Investigating, Planned),
            (Planned, AwaitingApproval),
            (AwaitingApproval, Executing),
            (Executing, Verifying),
            (Verifying, Completed),
            (Blocked, Investigating),
        ] {
            assert!(a.can_become(b), "{a:?} -> {b:?}");
        }
        for (a, b) in [
            (Created, Executing),
            (Created, Completed),
            (Executing, Completed),
            (Planned, Completed),
            (AwaitingApproval, Verifying),
            (Completed, Investigating),
            (Failed, Investigating),
            (Cancelled, Created),
            (Blocked, Executing),
        ] {
            assert!(!a.can_become(b), "{a:?} -> {b:?} must be illegal");
        }
        // Nothing may skip verification on the way to completed.
        for from in [
            Created,
            Investigating,
            Planned,
            AwaitingApproval,
            Executing,
            Blocked,
        ] {
            assert!(!from.can_become(Completed), "{from:?} -> completed");
        }
    }

    #[test]
    fn a_criterion_needs_a_passing_task_level_check_not_just_exit_zero() {
        let cs = [crit("a", false)];
        let ok = [step(
            "s1",
            StepKind::Verify,
            &["a"],
            1,
            2,
            Status::Succeeded,
            VerificationStatus::Passed,
        )];
        assert_eq!(evaluate(&cs, &ok)[0].outcome, CriterionOutcome::Passed);
        let exit_zero = [step(
            "s1",
            StepKind::Verify,
            &["a"],
            1,
            2,
            Status::Succeeded,
            VerificationStatus::Unverified,
        )];
        let r = &evaluate(&cs, &exit_zero)[0];
        assert_eq!(r.outcome, CriterionOutcome::Unverified);
        assert!(r.reason.contains("no task-level check"));
        let vf = [step(
            "s1",
            StepKind::Verify,
            &["a"],
            1,
            2,
            Status::VerificationFailed,
            VerificationStatus::Failed,
        )];
        assert_eq!(evaluate(&cs, &vf)[0].outcome, CriterionOutcome::Failed);
        let failed = [step(
            "s1",
            StepKind::Verify,
            &["a"],
            1,
            2,
            Status::Failed,
            VerificationStatus::Failed,
        )];
        assert_eq!(evaluate(&cs, &failed)[0].outcome, CriterionOutcome::Failed);
        assert_eq!(evaluate(&cs, &[])[0].outcome, CriterionOutcome::Unverified);
        assert!(!all_passed(&[]));
    }

    #[test]
    fn verification_that_predates_a_later_change_is_stale() {
        let cs = [crit("tests", true), crit("present", false)];
        let steps = [
            step(
                "verify1",
                StepKind::Verify,
                &["tests", "present"],
                1,
                2,
                Status::Succeeded,
                VerificationStatus::Passed,
            ),
            step(
                "change",
                StepKind::Change,
                &[],
                3,
                4,
                Status::Succeeded,
                VerificationStatus::Unverified,
            ),
        ];
        let r = evaluate(&cs, &steps);
        assert_eq!(
            r[0].outcome,
            CriterionOutcome::Unverified,
            "tests must be re-run after the change"
        );
        assert!(r[0].reason.contains("stale"));
        assert_eq!(
            r[1].outcome,
            CriterionOutcome::Passed,
            "a criterion that does not depend on later changes is unaffected"
        );
        // Re-verifying after the change restores it.
        let mut steps = Vec::from(steps);
        steps.push(step(
            "verify2",
            StepKind::Verify,
            &["tests"],
            5,
            6,
            Status::Succeeded,
            VerificationStatus::Passed,
        ));
        assert_eq!(evaluate(&cs, &steps)[0].outcome, CriterionOutcome::Passed);
    }

    #[test]
    fn a_change_whose_outcome_is_unknown_still_invalidates_earlier_verification() {
        let cs = [crit("tests", true)];
        let mut interrupted = step(
            "change",
            StepKind::Change,
            &[],
            3,
            4,
            Status::Cancelled,
            VerificationStatus::NotRun,
        );
        interrupted.view.outcome.as_mut().unwrap().outcome_known = false;
        interrupted.view.status = StepStatus::Unknown;
        let steps = [
            step(
                "v",
                StepKind::Verify,
                &["tests"],
                1,
                2,
                Status::Succeeded,
                VerificationStatus::Passed,
            ),
            interrupted,
        ];
        assert_eq!(
            evaluate(&cs, &steps)[0].outcome,
            CriterionOutcome::Unverified
        );
    }
}

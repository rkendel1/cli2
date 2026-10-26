//! Deterministic postcondition verification, performed by runtime code.

use std::path::Path;
use std::time::{Duration, SystemTime};

use cli2_core::{
    Check, CheckResult, CheckStatus, StepResult, Verification, VerificationProcedure,
    VerificationStatus,
};

/// What verification needs to know about each executed step.
pub struct StepObservation<'a> {
    pub result: &'a StepResult,
    pub started: SystemTime,
    pub stdout: &'a [u8],
}

/// Evaluate a plan's verification procedure against what actually happened.
///
/// Process-level checks (exit status) are reported separately from task-level
/// checks; a run where only process-level checks were possible is
/// `unverified`, never `passed`.
pub fn verify(procedure: &VerificationProcedure, steps: &[StepObservation<'_>]) -> Verification {
    let all_ran_ok = !steps.is_empty() && steps.iter().all(|s| s.result.process_succeeded());
    let mut checks = Vec::new();
    for planned in &procedure.checks {
        let (status, detail) = match &planned.check {
            Check::ProcessExitStatus { expected } => {
                if steps.is_empty() {
                    (CheckStatus::Skipped, "no process was executed".to_string())
                } else {
                    let bad: Vec<String> = steps
                        .iter()
                        .filter(|s| {
                            !(s.result.termination == cli2_core::Termination::Exited
                                && s.result.exit_code == Some(*expected))
                        })
                        .map(|s| {
                            format!(
                                "step {} ended {:?} with exit code {:?}",
                                s.result.index, s.result.termination, s.result.exit_code
                            )
                        })
                        .collect();
                    if bad.is_empty() {
                        (
                            CheckStatus::Passed,
                            format!("all steps exited with status {expected}"),
                        )
                    } else {
                        (CheckStatus::Failed, bad.join("; "))
                    }
                }
            }
            _ if !all_ran_ok => (
                CheckStatus::Skipped,
                "not evaluated because process execution did not succeed".to_string(),
            ),
            Check::FileExists {
                path,
                modified_after_start,
            } => {
                let first = &steps[0];
                let full = Path::new(&first.result.working_directory).join(path);
                match std::fs::metadata(&full) {
                    Ok(m) if m.is_file() => {
                        if *modified_after_start {
                            // Allow for coarse filesystem timestamp granularity.
                            let threshold = first.started - Duration::from_millis(5);
                            match m.modified() {
                                Ok(t) if t >= threshold => (
                                    CheckStatus::Passed,
                                    format!(
                                        "{} exists and was written during execution",
                                        full.display()
                                    ),
                                ),
                                Ok(_) => (
                                    CheckStatus::Failed,
                                    format!(
                                        "{} exists but was not modified during execution",
                                        full.display()
                                    ),
                                ),
                                Err(e) => (CheckStatus::Failed, format!("cannot read mtime: {e}")),
                            }
                        } else {
                            (CheckStatus::Passed, format!("{} exists", full.display()))
                        }
                    }
                    Ok(_) => (
                        CheckStatus::Failed,
                        format!("{} is not a regular file", full.display()),
                    ),
                    Err(_) => (
                        CheckStatus::Failed,
                        format!("{} does not exist", full.display()),
                    ),
                }
            }
            Check::PathExists { path } => {
                let full = Path::new(&steps[0].result.working_directory).join(path);
                if full.exists() {
                    (CheckStatus::Passed, format!("{} exists", full.display()))
                } else {
                    (
                        CheckStatus::Failed,
                        format!("{} does not exist", full.display()),
                    )
                }
            }
            Check::StdoutContains { text } => {
                let last = steps.last().expect("steps non-empty when all_ran_ok");
                let out = String::from_utf8_lossy(last.stdout);
                if out.contains(text.as_str()) {
                    (CheckStatus::Passed, "found in captured stdout".to_string())
                } else if last.result.stdout_truncated {
                    (
                        CheckStatus::Failed,
                        "not found in captured stdout (output was truncated)".to_string(),
                    )
                } else {
                    (
                        CheckStatus::Failed,
                        "not found in captured stdout".to_string(),
                    )
                }
            }
        };
        checks.push(CheckResult {
            check: planned.check.clone(),
            task_level: planned.task_level,
            status,
            detail,
        });
    }

    let any_failed = checks.iter().any(|c| c.status == CheckStatus::Failed);
    let task_level_passed = checks
        .iter()
        .any(|c| c.task_level && c.status == CheckStatus::Passed);
    let status = if steps.is_empty() {
        VerificationStatus::NotRun
    } else if any_failed {
        VerificationStatus::Failed
    } else if task_level_passed {
        VerificationStatus::Passed
    } else {
        VerificationStatus::Unverified
    };
    Verification {
        status,
        task_level: checks
            .iter()
            .any(|c| c.task_level && c.status != CheckStatus::Skipped),
        checks,
    }
}

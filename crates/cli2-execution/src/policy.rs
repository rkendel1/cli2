//! Authority: constraint validation, approval requirements and enforcement.
//!
//! Every decision here is made by runtime code from recorded facts. A model
//! (if one is configured) never participates.

use cli2_core::{
    new_id, now_rfc3339, ApprovalDecision, ApprovalLevel, ApprovalMethod, ApprovalPolicyOutcome,
    ApprovalRecord, ApprovalRequirement, Constraints, EffectCertainty, EffectClass, Effects,
    NetworkConstraint, Plan, Risk, APPROVAL_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

/// Local authority policy. Loaded from `policy.json` in the cli2 home
/// directory when present; otherwise the defaults below apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Effects that can never be approved. Default: privilege escalation.
    #[serde(default = "default_deny")]
    pub deny_effects: Vec<EffectClass>,
    /// Whether `--yes` may approve operations whose effects are unknown.
    /// Default true (they still need *some* approval; they never run silently).
    #[serde(default = "yes")]
    pub yes_approves_unknown_effects: bool,
}

fn default_deny() -> Vec<EffectClass> {
    vec![EffectClass::PrivilegeEscalation]
}
fn yes() -> bool {
    true
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            deny_effects: default_deny(),
            yes_approves_unknown_effects: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub risk: Risk,
    pub approval: ApprovalPolicyOutcome,
    /// Constraint violations: the plan must be rejected.
    pub violations: Vec<String>,
    pub limitations: Vec<String>,
}

/// Assess a plan's effects against the request constraints and the policy.
pub fn assess(effects: &Effects, constraints: &Constraints, policy: &Policy) -> Assessment {
    let mut violations = Vec::new();
    let mut limitations = Vec::new();
    let unknown = effects.certainty == EffectCertainty::Unknown;

    if constraints.modify_files == Some(false) {
        if unknown {
            violations.push(
                "request forbids file modification, but this operation's effects are unknown, so it cannot be shown to comply".into(),
            );
        } else if let Some(c) = effects.classes.iter().find(|c| c.modifies_files()) {
            violations.push(format!(
                "request forbids file modification, but the operation may have effect `{}` ({})",
                c.as_str(),
                effects.source
            ));
        } else if effects.certainty == EffectCertainty::Heuristic {
            limitations.push(
                "compliance with modify_files=false rests on a heuristic effect assessment".into(),
            );
        }
    }
    if constraints.network == Some(NetworkConstraint::Deny) {
        if unknown {
            violations.push(
                "request forbids network access, but this operation's effects are unknown, so it cannot be shown to comply".into(),
            );
        } else if let Some(c) = effects.classes.iter().find(|c| c.uses_network()) {
            violations.push(format!(
                "request forbids network access, but the operation may have effect `{}`",
                c.as_str()
            ));
        } else if effects.certainty == EffectCertainty::Heuristic {
            limitations
                .push("compliance with network=deny rests on a heuristic effect assessment".into());
        }
    }

    let mut reqs: Vec<ApprovalRequirement> = Vec::new();
    for c in &effects.classes {
        if policy.deny_effects.contains(c) {
            reqs.push(ApprovalRequirement {
                level: ApprovalLevel::Forbidden,
                reason: format!(
                    "effect `{}` is denied by policy and cannot be approved",
                    c.as_str()
                ),
                effect: Some(*c),
            });
        } else if c.is_high_impact() {
            let certainty = if effects.certainty == EffectCertainty::Declared {
                "declared"
            } else {
                "possible"
            };
            reqs.push(ApprovalRequirement {
                level: ApprovalLevel::Explicit,
                reason: format!(
                    "{certainty} high-impact effect `{}` requires explicit approval",
                    c.as_str()
                ),
                effect: Some(*c),
            });
        }
    }
    if reqs.is_empty() {
        if effects.is_well_characterized_read_only() {
            reqs.push(ApprovalRequirement {
                level: ApprovalLevel::None,
                reason: "declared read-only; runs under the default local policy".into(),
                effect: Some(EffectClass::ReadOnly),
            });
        } else if unknown {
            reqs.push(ApprovalRequirement {
                level: if policy.yes_approves_unknown_effects {
                    ApprovalLevel::Confirm
                } else {
                    ApprovalLevel::Explicit
                },
                reason: "effects are unknown and are not treated as read-only".into(),
                effect: None,
            });
        } else if effects.classes.contains(&EffectClass::ModifiesFiles) {
            let permitted = constraints.modify_files == Some(true)
                && effects.certainty == EffectCertainty::Declared;
            reqs.push(ApprovalRequirement {
                level: if permitted {
                    ApprovalLevel::None
                } else {
                    ApprovalLevel::Confirm
                },
                reason: if permitted {
                    "declared file modification permitted by the request's modify_files=true constraint".into()
                } else {
                    "may modify files; requires permission".into()
                },
                effect: Some(EffectClass::ModifiesFiles),
            });
        } else {
            reqs.push(ApprovalRequirement {
                level: ApprovalLevel::Confirm,
                reason: format!(
                    "read-only only by {} assessment; not well characterized",
                    effects.source
                ),
                effect: Some(EffectClass::ReadOnly),
            });
        }
    }
    let level = reqs
        .iter()
        .map(|r| r.level)
        .max()
        .unwrap_or(ApprovalLevel::Confirm);
    let risk = match level {
        ApprovalLevel::None => Risk::Low,
        ApprovalLevel::Confirm => Risk::Medium,
        ApprovalLevel::Explicit => Risk::High,
        ApprovalLevel::Forbidden => Risk::Forbidden,
    };
    if level == ApprovalLevel::Forbidden {
        violations.extend(
            reqs.iter()
                .filter(|r| r.level == ApprovalLevel::Forbidden)
                .map(|r| r.reason.clone()),
        );
    }
    Assessment {
        risk,
        approval: ApprovalPolicyOutcome {
            level,
            requirements: reqs,
        },
        violations,
        limitations,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorizationError {
    #[error("the plan has been modified since it was sealed (digest mismatch)")]
    PlanModified,
    #[error("the plan cannot be approved under the active policy")]
    Forbidden,
    #[error("approval required: {0}")]
    ApprovalRequired(String),
    #[error("the approval was granted for a different plan or plan version")]
    ApprovalMismatch,
    #[error("approval was denied")]
    Denied,
    #[error("the approval ({granted:?}) does not satisfy the required level ({required:?})")]
    Insufficient {
        required: ApprovalLevel,
        granted: ApprovalLevel,
    },
}

/// The level an approval method can grant.
pub fn method_grants(method: ApprovalMethod) -> ApprovalLevel {
    match method {
        ApprovalMethod::PolicyAutomatic => ApprovalLevel::None,
        ApprovalMethod::YesFlag => ApprovalLevel::Confirm,
        ApprovalMethod::Interactive | ApprovalMethod::DigestToken => ApprovalLevel::Explicit,
    }
}

/// Create an approval record bound to this exact plan.
pub fn grant(plan: &Plan, method: ApprovalMethod, decision: ApprovalDecision) -> ApprovalRecord {
    ApprovalRecord {
        schema_version: APPROVAL_SCHEMA_VERSION.into(),
        approval_id: new_id("appr"),
        plan_id: plan.plan_id.clone(),
        plan_digest: plan.digest.clone(),
        decision,
        method,
        level_granted: method_grants(method),
        decided_at: now_rfc3339(),
    }
}

/// Enforce authority immediately before execution.
pub fn authorize(plan: &Plan, approval: Option<&ApprovalRecord>) -> Result<(), AuthorizationError> {
    if !plan.is_intact() {
        return Err(AuthorizationError::PlanModified);
    }
    let required = plan.approval.level;
    match required {
        ApprovalLevel::Forbidden => return Err(AuthorizationError::Forbidden),
        ApprovalLevel::None => return Ok(()),
        _ => {}
    }
    let Some(a) = approval else {
        let reasons: Vec<_> = plan
            .approval
            .requirements
            .iter()
            .map(|r| r.reason.clone())
            .collect();
        return Err(AuthorizationError::ApprovalRequired(reasons.join("; ")));
    };
    if a.plan_id != plan.plan_id || a.plan_digest != plan.compute_digest() {
        return Err(AuthorizationError::ApprovalMismatch);
    }
    if a.decision != ApprovalDecision::Granted {
        return Err(AuthorizationError::Denied);
    }
    if a.level_granted < required {
        return Err(AuthorizationError::Insufficient {
            required,
            granted: a.level_granted,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cli2_core::*;

    fn effects(classes: Vec<EffectClass>, certainty: EffectCertainty) -> Effects {
        Effects {
            classes,
            certainty,
            source: "test".into(),
            confidence: 1.0,
            notes: vec![],
        }
    }

    fn plan_with(level: ApprovalLevel) -> Plan {
        let mut p = Plan {
            schema_version: PLAN_SCHEMA_VERSION.into(),
            plan_id: "plan_1".into(),
            request_id: "req_1".into(),
            created_at: "t".into(),
            intent: "x".into(),
            candidates: vec![],
            selection_reason: "".into(),
            steps: vec![Step::Process(ProcessStep {
                index: 0,
                capability_id: "executable:true".into(),
                capability_fingerprint: "fp".into(),
                executable: "true".into(),
                resolved_executable: "/bin/true".into(),
                executable_fingerprint: "fp".into(),
                arguments: vec![],
                working_directory: "/".into(),
            })],
            constraints: Constraints::default(),
            timeout_secs: 10,
            effects: effects(vec![], EffectCertainty::Unknown),
            risk: Risk::Medium,
            approval: ApprovalPolicyOutcome {
                level,
                requirements: vec![],
            },
            verification: VerificationProcedure {
                checks: vec![],
                success_criteria: vec![],
            },
            rationale: "".into(),
            limitations: vec![],
            digest: String::new(),
        };
        p.seal();
        p
    }

    #[test]
    fn unknown_effects_are_not_read_only() {
        let a = assess(
            &Effects::unknown("x"),
            &Constraints::default(),
            &Policy::default(),
        );
        assert_eq!(a.approval.level, ApprovalLevel::Confirm);
        let a = assess(
            &effects(vec![EffectClass::ReadOnly], EffectCertainty::Heuristic),
            &Constraints::default(),
            &Policy::default(),
        );
        assert_eq!(a.approval.level, ApprovalLevel::Confirm);
        let a = assess(
            &effects(vec![EffectClass::ReadOnly], EffectCertainty::Declared),
            &Constraints::default(),
            &Policy::default(),
        );
        assert_eq!(a.approval.level, ApprovalLevel::None);
    }

    #[test]
    fn high_impact_needs_explicit_and_privilege_is_forbidden() {
        let a = assess(
            &effects(vec![EffectClass::Publishes], EffectCertainty::Heuristic),
            &Constraints::default(),
            &Policy::default(),
        );
        assert_eq!(a.approval.level, ApprovalLevel::Explicit);
        let a = assess(
            &effects(
                vec![EffectClass::PrivilegeEscalation],
                EffectCertainty::Heuristic,
            ),
            &Constraints::default(),
            &Policy::default(),
        );
        assert_eq!(a.approval.level, ApprovalLevel::Forbidden);
        assert!(!a.violations.is_empty());
    }

    #[test]
    fn constraints_reject_unknown_and_modifying_plans() {
        let c = Constraints {
            modify_files: Some(false),
            network: None,
        };
        assert!(!assess(&Effects::unknown("x"), &c, &Policy::default())
            .violations
            .is_empty());
        assert!(!assess(
            &effects(vec![EffectClass::ModifiesFiles], EffectCertainty::Heuristic),
            &c,
            &Policy::default()
        )
        .violations
        .is_empty());
    }

    #[test]
    fn yes_cannot_satisfy_explicit() {
        let p = plan_with(ApprovalLevel::Explicit);
        let a = grant(&p, ApprovalMethod::YesFlag, ApprovalDecision::Granted);
        assert!(matches!(
            authorize(&p, Some(&a)),
            Err(AuthorizationError::Insufficient { .. })
        ));
        let a = grant(&p, ApprovalMethod::DigestToken, ApprovalDecision::Granted);
        assert_eq!(authorize(&p, Some(&a)), Ok(()));
        let p = plan_with(ApprovalLevel::Forbidden);
        let a = grant(&p, ApprovalMethod::Interactive, ApprovalDecision::Granted);
        assert_eq!(authorize(&p, Some(&a)), Err(AuthorizationError::Forbidden));
    }

    #[test]
    fn modified_plan_invalidates_approval() {
        let mut p = plan_with(ApprovalLevel::Confirm);
        let a = grant(&p, ApprovalMethod::YesFlag, ApprovalDecision::Granted);
        assert_eq!(authorize(&p, Some(&a)), Ok(()));
        // Tamper without resealing.
        if let Step::Process(s) = &mut p.steps[0] {
            s.arguments.push("--force".into());
        }
        assert_eq!(
            authorize(&p, Some(&a)),
            Err(AuthorizationError::PlanModified)
        );
        // Reseal: now a different plan version; the old approval doesn't apply.
        p.seal();
        assert_eq!(
            authorize(&p, Some(&a)),
            Err(AuthorizationError::ApprovalMismatch)
        );
    }
}

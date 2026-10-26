//! Execution plans and the approval contract.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capability::{CapabilityKind, EffectClass, Effects};
use crate::check::Check;
use crate::fileedit::FileEditStep;
use crate::request::Constraints;

pub const PLAN_SCHEMA_VERSION: &str = "cli2.plan.v1";
pub const APPROVAL_SCHEMA_VERSION: &str = "cli2.approval.v1";

/// A ranked candidate considered during resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Candidate {
    pub capability_id: String,
    pub kind: CapabilityKind,
    pub score: f64,
    pub viable: bool,
    /// Why the candidate scored as it did, or why it is not viable.
    pub reasons: Vec<String>,
    /// The argument vector this candidate would run, when constructible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<Vec<String>>,
}

/// One concrete process invocation. Plans never rely on a shell interpreting a
/// concatenated command string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStep {
    pub index: usize,
    pub capability_id: String,
    pub capability_fingerprint: String,
    /// Executable name as known to the capability interface.
    pub executable: String,
    /// Absolute path that will actually be executed.
    pub resolved_executable: String,
    /// Fingerprint of the executable file at planning time.
    pub executable_fingerprint: String,
    pub arguments: Vec<String>,
    pub working_directory: String,
}

impl ProcessStep {
    pub fn argv(&self) -> Vec<String> {
        let mut v = vec![self.executable.clone()];
        v.extend(self.arguments.iter().cloned());
        v
    }

    pub fn display_command(&self) -> String {
        self.argv()
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One step of a plan: a process, or a governed file modification.
///
/// Process steps serialize exactly as they always have (no `kind`), so plans
/// recorded before file edits existed keep their shape and their digests. A
/// file edit carries `"kind": "file_edit"`. An unknown `kind` is refused.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Step {
    Process(ProcessStep),
    FileEdit(FileEditStep),
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(d)?;
        match value.get("kind").map(|k| k.as_str()) {
            None => serde_json::from_value(value)
                .map(Step::Process)
                .map_err(D::Error::custom),
            Some(Some("file_edit")) => serde_json::from_value(value)
                .map(Step::FileEdit)
                .map_err(D::Error::custom),
            Some(_) => Err(D::Error::custom("unknown plan step kind")),
        }
    }
}

impl Step {
    pub fn index(&self) -> usize {
        match self {
            Step::Process(p) => p.index,
            Step::FileEdit(f) => f.index,
        }
    }

    pub fn capability_id(&self) -> &str {
        match self {
            Step::Process(p) => &p.capability_id,
            Step::FileEdit(f) => &f.capability_id,
        }
    }

    pub fn as_process(&self) -> Option<&ProcessStep> {
        match self {
            Step::Process(p) => Some(p),
            Step::FileEdit(_) => None,
        }
    }

    pub fn as_file_edit(&self) -> Option<&FileEditStep> {
        match self {
            Step::FileEdit(f) => Some(f),
            Step::Process(_) => None,
        }
    }

    /// Human display of what the step does.
    pub fn display_command(&self) -> String {
        match self {
            Step::Process(p) => p.display_command(),
            Step::FileEdit(f) => format!("edit {}", shell_quote(&f.path)),
        }
    }
}

/// Quote an argument for human display only (never used for execution).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    Forbidden,
}

/// The approval a plan needs before the runtime will execute it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLevel {
    /// Well-characterized read-only operation; runs under the default policy.
    None,
    /// Requires confirmation; `--yes` may grant it non-interactively.
    Confirm,
    /// Requires explicit approval of this exact plan (interactive `yes`, or the
    /// plan digest passed via `--approve`). `--yes` is not sufficient.
    Explicit,
    /// Cannot be approved under the active policy.
    Forbidden,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequirement {
    pub level: ApprovalLevel,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectClass>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalPolicyOutcome {
    pub level: ApprovalLevel,
    pub requirements: Vec<ApprovalRequirement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlannedCheck {
    pub check: Check,
    pub task_level: bool,
    /// Where the check came from (`runtime`, `caller_expectation`,
    /// `interface:output_parameter`, ...).
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationProcedure {
    pub checks: Vec<PlannedCheck>,
    pub success_criteria: Vec<String>,
}

impl VerificationProcedure {
    pub fn has_task_level_checks(&self) -> bool {
        self.checks.iter().any(|c| c.task_level)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub schema_version: String,
    pub plan_id: String,
    pub request_id: String,
    pub created_at: String,
    pub intent: String,
    /// Ranked candidates, best first.
    pub candidates: Vec<Candidate>,
    /// Why the selected candidate was chosen (or why it was the only viable one).
    pub selection_reason: String,
    pub steps: Vec<Step>,
    pub constraints: Constraints,
    /// Timeout applied to each step, in seconds.
    pub timeout_secs: u64,
    pub effects: Effects,
    pub risk: Risk,
    pub approval: ApprovalPolicyOutcome,
    pub verification: VerificationProcedure,
    /// Summary of why the plan fits the intent.
    pub rationale: String,
    #[serde(default)]
    pub limitations: Vec<String>,
    /// SHA-256 over everything that determines what will run and how it is
    /// judged. Approvals are bound to this value.
    pub digest: String,
}

/// The subset of a plan an approval is bound to.
#[derive(Serialize)]
struct PlanBinding<'a> {
    plan_id: &'a str,
    request_id: &'a str,
    steps: &'a [Step],
    constraints: &'a Constraints,
    timeout_secs: u64,
    effects: &'a [EffectClass],
    effect_certainty: &'a crate::capability::EffectCertainty,
    approval_level: ApprovalLevel,
    checks: &'a [PlannedCheck],
}

impl Plan {
    /// Recompute the digest from the plan's current contents.
    pub fn compute_digest(&self) -> String {
        let binding = PlanBinding {
            plan_id: &self.plan_id,
            request_id: &self.request_id,
            steps: &self.steps,
            constraints: &self.constraints,
            timeout_secs: self.timeout_secs,
            effects: &self.effects.classes,
            effect_certainty: &self.effects.certainty,
            approval_level: self.approval.level,
            checks: &self.verification.checks,
        };
        let bytes = serde_json::to_vec(&binding).expect("plan binding serializes");
        format!("sha256:{}", hex::encode(Sha256::digest(&bytes)))
    }

    pub fn seal(&mut self) {
        self.digest = self.compute_digest();
    }

    pub fn is_intact(&self) -> bool {
        self.digest == self.compute_digest()
    }

    /// Short digest form shown to humans and accepted by `--approve`.
    pub fn short_digest(&self) -> String {
        short_digest(&self.digest)
    }
}

pub fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.chars().take(12).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Granted,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMethod {
    /// No approval was needed under the default policy.
    PolicyAutomatic,
    /// A person confirmed at an interactive prompt.
    Interactive,
    /// `--yes` was supplied (grants `confirm` level only).
    YesFlag,
    /// The caller presented the plan digest via `--approve`.
    DigestToken,
}

/// An approval decision, bound to one plan ID and digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRecord {
    pub schema_version: String,
    pub approval_id: String,
    pub plan_id: String,
    pub plan_digest: String,
    pub decision: ApprovalDecision,
    pub method: ApprovalMethod,
    /// The highest level this approval satisfies.
    pub level_granted: ApprovalLevel,
    pub decided_at: String,
}

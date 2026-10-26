//! Capability records: what the environment can do, with provenance.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::Check;

pub const CAPABILITY_SCHEMA_VERSION: &str = "cli2.capability.v1";

/// What sort of thing a capability is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// An executable file found on `PATH`.
    Executable,
    /// A task defined by a project manifest (npm script, Make target, ...).
    ProjectTask,
    /// A shell operation. Not produced by slice one discovery: built-ins are not
    /// represented because their invocation context cannot be captured accurately
    /// with an executable + argument vector.
    ShellOperation,
}

/// How much the system knows about a capability. These are knowledge states,
/// not safety ratings.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EnrichmentState {
    /// Name and location only.
    Discovered,
    /// Purpose inferred from metadata or help output.
    Described,
    /// Invocation constraints, inputs, outputs, or effects are characterized.
    Characterized,
    /// Real execution history exists for the current fingerprint.
    Observed,
}

/// A value together with where it came from and how much it is trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Provenanced<T> {
    pub value: T,
    /// Where the value came from, e.g. `package_manifest`, `help_output`,
    /// `heuristic:task_name`, `user_declaration`.
    pub source: String,
    /// 0.0 – 1.0.
    pub confidence: f64,
}

impl<T> Provenanced<T> {
    pub fn new(value: T, source: impl Into<String>, confidence: f64) -> Self {
        Self {
            value,
            source: source.into(),
            confidence,
        }
    }
}

/// Where discovery found a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySourceKind {
    Path,
    PackageManifest,
    Makefile,
    CargoManifest,
    Justfile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DiscoverySource {
    pub kind: DiscoverySourceKind,
    /// RFC 3339 timestamp of the scan that produced (or last confirmed) this record.
    pub discovered_at: String,
}

/// Physical location, kept distinct from logical identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    /// Absolute path of the executable or of the manifest that defines the task.
    pub path: String,
    /// Project root for project tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    /// Other executables on `PATH` with the same name that this one shadows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceCompleteness {
    /// Nothing is known beyond the executable name.
    Unknown,
    /// Parameters were inferred, e.g. from a usage line in help output.
    Inferred,
    /// The exact invocation is defined by a manifest or a user declaration.
    Declared,
    /// The invocation has been executed and observed.
    Observed,
}

/// A positional input of a capability's interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Parameter {
    pub name: String,
    pub required: bool,
    /// Whether the parameter can appear more than once (`<file>...`).
    #[serde(default)]
    pub variadic: bool,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Interface {
    /// Executable name (not path) to invoke. For project tasks this is the task
    /// runner (`npm`, `make`, ...).
    pub executable: String,
    /// Fixed leading arguments, e.g. `["run", "test"]`.
    pub arguments: Vec<String>,
    /// Positional parameters appended after `arguments`, in order.
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// Flags documented by the capability (never invented).
    #[serde(default)]
    pub known_flags: Vec<String>,
    pub completeness: InterfaceCompleteness,
    pub source: String,
}

/// Classes of effect an operation may have.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ReadOnly,
    ModifiesFiles,
    Deletes,
    /// Network access / external communication.
    Network,
    Publishes,
    Deploys,
    PrivilegeEscalation,
}

impl EffectClass {
    /// Effects the approval policy always treats as high impact.
    pub fn is_high_impact(self) -> bool {
        matches!(
            self,
            EffectClass::Deletes
                | EffectClass::Network
                | EffectClass::Publishes
                | EffectClass::Deploys
                | EffectClass::PrivilegeEscalation
        )
    }

    pub fn modifies_files(self) -> bool {
        matches!(
            self,
            EffectClass::ModifiesFiles
                | EffectClass::Deletes
                | EffectClass::Publishes
                | EffectClass::Deploys
        )
    }

    pub fn uses_network(self) -> bool {
        matches!(
            self,
            EffectClass::Network | EffectClass::Publishes | EffectClass::Deploys
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EffectClass::ReadOnly => "read_only",
            EffectClass::ModifiesFiles => "modifies_files",
            EffectClass::Deletes => "deletes",
            EffectClass::Network => "network",
            EffectClass::Publishes => "publishes",
            EffectClass::Deploys => "deploys",
            EffectClass::PrivilegeEscalation => "privilege_escalation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "read_only" | "readonly" | "read-only" => EffectClass::ReadOnly,
            "modifies_files" | "modify_files" | "writes" => EffectClass::ModifiesFiles,
            "deletes" | "delete" => EffectClass::Deletes,
            "network" => EffectClass::Network,
            "publishes" | "publish" => EffectClass::Publishes,
            "deploys" | "deploy" => EffectClass::Deploys,
            "privilege_escalation" => EffectClass::PrivilegeEscalation,
            _ => return None,
        })
    }
}

/// How firmly the effect assessment is established.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectCertainty {
    /// Nothing is known. Unknown effects are never treated as read-only.
    Unknown,
    /// Guessed from names or script bodies.
    Heuristic,
    /// Declared by a user (bound to the capability fingerprint).
    Declared,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Effects {
    /// Possible effects. Empty when certainty is `unknown`.
    pub classes: Vec<EffectClass>,
    pub certainty: EffectCertainty,
    pub source: String,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Effects {
    pub fn unknown(note: impl Into<String>) -> Self {
        Self {
            classes: vec![],
            certainty: EffectCertainty::Unknown,
            source: "none".into(),
            confidence: 0.0,
            notes: vec![note.into()],
        }
    }

    /// True only when the operation is known (declared) to be read-only.
    pub fn is_well_characterized_read_only(&self) -> bool {
        self.certainty >= EffectCertainty::Declared
            && !self.classes.is_empty()
            && self.classes.iter().all(|c| *c == EffectClass::ReadOnly)
    }
}

/// Bounded capture of help output, recorded only when explicitly requested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HelpCapture {
    pub captured_at: String,
    pub arguments: Vec<String>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    /// The first part of the help text.
    pub excerpt: String,
}

/// A user declaration about a capability. It is bound to the fingerprint at the
/// time of declaration and is discarded if the capability changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Characterization {
    pub fingerprint: String,
    pub declared_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects: Option<Vec<EffectClass>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Capability {
    pub schema_version: String,
    /// Stable logical ID, e.g. `executable:rg` or `project-script:npm:test`.
    pub id: String,
    pub kind: CapabilityKind,
    pub name: String,
    pub location: Location,
    pub source: DiscoverySource,
    /// Changes whenever the underlying executable or manifest entry changes.
    pub fingerprint: String,
    pub enrichment: EnrichmentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<Provenanced<String>>,
    pub interface: Interface,
    pub effects: Effects,
    /// Task-level checks known to be meaningful for this capability.
    #[serde(default)]
    pub postconditions: Vec<Provenanced<Check>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<HelpCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub characterization: Option<Characterization>,
}

impl Capability {
    /// Scope key used by the index: the project root for tasks, `env` otherwise.
    pub fn scope(&self) -> String {
        match &self.location.project_root {
            Some(root) if self.kind == CapabilityKind::ProjectTask => root.clone(),
            _ => "env".to_string(),
        }
    }
}

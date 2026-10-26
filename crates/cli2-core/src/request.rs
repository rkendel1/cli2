//! Intent requests.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::Check;
use crate::CoreError;

pub const REQUEST_SCHEMA_VERSION: &str = "cli2.request.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkConstraint {
    /// Network access is permitted.
    Allow,
    /// Network access is forbidden; plans that may use the network are rejected.
    Deny,
    /// No statement either way. Not interpreted as permission.
    Unknown,
}

/// Explicit constraints. Unspecified constraints are recorded as absent and are
/// never interpreted as permission.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modify_files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConstraint>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    /// Working directory, relative to the caller's directory or absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
}

/// The externally validated request contract (`cli2.request.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestInput {
    #[schemars(extend("const" = "cli2.request.v1"))]
    pub schema_version: String,
    #[schemars(length(min = 1))]
    pub intent: String,
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub context: RequestContext,
    /// Capability ID chosen by the caller among previously offered alternatives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    /// Values for named interface parameters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    /// Additional task-level checks the caller wants verified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expectations: Vec<Check>,
}

impl RequestInput {
    pub fn new(intent: impl Into<String>) -> Self {
        Self {
            schema_version: REQUEST_SCHEMA_VERSION.into(),
            intent: intent.into(),
            constraints: Constraints::default(),
            context: RequestContext::default(),
            selection: None,
            bindings: BTreeMap::new(),
            expectations: vec![],
        }
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != REQUEST_SCHEMA_VERSION {
            return Err(CoreError::InvalidRequest(format!(
                "unsupported schema_version {:?}; expected {REQUEST_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.intent.trim().is_empty() {
            return Err(CoreError::InvalidRequest("intent must be non-empty".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallerKind {
    /// A person at an interactive terminal.
    Interactive,
    /// A non-interactive caller (agent, script, pipe).
    NonInteractive,
}

/// Caller identity as established by the runtime, never taken from the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Caller {
    pub kind: CallerKind,
    /// Local OS user ID, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    pub established_by: String,
}

/// A request as recorded by the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordedRequest {
    pub schema_version: String,
    pub request_id: String,
    pub received_at: String,
    pub intent: String,
    pub constraints: Constraints,
    /// The resolved, absolute working directory.
    pub working_directory: String,
    pub caller: Caller,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expectations: Vec<Check>,
}

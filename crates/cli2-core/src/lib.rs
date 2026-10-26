//! CLI 2.0 contracts.
//!
//! These types are the boundary between the stages of the CLI 2.0 loop:
//! intent → discovery → resolution → authority/execution → verification/evidence.
//! Externally visible JSON contracts are versioned (`cli2.<name>.v1`) and their
//! JSON Schemas are generated from these types (see [`schemas`]).

pub mod capability;
pub mod check;
pub mod fileedit;
pub mod plan;
pub mod request;
pub mod result;

pub use capability::*;
pub use check::*;
pub use fileedit::*;
pub use plan::*;
pub use request::*;
pub use result::*;

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

/// Generate a runtime ID such as `req_3f2a...`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Current time as RFC 3339 with millisecond precision (UTC).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// RFC 3339 timestamp (same format as `now_rfc3339`) for `days` days ago.
pub fn rfc3339_days_ago(days: u64) -> String {
    let d = chrono::Duration::days(days.min(36_500) as i64);
    (chrono::Utc::now() - d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The published JSON Schemas, as `(file name, schema)`.
pub fn schemas() -> Vec<(&'static str, serde_json::Value)> {
    fn gen<T: schemars::JsonSchema>() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes")
    }
    vec![
        ("request.schema.json", gen::<RequestInput>()),
        ("capability.schema.json", gen::<Capability>()),
        ("plan.schema.json", gen::<Plan>()),
        ("result.schema.json", gen::<ExecutionResult>()),
        ("approval.schema.json", gen::<ApprovalRecord>()),
    ]
}

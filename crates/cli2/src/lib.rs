//! CLI 2.0 runtime: orchestration of discovery, resolution, authority,
//! execution and evidence. The `cli2` binary is a thin layer over [`Engine`].

pub mod engine;
pub mod render;
pub mod semantic;

pub use engine::{
    digest_matches, ApprovalAnswer, Config, Engine, EngineError, Interaction, NonInteractive,
    RunOptions, ScanReport, SemanticEvent, DEFAULT_TIMEOUT_SECS,
};

//! CLI 2.0 runtime: orchestration of discovery, resolution, authority,
//! execution and evidence. The `cli2` binary is a thin layer over [`Engine`].

pub mod batch;
pub mod clipboard;
pub mod engine;
pub mod eval;
pub mod output;
pub mod render;
pub mod semantic;
pub mod workflow;

pub use engine::{
    digest_matches, ApprovalAnswer, CaptureNote, Config, Engine, EngineError, Interaction,
    NonInteractive, PruneReport, RunOptions, ScanReport, SemanticEvent, DEFAULT_TIMEOUT_SECS,
};

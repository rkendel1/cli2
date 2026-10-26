//! Verification checks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A deterministic check evaluated by runtime code after execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Check {
    /// The process exit status equals `expected`. This is process-level evidence
    /// only: it never by itself establishes task-level success.
    ProcessExitStatus { expected: i32 },
    /// A file exists after execution (optionally modified after the step started).
    FileExists {
        path: String,
        #[serde(default)]
        modified_after_start: bool,
    },
    /// A file or directory exists after execution.
    PathExists { path: String },
    /// Captured standard output contains the given text.
    StdoutContains { text: String },
    /// After a file edit, the file read back from disk has exactly this
    /// content hash. It shows the edit landed as planned; it says nothing about
    /// whether the change achieves anything.
    FileSha256 { path: String, sha256: String },
}

impl Check {
    /// Whether the check says something about the task rather than the process.
    pub fn is_task_level(&self) -> bool {
        !matches!(self, Check::ProcessExitStatus { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Check::ProcessExitStatus { .. } => "process_exit_status",
            Check::FileExists { .. } => "file_exists",
            Check::PathExists { .. } => "path_exists",
            Check::StdoutContains { .. } => "stdout_contains",
            Check::FileSha256 { .. } => "file_sha256",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Check::ProcessExitStatus { expected } => {
                format!("process exits with status {expected}")
            }
            Check::FileExists {
                path,
                modified_after_start,
            } => {
                if *modified_after_start {
                    format!("file {path} exists and was written during execution")
                } else {
                    format!("file {path} exists")
                }
            }
            Check::PathExists { path } => format!("path {path} exists"),
            Check::StdoutContains { text } => format!("stdout contains {text:?}"),
            Check::FileSha256 { path, sha256 } => {
                format!("file {path} reads back with content hash {sha256}")
            }
        }
    }
}

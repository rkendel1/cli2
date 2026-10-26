//! Capability discovery.
//!
//! Discovery is pure inspection: it reads directory entries, file metadata and
//! manifest files. It never executes a discovered program. (Help capture is an
//! explicit, on-demand operation performed by the runtime; this crate only
//! parses the captured text — see [`help`].)

pub mod effects;
pub mod help;
pub mod path;
pub mod project;

pub use effects::{heuristic_effects_for_executable, heuristic_effects_for_task};
pub use help::{parse_help, ParsedHelp};
pub use path::{executable_fingerprint, scan_path, PathScan};
pub use project::{find_project_root, scan_project, ManifestInfo, ProjectScan};

/// The active shell and working directory, recorded with every scan.
///
/// Shell built-ins are deliberately not indexed as capabilities: slice one
/// executes an executable plus an argument vector without a shell, so a
/// built-in's invocation context cannot be represented accurately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellContext {
    pub shell: Option<String>,
    pub working_directory: String,
}

impl ShellContext {
    pub fn detect(working_directory: &std::path::Path) -> Self {
        Self {
            shell: std::env::var("SHELL").ok().filter(|s| !s.is_empty()),
            working_directory: working_directory.display().to_string(),
        }
    }
}

//! Captured results as first-class, retrievable data.
//!
//! Nothing here stores anything new. Every executed process is already
//! recorded by the journal (argv, working directory, termination, timing, and
//! content-addressed stdout/stderr blobs). This module gives those records
//! stable IDs and renders them:
//!
//! * **Execution ID** `exec_<n>` — the journal's result sequence number,
//!   which is never reused.
//! * **Output ID** `out_<n>` (step 0) or `out_<n>.<step>` — one executed step
//!   of that execution.
//!
//! Captured text is data. Nothing in this module executes it.

use std::fmt::Write as _;

use cli2_core::{
    shell_quote, ExecutionResult, StepResult, Termination, Verification, VerificationStatus,
};
use cli2_evidence::Journal;
use serde::{Deserialize, Serialize};

use crate::engine::EngineError;

pub const OUTPUT_SCHEMA: &str = "cli2.output.v1";
pub const OUTPUT_LIST_SCHEMA: &str = "cli2.output_list.v1";
/// Text embedded in JSON records per stream. Full content stays in evidence
/// and is available with `cli2 output copy`.
pub const INLINE_LIMIT: usize = 256 * 1024;

/// Captured bytes kept per stream per process (default 8 MiB).
pub const DEFAULT_MAX_BYTES: usize = cli2_execution::DEFAULT_MAX_OUTPUT;
pub const MIN_MAX_BYTES: usize = 1024;
pub const MAX_MAX_BYTES: usize = 256 * 1024 * 1024;

/// The `output` section of `$CLI2_HOME/config.json`:
/// `{ "output": { "max_bytes": 8388608 } }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputSettings {
    pub max_bytes: usize,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    max_bytes: Option<usize>,
}

impl OutputSettings {
    pub fn from_config_json(text: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut s = Self::default();
        if let Some(o) = v.get("output") {
            let raw: RawOutput =
                serde_json::from_value(o.clone()).map_err(|e| format!("output: {e}"))?;
            if let Some(n) = raw.max_bytes {
                if !(MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&n) {
                    return Err(format!(
                        "output.max_bytes {n} must be between {MIN_MAX_BYTES} and {MAX_MAX_BYTES}"
                    ));
                }
                s.max_bytes = n;
            }
        }
        Ok(s)
    }

    pub fn load(home: &std::path::Path) -> Result<Self, String> {
        let path = home.join("config.json");
        match std::fs::read_to_string(&path) {
            Ok(t) => Self::from_config_json(&t)
                .map_err(|e| format!("invalid config file {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputId {
    pub seq: i64,
    pub step: usize,
}

impl OutputId {
    pub fn format(self) -> String {
        if self.step == 0 {
            format!("out_{}", self.seq)
        } else {
            format!("out_{}.{}", self.seq, self.step)
        }
    }

    /// `out_17`, `out_17.2`, or `exec_17` (its last started step).
    /// `latest` is resolved by the caller.
    pub fn parse(s: &str) -> Option<ParsedId> {
        if let Some(rest) = s.strip_prefix("exec_") {
            return rest.parse().ok().map(ParsedId::Execution);
        }
        let rest = s.strip_prefix("out_")?;
        let (seq, step) = match rest.split_once('.') {
            Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
            None => (rest.parse().ok()?, 0),
        };
        Some(ParsedId::Output(OutputId { seq, step }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedId {
    Output(OutputId),
    Execution(i64),
}

/// Which representation to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Response,
    CommandAndResponse,
    Stdout,
    Stderr,
}

impl Part {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "response" => Some(Self::Response),
            "command-and-response" => Some(Self::CommandAndResponse),
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Command {
    /// The argv actually executed: element 0 is the resolved absolute path.
    pub argv: Vec<String>,
    /// Human display only (quoted so it can be read unambiguously). Never executed.
    pub display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stream {
    /// Bytes the process produced.
    pub bytes: u64,
    /// Bytes retained in evidence (≤ `bytes`).
    pub captured_bytes: u64,
    /// The retained content is shorter than what the process produced.
    pub truncated: bool,
    /// The content is no longer retained (pruned or damaged).
    pub available: bool,
    /// Content was not valid UTF-8; `text` has replacement characters.
    pub lossy: bool,
    /// `text` was cut to `INLINE_LIMIT` for this record; use `output copy` for all of it.
    pub inline_truncated: bool,
    pub evidence_ref: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Process {
    pub termination: Termination,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub pid: Option<u32>,
    /// The process exited with status 0. This is NOT task success.
    pub exited_zero: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskOutcome {
    /// The request's overall status (e.g. `succeeded`, `verification_failed`).
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputRecord {
    pub schema_version: String,
    pub output_id: String,
    pub execution_id: String,
    pub request_id: String,
    pub plan_id: Option<String>,
    pub step_index: usize,
    pub capability_id: String,
    pub command: Command,
    pub working_directory: String,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    /// What the process did.
    pub process: Process,
    pub stdout: Stream,
    pub stderr: Stream,
    /// Whether the task was accomplished is separate from the process status.
    pub task: TaskOutcome,
    /// Verification of the whole execution (it covers every step of the plan).
    pub verification: Verification,
    pub evidence_ref: String,
    /// Anything the user should know before trusting this record.
    pub notes: Vec<String>,
}

fn stream(
    journal: &Journal,
    reference: &Option<String>,
    total: u64,
    truncated: bool,
    inline: bool,
) -> Stream {
    let mut s = Stream {
        bytes: total,
        captured_bytes: 0,
        truncated,
        available: false,
        lossy: false,
        inline_truncated: false,
        evidence_ref: reference.clone(),
        text: None,
    };
    if let Some(r) = reference {
        if let Ok(bytes) = journal.read_blob(r) {
            s.available = true;
            s.captured_bytes = bytes.len() as u64;
            if inline {
                let cut = bytes.len().min(INLINE_LIMIT);
                s.inline_truncated = cut < bytes.len();
                let slice = &bytes[..cut];
                match std::str::from_utf8(slice) {
                    Ok(t) => s.text = Some(t.to_string()),
                    Err(_) => {
                        s.lossy = true;
                        s.text = Some(String::from_utf8_lossy(slice).into_owned());
                    }
                }
            } else {
                s.lossy = std::str::from_utf8(&bytes).is_err();
            }
        }
    }
    s
}

impl OutputRecord {
    pub fn build(
        journal: &Journal,
        seq: i64,
        result: &ExecutionResult,
        step: &StepResult,
        inline: bool,
    ) -> Self {
        let id = OutputId {
            seq,
            step: step.index,
        };
        let stdout = stream(
            journal,
            &step.stdout_ref,
            step.stdout_bytes,
            step.stdout_truncated,
            inline,
        );
        let stderr = stream(
            journal,
            &step.stderr_ref,
            step.stderr_bytes,
            step.stderr_truncated,
            inline,
        );
        let mut argv = vec![step.executable.clone()];
        argv.extend(step.arguments.iter().cloned());
        let mut notes = Vec::new();
        if stdout.truncated || stderr.truncated {
            notes.push(
                "Output was truncated when captured; the retained content is incomplete.".into(),
            );
        }
        if !stdout.available || !stderr.available {
            notes.push(
                "Some captured content is no longer retained (pruned); metadata is intact.".into(),
            );
        }
        if result.steps.len() > 1 {
            notes.push("Verification covers the whole execution, not this step alone.".into());
        }
        match (step.termination, result.verification.status) {
            (Termination::Exited, VerificationStatus::NotRun | VerificationStatus::Unverified)
                if step.exit_code == Some(0) =>
            {
                notes.push("A zero exit status shows the process completed; no task-level verification established that the goal was met.".into());
            }
            _ => {}
        }
        OutputRecord {
            schema_version: OUTPUT_SCHEMA.into(),
            output_id: id.format(),
            execution_id: format!("exec_{seq}"),
            request_id: result.request_id.clone(),
            plan_id: result.plan_id.clone(),
            step_index: step.index,
            capability_id: step.capability_id.clone(),
            command: Command {
                display: display_command(&argv),
                argv,
            },
            working_directory: step.working_directory.clone(),
            started_at: step.started_at.clone(),
            finished_at: step.finished_at.clone(),
            duration_ms: step.duration_ms,
            process: Process {
                termination: step.termination,
                exit_code: step.exit_code,
                signal: step.signal,
                pid: step.pid,
                exited_zero: step.process_succeeded(),
                error: step.error.clone(),
            },
            stdout,
            stderr,
            task: TaskOutcome {
                status: result.status.as_str().into(),
                message: result.message.clone(),
            },
            verification: result.verification.clone(),
            evidence_ref: result.evidence_ref.clone(),
            notes,
        }
    }
}

/// `rg authentication`: the program's file name and quoted arguments.
pub fn display_command(argv: &[String]) -> String {
    let mut parts = Vec::with_capacity(argv.len());
    for (i, a) in argv.iter().enumerate() {
        if i == 0 {
            let name = a.rsplit('/').next().unwrap_or(a);
            parts.push(shell_quote(name));
        } else {
            parts.push(shell_quote(a));
        }
    }
    parts.join(" ")
}

fn read(journal: &Journal, r: &Option<String>) -> Option<Vec<u8>> {
    r.as_deref().and_then(|r| journal.read_blob(r).ok())
}

/// The captured response: stdout, exactly as captured.
pub fn response_bytes(journal: &Journal, step: &StepResult) -> Result<Vec<u8>, EngineError> {
    read(journal, &step.stdout_ref)
        .ok_or_else(|| EngineError::NotFound("the captured stdout is no longer retained".into()))
}

pub fn stderr_bytes(journal: &Journal, step: &StepResult) -> Result<Vec<u8>, EngineError> {
    read(journal, &step.stderr_ref)
        .ok_or_else(|| EngineError::NotFound("the captured stderr is no longer retained".into()))
}

/// Command and response together, as text:
///
/// ```text
/// $ rg authentication
/// <stdout>
/// [stderr]            (only if there was any)
/// <stderr>
/// [exit 1]            (only if the process did not exit 0)
/// [output truncated: …]
/// ```
///
/// Markers are on their own lines so they are easy to strip. Never executed.
pub fn command_and_response(journal: &Journal, step: &StepResult) -> Result<String, EngineError> {
    let mut argv = vec![step.executable.clone()];
    argv.extend(step.arguments.iter().cloned());
    let mut out = String::new();
    let _ = writeln!(out, "$ {}", display_command(&argv));
    let stdout = response_bytes(journal, step)?;
    let text = String::from_utf8_lossy(&stdout);
    out.push_str(&text);
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
    let stderr = stderr_bytes(journal, step)?;
    if !stderr.is_empty() {
        out.push_str("[stderr]\n");
        let t = String::from_utf8_lossy(&stderr);
        out.push_str(&t);
        if !t.ends_with('\n') {
            out.push('\n');
        }
    }
    match (step.termination, step.exit_code) {
        (Termination::Exited, Some(0)) => {}
        (Termination::Exited, Some(c)) => {
            let _ = writeln!(out, "[exit {c}]");
        }
        (t, _) => {
            let _ = writeln!(out, "[{}]", termination_word(t, step.signal));
        }
    }
    if step.stdout_truncated {
        let _ = writeln!(
            out,
            "[stdout truncated: kept {} of {} bytes]",
            stdout.len(),
            step.stdout_bytes
        );
    }
    if step.stderr_truncated {
        let _ = writeln!(
            out,
            "[stderr truncated: kept {} of {} bytes]",
            stderr.len(),
            step.stderr_bytes
        );
    }
    Ok(out)
}

pub fn termination_word(t: Termination, signal: Option<i32>) -> String {
    match (t, signal) {
        (Termination::TimedOut, _) => "timed out".into(),
        (Termination::Cancelled, _) => "interrupted".into(),
        (Termination::SpawnFailed, _) => "could not start".into(),
        (Termination::Signaled, Some(s)) => format!("killed by signal {s}"),
        (Termination::Signaled, None) => "killed by a signal".into(),
        (Termination::NotStarted, _) => "not started".into(),
        (Termination::Exited, _) => "exited".into(),
        (Termination::Applied, _) => "applied".into(),
        (Termination::Refused, _) => "not applied".into(),
    }
}

/// Steps of a result that actually started a process.
pub fn started_steps(r: &ExecutionResult) -> impl Iterator<Item = &StepResult> {
    r.steps
        .iter()
        .filter(|s| s.termination != Termination::NotStarted)
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputSummary {
    pub output_id: String,
    pub execution_id: String,
    pub request_id: String,
    pub plan_id: Option<String>,
    pub command: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub termination: Termination,
    pub exit_code: Option<i32>,
    pub task_status: String,
    pub verification: VerificationStatus,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub truncated: bool,
}

pub fn summarize(seq: i64, r: &ExecutionResult, s: &StepResult) -> OutputSummary {
    let mut argv = vec![s.executable.clone()];
    argv.extend(s.arguments.iter().cloned());
    OutputSummary {
        output_id: OutputId { seq, step: s.index }.format(),
        execution_id: format!("exec_{seq}"),
        request_id: r.request_id.clone(),
        plan_id: r.plan_id.clone(),
        command: display_command(&argv),
        started_at: s.started_at.clone(),
        duration_ms: s.duration_ms,
        termination: s.termination,
        exit_code: s.exit_code,
        task_status: r.status.as_str().into(),
        verification: r.verification.status,
        stdout_bytes: s.stdout_bytes,
        stderr_bytes: s.stderr_bytes,
        truncated: s.stdout_truncated || s.stderr_truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        assert_eq!(OutputId { seq: 17, step: 0 }.format(), "out_17");
        assert_eq!(OutputId { seq: 17, step: 2 }.format(), "out_17.2");
        assert_eq!(
            OutputId::parse("out_17.2"),
            Some(ParsedId::Output(OutputId { seq: 17, step: 2 }))
        );
        assert_eq!(
            OutputId::parse("out_5"),
            Some(ParsedId::Output(OutputId { seq: 5, step: 0 }))
        );
        assert_eq!(OutputId::parse("exec_9"), Some(ParsedId::Execution(9)));
        for bad in ["out_", "out_x", "out_1.x", "17", "req_abc", ""] {
            assert_eq!(OutputId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn display_quotes_each_argument_unambiguously() {
        let argv: Vec<String> = ["/usr/bin/rg", "two words", "", "--flag", "it's"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            display_command(&argv),
            "rg 'two words' '' --flag 'it'\\''s'"
        );
    }
}

//! Orchestration of the CLI 2.0 loop:
//! intent → discovery/index → resolution → authority/execution → verification/evidence.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cli2_core::*;
use cli2_discovery::{
    find_project_root, parse_help, sanitize_help, scan_path, scan_project, ShellContext,
    PARSER_VERSION,
};
use cli2_evidence::{Journal, JournalError, Observation, StepObservation, TaskRecord, TaskSummary};
use cli2_execution::{
    assess, authorize, grant, AuthorizationError, CancelToken, Policy, ProcessSpec,
};
use cli2_index::{Index, IndexError, ReconcileReport, ENV_SCOPE};
use cli2_resolver::{
    resolve, HistoryStats, NoInference, Resolution, ResolveContext, SemanticRanker, SemanticReport,
};
use serde::Serialize;

use crate::semantic::{
    AppleFmRanker, Fallback, Provider, ResolverMode, ResolverSettings, PROVIDER_ID,
};

/// Default per-step timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
const HELP_TIMEOUT: Duration = Duration::from_secs(5);
const HELP_MAX_BYTES: usize = 64 * 1024;
const HELP_EXCERPT_CHARS: usize = 4000;
/// At most this many executables have `--help` run per request.
const MAX_HELP_CAPTURES_PER_REQUEST: usize = 3;

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the index, journal and evidence live.
    pub home: PathBuf,
    /// The `PATH` value to discover executables from.
    pub path_var: OsString,
    pub default_timeout_secs: u64,
    /// Bound on `<exe> --help` captures.
    pub help_timeout: Duration,
    pub policy: Policy,
    /// Captured bytes kept per stream per process; `None`: from config.json
    /// (`output.max_bytes`), else 8 MiB.
    pub max_output_bytes: Option<usize>,
    /// Resolver settings; when `None`, read from `$CLI2_HOME/config.json`
    /// (absent file means deterministic resolution only).
    pub resolver: Option<ResolverSettings>,
}

impl Config {
    /// Resolve the configuration from the environment:
    /// `CLI2_HOME`, else `$XDG_DATA_HOME/cli2`, else `~/.local/share/cli2`.
    pub fn from_env() -> Result<Self, EngineError> {
        let home = match std::env::var_os("CLI2_HOME").filter(|v| !v.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x).join("cli2"),
                None => {
                    let home = std::env::var_os("HOME").ok_or_else(|| {
                        EngineError::Invalid(
                            "cannot determine data directory: set CLI2_HOME".into(),
                        )
                    })?;
                    PathBuf::from(home).join(".local/share/cli2")
                }
            },
        };
        let policy_path = home.join("policy.json");
        let policy = if policy_path.exists() {
            let text = std::fs::read_to_string(&policy_path)?;
            serde_json::from_str(&text).map_err(|e| {
                EngineError::Invalid(format!(
                    "invalid policy file {}: {e}",
                    policy_path.display()
                ))
            })?
        } else {
            Policy::default()
        };
        Ok(Self {
            home,
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            default_timeout_secs: DEFAULT_TIMEOUT_SECS,
            help_timeout: HELP_TIMEOUT,
            policy,
            max_output_bytes: None,
            resolver: None,
        })
    }

    pub fn index_path(&self) -> PathBuf {
        self.home.join("index.db")
    }

    pub fn evidence_dir(&self) -> PathBuf {
        self.home.join("evidence")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    IndexCorrupt(String),
    #[error(transparent)]
    Index(IndexError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The runtime refused an operation for a stable, machine-readable reason
    /// (for example an approval that does not match the plan, or a stale plan).
    #[error("{message}")]
    Refused { code: &'static str, message: String },
    /// `--resolver apple-fm` was requested, the model could not be used, and
    /// no deterministic resolution could stand in for it.
    #[error("{0}")]
    SemanticUnavailable(String),
}

impl From<IndexError> for EngineError {
    fn from(e: IndexError) -> Self {
        match e {
            IndexError::Corrupt { .. } => EngineError::IndexCorrupt(e.to_string()),
            other => EngineError::Index(other),
        }
    }
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            EngineError::IndexCorrupt(_) => "index_corrupt",
            EngineError::Index(_) => "index_error",
            EngineError::Journal(JournalError::NotFound(_)) | EngineError::NotFound(_) => {
                "not_found"
            }
            EngineError::Journal(_) => "journal_error",
            EngineError::Invalid(_) => "invalid_input",
            EngineError::Io(_) => "io_error",
            EngineError::SemanticUnavailable(_) => "semantic_resolver_unavailable",
            EngineError::Refused { code, .. } => code,
        }
    }

    /// Documented process exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            EngineError::IndexCorrupt(_) => 9,
            EngineError::Invalid(_) | EngineError::NotFound(_) => 2,
            EngineError::SemanticUnavailable(_) => 11,
            EngineError::Refused { .. } => 6,
            EngineError::Journal(JournalError::NotFound(_)) => 2,
            _ => 10,
        }
    }

    pub fn remedy(&self) -> Option<&'static str> {
        match self {
            EngineError::IndexCorrupt(_) => {
                Some("run `cli2 scan --rebuild` to move the corrupt index aside and rebuild it from discovery")
            }
            EngineError::SemanticUnavailable(_) => {
                Some("run `cli2 model status` for details, or use `--resolver deterministic` / `--resolver auto`")
            }
            _ => None,
        }
    }
}

/// Answer from an interactive approver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    Granted,
    Denied,
    /// No one can be asked (non-interactive caller).
    Unavailable,
}

/// The interaction surface: preview before any execution decision, and
/// approval prompts.
pub trait Interaction {
    /// Called with every plan before the runtime decides whether to execute it.
    fn preview(&mut self, plan: &Plan);
    /// Ask for approval of this exact plan at its required level.
    fn ask(&mut self, plan: &Plan) -> ApprovalAnswer;
}

/// Non-interactive callers: nothing is shown and nobody can approve.
pub struct NonInteractive;

impl Interaction for NonInteractive {
    fn preview(&mut self, _: &Plan) {}
    fn ask(&mut self, _: &Plan) -> ApprovalAnswer {
        ApprovalAnswer::Unavailable
    }
}

/// Structured, ordered observations of an execution, for embedding clients.
/// Callbacks must not block; they report, they never control.
pub trait ExecutionObserver: Send + Sync {
    /// A step's process was started.
    fn step_started(&self, _step: &Step, _pid: u32) {}
    /// Output, as it is read (not subject to the capture limit).
    fn output(&self, _index: usize, _stream: cli2_execution::StreamKind, _chunk: &[u8]) {}
    fn step_finished(&self, _result: &StepResult) {}
    fn verification(&self, _verification: &Verification) {}
    /// The final, durably recorded result.
    fn finished(&self, _result: &ExecutionResult) {}
}

#[derive(Clone)]
pub struct EngineObserver(pub std::sync::Arc<dyn ExecutionObserver>);

impl std::fmt::Debug for EngineObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineObserver")
    }
}

struct StepObserver {
    step: Step,
    inner: std::sync::Arc<dyn ExecutionObserver>,
}

impl cli2_execution::ProcessObserver for StepObserver {
    fn spawned(&self, pid: u32) {
        self.inner.step_started(&self.step, pid);
    }
    fn output(&self, stream: cli2_execution::StreamKind, chunk: &[u8]) {
        self.inner.output(self.step.index(), stream, chunk);
    }
}

/// A request to modify one file; see [`Engine::plan_file_edit`].
#[derive(Debug, Clone)]
pub struct FileEditInput {
    pub project_root: String,
    /// Relative to `project_root`.
    pub path: String,
    /// Hash of the version the patch was prepared against; `None` creates the file.
    pub expected_sha256: Option<String>,
    pub hunks: Vec<Hunk>,
    pub intent: String,
    pub constraints: Constraints,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// An approval decision already recorded in the journal (by `approve_plan`).
    /// It is not trusted: `authorize` still checks that it is granted, bound to
    /// this plan's ID and exact digest, and sufficient for the required level.
    pub preapproved: Option<ApprovalRecord>,
    /// Observe the execution (status changes and output) as it happens.
    pub observer: Option<EngineObserver>,
    /// Execute the plan (false: plan only, `--no-exec`).
    pub execute: bool,
    /// `--yes`: approve plans whose required level is `confirm`.
    pub yes: bool,
    /// `--approve DIGEST`: explicit approval of the plan with this digest.
    pub approve_token: Option<String>,
    pub timeout_secs: Option<u64>,
    /// Allow capturing `--help` of an uncharacterized best match.
    pub capture_help: bool,
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub schema_version: &'static str,
    pub scanned_at: String,
    pub index_path: String,
    pub shell: Option<String>,
    pub working_directory: String,
    pub path_directories: Vec<String>,
    pub environment: ReconcileReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ReconcileReport>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuilt_from: Option<String>,
}

/// What `ensure_fresh` found and did.
struct Freshness {
    project_root: Option<String>,
    reports: Vec<ReconcileReport>,
    notes: Vec<String>,
}

pub struct Engine {
    pub config: Config,
    pub(crate) index: Index,
    pub(crate) journal: Journal,
    ranker: Box<dyn SemanticRanker>,
    cancel: CancelToken,
    settings: ResolverSettings,
    output_max_bytes: usize,
    /// `None`: legacy behaviour (run the installed ranker, single pass).
    mode: Option<ResolverMode>,
    semantic: Option<AppleFmRanker>,
    last_semantic: Option<SemanticEvent>,
}

/// The semantic-resolution evidence event: which resolver ordered the
/// candidates, what the model did, and whether a fallback occurred.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticEvent {
    pub schema_version: &'static str,
    pub resolver_mode: &'static str,
    /// `deterministic` or the provider that produced the final ordering.
    pub ordered_by: String,
    pub semantic_consulted: bool,
    pub fallback: bool,
    pub fallback_reason: Option<String>,
    pub report: Option<SemanticReport>,
}

/// What `prune_outputs` did (or would do, for a dry run).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    /// Executions older than the cutoff.
    pub executions: usize,
    pub blobs_removed: usize,
    pub bytes_freed: u64,
    /// Content kept because something newer still refers to it.
    pub kept_shared: usize,
}

/// What `capture_interface` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureNote {
    /// `--help` was run and parsed.
    Captured,
    /// A valid stored capture was used; nothing was run.
    Cached,
    /// A stored capture was re-parsed by the current parser; nothing was run.
    Reparsed,
}

/// A resolution plus how it was produced.
struct Resolved {
    resolution: Resolution,
    event: Option<SemanticEvent>,
    notes: Vec<String>,
}

/// A captured interface is only valid for the executable it was captured from.
/// Cheap check (one `stat` per captured executable): if the file on disk no
/// longer matches, the interface is dropped from this resolution and reported.
fn discard_stale_interfaces(caps: &mut [Capability]) -> Vec<String> {
    let mut notes = Vec::new();
    for c in caps.iter_mut() {
        if c.kind != CapabilityKind::Executable || c.interface.capture.is_none() {
            continue;
        }
        let live = cli2_discovery::executable_fingerprint(Path::new(&c.location.path)).ok();
        if live.as_deref() != Some(c.fingerprint.as_str()) {
            notes.push(format!(
                "`{}` changed on disk since its interface was captured; the captured interface was not used. Run `cli2 scan`, then capture it again.",
                c.name
            ));
            c.interface.parameters.clear();
            c.interface.options.clear();
            c.interface.known_flags.clear();
            c.interface.capture = None;
            c.interface.completeness = InterfaceCompleteness::Unknown;
            c.help = None;
        }
    }
    notes
}

fn output_limit(config: &Config) -> Result<usize, EngineError> {
    match config.max_output_bytes {
        Some(n) => Ok(n),
        None => crate::output::OutputSettings::load(&config.home)
            .map(|s| s.max_bytes)
            .map_err(EngineError::Invalid),
    }
}

fn settings_for(config: &Config) -> Result<ResolverSettings, EngineError> {
    match &config.resolver {
        Some(s) => Ok(s.clone()),
        None => ResolverSettings::load(&config.home).map_err(EngineError::Invalid),
    }
}

impl Engine {
    pub fn open(config: Config) -> Result<Self, EngineError> {
        let index = Index::open(&config.index_path())?;
        let journal = Journal::open(&config.evidence_dir())?;
        let settings = settings_for(&config)?;
        let output_max = output_limit(&config)?;
        let mut e = Self::assemble(config, index, journal, settings);
        e.output_max_bytes = output_max;
        Ok(e)
    }

    fn assemble(
        config: Config,
        index: Index,
        journal: Journal,
        settings: ResolverSettings,
    ) -> Self {
        let mode = settings.default_mode();
        let semantic = (settings.provider == Provider::AppleFoundationModels)
            .then(|| AppleFmRanker::new(&settings));
        Self {
            config,
            index,
            journal,
            ranker: Box::new(NoInference),
            cancel: CancelToken::new(),
            settings,
            output_max_bytes: crate::output::DEFAULT_MAX_BYTES,
            mode,
            semantic,
            last_semantic: None,
        }
    }

    /// Select the resolver for this engine (`--resolver`). Choosing
    /// `apple-fm` or `auto` enables the Apple provider even when the config
    /// file does not name one.
    pub fn set_resolver_mode(&mut self, mode: ResolverMode) {
        if mode != ResolverMode::Deterministic && self.semantic.is_none() {
            self.semantic = Some(AppleFmRanker::new(&self.settings));
        }
        self.mode = Some(mode);
    }

    /// Install a semantic ranker directly (tests, alternative bridges).
    pub fn set_semantic(&mut self, ranker: AppleFmRanker) {
        self.semantic = Some(ranker);
    }

    pub fn resolver_settings(&self) -> &ResolverSettings {
        &self.settings
    }

    /// The semantic-resolution evidence recorded by the most recent `run`.
    pub fn last_semantic(&self) -> Option<&SemanticEvent> {
        self.last_semantic.as_ref()
    }

    /// Move a corrupt index aside and start a fresh one. Evidence is untouched.
    pub fn open_rebuilding(config: Config) -> Result<(Self, Option<PathBuf>), EngineError> {
        let (index, moved) = Index::rebuild(&config.index_path())?;
        let journal = Journal::open(&config.evidence_dir())?;
        let settings = settings_for(&config)?;
        let output_max = output_limit(&config)?;
        let mut e = Self::assemble(config, index, journal, settings);
        e.output_max_bytes = output_max;
        Ok((e, moved))
    }

    /// Replace the semantic ranker (extension point; default: none).
    pub fn set_ranker(&mut self, ranker: Box<dyn SemanticRanker>) {
        self.ranker = ranker;
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    // ---------------------------------------------------------------- discovery

    pub fn scan(&mut self, cwd: &Path) -> Result<ScanReport, EngineError> {
        let now = now_rfc3339();
        let shell = ShellContext::detect(cwd);
        let env = self.scan_env(&now)?;
        let mut warnings = env.1;
        let (project_root, project) = match find_project_root(cwd) {
            Some(root) => {
                let (report, w) = self.scan_project_scope(&root, &now)?;
                warnings.extend(w);
                (Some(root.display().to_string()), Some(report))
            }
            None => (None, None),
        };
        self.journal.event(
            "system",
            "discovery_scan",
            &serde_json::json!({
                "working_directory": shell.working_directory,
                "environment": &env.0,
                "project": &project,
            }),
        )?;
        Ok(ScanReport {
            schema_version: "cli2.scan.v1",
            scanned_at: now,
            index_path: self.index.path().display().to_string(),
            shell: shell.shell,
            working_directory: shell.working_directory,
            path_directories: env.2,
            environment: env.0,
            project_root,
            project,
            warnings,
            rebuilt_from: None,
        })
    }

    fn scan_env(
        &mut self,
        now: &str,
    ) -> Result<(ReconcileReport, Vec<String>, Vec<String>), EngineError> {
        let scan = scan_path(&self.config.path_var, now);
        let mut lines: Vec<String> = scan
            .capabilities
            .iter()
            .map(|c| format!("{}={}@{}", c.id, c.fingerprint, c.location.path))
            .collect();
        lines.sort();
        let fp = format!("path:v1:{}", sha256_hex(lines.join("\n").as_bytes()));
        let report = self
            .index
            .reconcile_scope(ENV_SCOPE, &fp, scan.capabilities, now)?;
        Ok((report, scan.warnings, scan.directories))
    }

    fn scan_project_scope(
        &mut self,
        root: &Path,
        now: &str,
    ) -> Result<(ReconcileReport, Vec<String>), EngineError> {
        let scan = scan_project(root, now);
        let fp = scan.scope_fingerprint();
        let report = self
            .index
            .reconcile_scope(&scan.root, &fp, scan.capabilities, now)?;
        Ok((report, scan.warnings))
    }

    /// Make sure the index reflects the current project manifests (cheap: the
    /// manifests are hashed and compared with the stored scope fingerprint) and
    /// that the environment has been scanned at least once.
    fn ensure_fresh(&mut self, cwd: &Path) -> Result<Freshness, EngineError> {
        let now = now_rfc3339();
        let mut reports = Vec::new();
        let mut notes = Vec::new();
        if self.index.scope_info(ENV_SCOPE)?.is_none() {
            let (r, _, _) = self.scan_env(&now)?;
            notes.push(
                "The environment had not been scanned; PATH was scanned automatically.".into(),
            );
            reports.push(r);
        }
        let root = find_project_root(cwd);
        if let Some(root) = &root {
            let current = cli2_discovery::project::manifests_fingerprint(
                &cli2_discovery::project::probe_manifests(root),
            );
            let stored = self.index.scope_info(&root.display().to_string())?;
            if stored.map(|s| s.fingerprint) != Some(current) {
                let (r, _) = self.scan_project_scope(root, &now)?;
                if !r.is_noop() {
                    notes.push(format!(
                        "Project manifests in {} changed or were not indexed; project tasks were re-discovered ({} added, {} changed, {} removed).",
                        root.display(),
                        r.added.len(),
                        r.changed.len(),
                        r.removed.len()
                    ));
                }
                reports.push(r);
            }
        }
        Ok(Freshness {
            project_root: root.map(|r| r.display().to_string()),
            reports,
            notes,
        })
    }

    fn observations(&self) -> Result<BTreeMap<String, Observation>, EngineError> {
        Ok(self.journal.observations()?)
    }

    fn with_observations(&self, mut caps: Vec<Capability>) -> Result<Vec<Capability>, EngineError> {
        let obs = self.observations()?;
        for c in &mut caps {
            if let Some(o) = obs.get(&c.id) {
                if o.fingerprints.contains(&c.fingerprint) {
                    c.enrichment = EnrichmentState::Observed;
                }
            }
        }
        Ok(caps)
    }

    /// Capabilities visible from `cwd`: environment plus the current project.
    pub fn inventory(&mut self, cwd: &Path) -> Result<Vec<Capability>, EngineError> {
        let root = self.ensure_fresh(cwd)?.project_root;
        let caps = self.index.visible(root.as_deref())?;
        self.with_observations(caps)
    }

    pub fn get_capability(&mut self, id: &str, cwd: &Path) -> Result<Capability, EngineError> {
        let root = self.ensure_fresh(cwd)?.project_root;
        let cap = self
            .index
            .get(id, root.as_deref())?
            .ok_or_else(|| EngineError::NotFound(format!("no indexed capability with ID {id}")))?;
        Ok(self.with_observations(vec![cap])?.remove(0))
    }

    /// Obtain the interface of an executable from its own `--help` output
    /// (explicit, user-consented, bounded) and record it with provenance.
    ///
    /// Lifecycle rules (deterministic):
    /// * a parsed capture bound to the current fingerprint and parser version
    ///   is reused without running anything;
    /// * a capture made by an older parser is re-parsed from its saved output,
    ///   without running anything (re-run only if that output is gone);
    /// * a failed capture (timeout, no usable help, abnormal exit) is not
    ///   retried automatically; `refresh` forces a new attempt;
    /// * a changed executable has a new fingerprint, so its old interface was
    ///   already discarded by the index and this captures afresh;
    /// * a failure never leaves a trusted interface behind.
    pub fn capture_help(&mut self, id: &str, cwd: &Path) -> Result<Capability, EngineError> {
        self.capture_interface(id, cwd, false).map(|(c, _)| c)
    }

    /// Like [`capture_help`](Self::capture_help); also says what happened.
    pub fn capture_interface(
        &mut self,
        id: &str,
        cwd: &Path,
        refresh: bool,
    ) -> Result<(Capability, CaptureNote), EngineError> {
        let mut cap = self.get_capability(id, cwd)?;
        if cap.kind != CapabilityKind::Executable {
            return Err(EngineError::Invalid(format!(
                "{id} is a project task; its interface comes from its manifest"
            )));
        }
        let path = PathBuf::from(&cap.location.path);
        let current = cli2_discovery::executable_fingerprint(&path)?;
        if current != cap.fingerprint {
            return Err(EngineError::Invalid(format!(
                "{id} changed since it was indexed; run `cli2 scan` first"
            )));
        }
        if let Some(prev) = cap.interface.capture.clone() {
            if prev.executable_fingerprint == cap.fingerprint && !refresh {
                match prev.status {
                    CaptureStatus::Parsed if prev.parser_version == PARSER_VERSION => {
                        return Ok((cap, CaptureNote::Cached));
                    }
                    CaptureStatus::Parsed => {
                        if let Some(bytes) = prev
                            .output_ref
                            .as_deref()
                            .and_then(|r| self.journal.read_blob(r).ok())
                        {
                            let text = String::from_utf8_lossy(&bytes).to_string();
                            self.apply_capture(&mut cap, &text, prev.clone(), prev.status);
                            self.index.put(&cap)?;
                            return Ok((cap, CaptureNote::Reparsed));
                        }
                    }
                    failed => {
                        return Err(EngineError::Invalid(format!(
                            "a previous help capture of `{}` failed ({failed:?} at {}); not retrying automatically. Use `cli2 describe {id} --capture-help --refresh-help` to try again",
                            cap.name, prev.captured_at
                        )));
                    }
                }
            }
        }

        let method = vec!["--help".to_string()];
        // Help runs in cli2's own data directory, not the user's project.
        let mut spec = ProcessSpec::new(
            path.clone(),
            method.clone(),
            &self.config.home,
            self.config.help_timeout,
        );
        spec.max_output_bytes = HELP_MAX_BYTES;
        let out = cli2_execution::run(&spec, &self.cancel);
        let raw = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let text = String::from_utf8_lossy(raw).to_string();
        let output_ref = if raw.is_empty() {
            None
        } else {
            Some(self.journal.store_blob(raw)?)
        };
        self.journal.event(
            "system",
            "help_captured",
            &serde_json::json!({
                "capability_id": id,
                "fingerprint": cap.fingerprint,
                "method": method,
                "termination": out.termination,
                "exit_code": out.exit_code,
                "output_ref": output_ref,
                "parser_version": PARSER_VERSION,
            }),
        )?;
        let record = InterfaceCapture {
            status: CaptureStatus::Parsed,
            captured_at: now_rfc3339(),
            executable_path: cap.location.path.clone(),
            executable_fingerprint: cap.fingerprint.clone(),
            method,
            exit_code: out.exit_code,
            truncated: out.stdout_truncated() || out.stderr_truncated(),
            output_ref,
            parser_version: PARSER_VERSION,
            usage_line: None,
            alternate_usage_forms: 0,
            uncertainties: vec![],
        };
        let status = match out.termination {
            Termination::Exited if !text.trim().is_empty() => CaptureStatus::Parsed,
            Termination::Exited => CaptureStatus::NoUsableHelp,
            Termination::TimedOut => CaptureStatus::TimedOut,
            _ => CaptureStatus::Failed,
        };
        let status = self.apply_capture(&mut cap, &text, record, status);
        self.index.put(&cap)?;
        match status {
            CaptureStatus::Parsed => Ok((cap, CaptureNote::Captured)),
            other => Err(EngineError::Invalid(format!(
                "`{} --help` produced no usable help ({other:?}, exit {:?}); the interface stays unknown. The failure was recorded and will not be retried automatically",
                cap.name, out.exit_code
            ))),
        }
    }

    /// Replace the capability's captured interface with what `text` yields.
    /// Anything not understood stays unknown or partial; a failed capture
    /// leaves no previously-trusted interface behind.
    fn apply_capture(
        &self,
        cap: &mut Capability,
        text: &str,
        mut record: InterfaceCapture,
        status: CaptureStatus,
    ) -> CaptureStatus {
        cap.interface.parameters.clear();
        cap.interface.options.clear();
        cap.interface.known_flags.clear();
        cap.interface.completeness = InterfaceCompleteness::Unknown;
        cap.interface.source = "path".into();
        cap.help = None;
        let mut status = status;
        if status == CaptureStatus::Parsed {
            let parsed = parse_help(text, &cap.name);
            if !parsed.recognized_anything() {
                status = CaptureStatus::NoUsableHelp;
                record
                    .uncertainties
                    .push("the output was not recognizable as help text".into());
            } else {
                cap.help = Some(HelpCapture {
                    captured_at: record.captured_at.clone(),
                    arguments: record.method.clone(),
                    exit_code: record.exit_code,
                    truncated: record.truncated,
                    excerpt: sanitize_help(text)
                        .chars()
                        .take(HELP_EXCERPT_CHARS)
                        .collect(),
                });
                if let Some(p) = parsed.purpose.clone() {
                    if cap
                        .characterization
                        .as_ref()
                        .and_then(|c| c.purpose.as_ref())
                        .is_none()
                    {
                        cap.purpose = Some(Provenanced::new(p, "help_output", 0.6));
                    }
                }
                cap.enrichment = cap.enrichment.max(EnrichmentState::Described);
                record.usage_line = parsed.usage_line.clone();
                record.alternate_usage_forms = parsed.alternate_usage_forms;
                record.uncertainties = parsed.uncertainties.clone();
                cap.interface.known_flags = parsed.flags.clone();
                cap.interface.options = parsed.options.clone();
                if parsed.usage_complete {
                    cap.interface.parameters = parsed.parameters;
                    cap.interface.completeness = InterfaceCompleteness::Inferred;
                    cap.interface.source = "help_output:usage".into();
                    cap.enrichment = cap.enrichment.max(EnrichmentState::Characterized);
                } else if !cap.interface.options.is_empty() || !parsed.parameters.is_empty() {
                    cap.interface.completeness = InterfaceCompleteness::Partial;
                    cap.interface.source = "help_output:partial".into();
                }
            }
        } else {
            record
                .uncertainties
                .push(format!("capture ended with {status:?}"));
        }
        record.status = status;
        record.parser_version = PARSER_VERSION;
        cap.interface.capture = Some(record);
        status
    }

    /// Record a user declaration of effects (and optionally purpose). The
    /// declaration is bound to the current fingerprint.
    pub fn characterize(
        &mut self,
        id: &str,
        cwd: &Path,
        effects: Vec<EffectClass>,
        purpose: Option<String>,
    ) -> Result<Capability, EngineError> {
        if effects.is_empty() {
            return Err(EngineError::Invalid("declare at least one effect".into()));
        }
        if effects.contains(&EffectClass::ReadOnly) && effects.len() > 1 {
            return Err(EngineError::Invalid(
                "read_only cannot be combined with other effects".into(),
            ));
        }
        let mut cap = self.get_capability(id, cwd)?;
        cap.characterization = Some(Characterization {
            fingerprint: cap.fingerprint.clone(),
            declared_at: now_rfc3339(),
            effects: Some(effects.clone()),
            purpose: purpose.clone(),
        });
        cap.effects = Effects {
            classes: effects,
            certainty: EffectCertainty::Declared,
            source: "user_declaration".into(),
            confidence: 1.0,
            notes: vec!["declared by the user for this fingerprint".into()],
        };
        if let Some(p) = purpose {
            cap.purpose = Some(Provenanced::new(p, "user_declaration", 1.0));
        }
        cap.enrichment = cap.enrichment.max(EnrichmentState::Characterized);
        self.index.put(&cap)?;
        self.journal
            .event("system", "capability_characterized", &cap)?;
        Ok(cap)
    }

    // --------------------------------------------------------------- the loop

    /// Resolve an intent; then preview, authorize, execute and verify it
    /// (unless `opts.execute` is false).
    pub fn run(
        &mut self,
        input: RequestInput,
        opts: &RunOptions,
        ui: &mut dyn Interaction,
    ) -> Result<ExecutionResult, EngineError> {
        input
            .validate()
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let base = std::env::current_dir()?;
        let wd = match &input.context.working_directory {
            Some(w) => base.join(w),
            None => base,
        };
        let wd = wd.canonicalize().map_err(|e| {
            EngineError::Invalid(format!("working directory {}: {e}", wd.display()))
        })?;
        if !wd.is_dir() {
            return Err(EngineError::Invalid(format!(
                "{} is not a directory",
                wd.display()
            )));
        }
        let request = RecordedRequest {
            schema_version: REQUEST_SCHEMA_VERSION.into(),
            request_id: new_id("req"),
            received_at: now_rfc3339(),
            intent: input.intent.trim().to_string(),
            constraints: input.constraints.clone(),
            working_directory: wd.display().to_string(),
            caller: Caller {
                kind: if opts.interactive {
                    CallerKind::Interactive
                } else {
                    CallerKind::NonInteractive
                },
                #[cfg(unix)]
                // SAFETY: getuid has no failure modes.
                uid: Some(unsafe { libc::getuid() }),
                #[cfg(not(unix))]
                uid: None,
                established_by: "runtime:local_process".into(),
            },
            selection: input.selection.clone(),
            bindings: input.bindings.clone(),
            expectations: input.expectations.clone(),
        };
        self.journal.record_request(&request)?;

        let Freshness {
            project_root: root,
            reports,
            mut notes,
        } = self.ensure_fresh(&wd)?;
        if !reports.is_empty() {
            self.journal
                .event(&request.request_id, "index_reconciled", &reports)?;
        }

        self.last_semantic = None;
        let mut resolution = self.resolve_recorded(&request, root.as_deref(), &mut notes)?;
        if opts.capture_help {
            if let Resolution::Clarify { clarification, .. } = &resolution {
                // Consent covers `--help` of the few offered executables whose
                // interface is unknown and has not been attempted before.
                let mut targets = Vec::new();
                for o in &clarification.options {
                    if targets.len() == MAX_HELP_CAPTURES_PER_REQUEST {
                        break;
                    }
                    let cap = self.get_capability(&o.capability_id, &wd)?;
                    if cap.kind == CapabilityKind::Executable
                        && cap.interface.completeness == InterfaceCompleteness::Unknown
                        && cap.interface.capture.is_none()
                    {
                        targets.push(cap);
                    }
                }
                let mut captured_any = false;
                for cap in targets {
                    let id = cap.id.clone();
                    match self.capture_interface(&id, &wd, false) {
                        Ok(_) => {
                            captured_any = true;
                            notes.push(format!(
                                "Captured `{} --help` to learn the interface of {id} (requested with --capture-help).",
                                cap.name
                            ));
                        }
                        Err(e) => notes.push(format!("Help capture for {id} failed: {e}")),
                    }
                }
                if captured_any {
                    resolution = self.resolve_recorded(&request, root.as_deref(), &mut notes)?;
                }
            }
        }

        // Advice from verified history. It never influences what is planned.
        let planned_cap = match &resolution {
            Resolution::Planned(d) => Some(d.selected.id.clone()),
            Resolution::Clarify { .. } => None,
        };
        let advice = self.advise_history(&request.intent, &wd, planned_cap.as_deref());
        notes.extend(advice.notes);

        let draft = match resolution {
            Resolution::Clarify {
                candidates,
                clarification,
                limitations,
            } => {
                notes.extend(limitations);
                let result = ExecutionResult {
                    schema_version: RESULT_SCHEMA_VERSION.into(),
                    request_id: request.request_id.clone(),
                    plan_id: None,
                    status: Status::NeedsClarification,
                    process_status: ProcessStatus::NotRun,
                    steps: vec![],
                    verification: Verification::not_run(),
                    approval: None,
                    message: clarification.question.clone(),
                    clarification: Some(clarification),
                    rejection: vec![],
                    limitations: notes,
                    evidence_ref: evidence_ref(&request.request_id),
                    plan: None,
                };
                self.journal
                    .event(&request.request_id, "candidates_ranked", &candidates)?;
                self.journal.record_result(&result, None)?;
                return Ok(result);
            }
            Resolution::Planned(d) => d,
        };

        if let Some(cap) = &draft.interface_capture {
            self.journal.event(
                &request.request_id,
                "interface_provenance",
                &serde_json::json!({
                    "capability_id": draft.selected.id,
                    "interface_completeness": draft.selected.interface.completeness,
                    "capture": cap,
                    "arguments": draft.steps.first().and_then(|s| s.as_process()).map(|p| &p.arguments),
                }),
            )?;
        }
        let assessment = assess(&draft.effects, &request.constraints, &self.config.policy);
        let mut limitations = notes;
        limitations.extend(draft.limitations.iter().cloned());
        limitations.extend(assessment.limitations.iter().cloned());
        let mut plan = Plan {
            schema_version: PLAN_SCHEMA_VERSION.into(),
            plan_id: new_id("plan"),
            request_id: request.request_id.clone(),
            created_at: now_rfc3339(),
            intent: request.intent.clone(),
            candidates: draft.candidates.clone(),
            selection_reason: draft.selection_reason.clone(),
            steps: draft.steps.clone(),
            constraints: request.constraints.clone(),
            timeout_secs: opts
                .timeout_secs
                .unwrap_or(self.config.default_timeout_secs),
            effects: draft.effects.clone(),
            risk: assessment.risk,
            approval: assessment.approval.clone(),
            verification: draft.verification.clone(),
            rationale: draft.rationale.clone(),
            limitations: limitations.clone(),
            digest: String::new(),
        };
        plan.seal();
        self.journal.record_plan(&plan)?;
        ui.preview(&plan);

        if !assessment.violations.is_empty() {
            let result = self.finish_without_execution(
                &request,
                &plan,
                Status::Rejected,
                assessment.violations.clone(),
                None,
                "Rejected: the plan violates the request's constraints or the active policy."
                    .into(),
            )?;
            return Ok(result);
        }
        if !opts.execute {
            let result = self.finish_without_execution(
                &request,
                &plan,
                Status::Planned,
                vec![],
                None,
                format!(
                    "Plan {} created and not executed (approval required: {:?}); run `cli2 exec {}` to execute it.",
                    plan.plan_id, plan.approval.level, plan.plan_id
                ),
            )?;
            return Ok(result);
        }
        self.execute_plan(&request, plan, opts, ui)
    }

    /// Resolve, journal the semantic-resolution event, and surface notes.
    fn resolve_recorded(
        &mut self,
        request: &RecordedRequest,
        root: Option<&str>,
        notes: &mut Vec<String>,
    ) -> Result<Resolution, EngineError> {
        match self.resolve_request(request, root) {
            Ok(r) => {
                notes.extend(r.notes);
                if let Some(ev) = r.event {
                    self.journal
                        .event(&request.request_id, "semantic_resolution", &ev)?;
                    self.last_semantic = Some(ev);
                }
                Ok(r.resolution)
            }
            Err((e, ev)) => {
                if let Some(ev) = ev {
                    self.journal
                        .event(&request.request_id, "semantic_resolution", &ev)?;
                    self.last_semantic = Some(ev);
                }
                Err(e)
            }
        }
    }

    fn resolve_with(
        &self,
        request: &RecordedRequest,
        caps: &[Capability],
        root: Option<&str>,
        history: &BTreeMap<String, HistoryStats>,
        ranker: &dyn SemanticRanker,
        extra_terms: &[String],
    ) -> Resolution {
        resolve(&ResolveContext {
            request,
            capabilities: caps,
            project_root: root,
            history,
            ranker,
            extra_terms,
        })
    }

    #[allow(clippy::type_complexity, clippy::result_large_err)]
    fn resolve_request(
        &self,
        request: &RecordedRequest,
        root: Option<&str>,
    ) -> Result<Resolved, (EngineError, Option<SemanticEvent>)> {
        let wrap = |e: IndexError| (EngineError::from(e), None);
        let mut caps = self.index.visible(root).map_err(wrap)?;
        let stale_notes = discard_stale_interfaces(&mut caps);
        let history: BTreeMap<String, HistoryStats> = self
            .observations()
            .map_err(|e| (e, None))?
            .into_iter()
            .map(|(k, o)| {
                (
                    k,
                    HistoryStats {
                        successes: o.successes,
                        failures: o.failures,
                    },
                )
            })
            .collect();

        let Some(mode) = self.mode else {
            // Legacy: the installed ranker (default: none), one pass.
            let resolution =
                self.resolve_with(request, &caps, root, &history, self.ranker.as_ref(), &[]);
            return Ok(Resolved {
                resolution,
                event: self.ranker.take_report().map(|report| SemanticEvent {
                    schema_version: "cli2.semantic_event.v1",
                    resolver_mode: "custom",
                    ordered_by: self.ranker.name().to_string(),
                    semantic_consulted: true,
                    fallback: false,
                    fallback_reason: None,
                    report: Some(report),
                }),
                notes: stale_notes.clone(),
            });
        };

        let deterministic = || self.resolve_with(request, &caps, root, &history, &NoInference, &[]);
        if mode == ResolverMode::Deterministic {
            return Ok(Resolved {
                resolution: deterministic(),
                event: Some(SemanticEvent {
                    schema_version: "cli2.semantic_event.v1",
                    resolver_mode: mode.as_str(),
                    ordered_by: "deterministic".into(),
                    semantic_consulted: false,
                    fallback: false,
                    fallback_reason: None,
                    report: None,
                }),
                notes: stale_notes.clone(),
            });
        }

        // Deterministic first. In `auto`, a plan it can build is final.
        let det = deterministic();
        if mode == ResolverMode::Auto && matches!(det, Resolution::Planned(_)) {
            return Ok(Resolved {
                resolution: det,
                event: Some(SemanticEvent {
                    schema_version: "cli2.semantic_event.v1",
                    resolver_mode: mode.as_str(),
                    ordered_by: "deterministic".into(),
                    semantic_consulted: false,
                    fallback: false,
                    fallback_reason: None,
                    report: None,
                }),
                notes: stale_notes.clone(),
            });
        }

        // Semantic pass.
        let attempt: Result<(Resolution, SemanticReport), (String, Option<SemanticReport>)> =
            match &self.semantic {
                None => Err((
                    "no semantic provider is configured (set resolver.provider in config.json)"
                        .into(),
                    None,
                )),
                Some(sem) => match sem.expand_terms(request) {
                    Err(e) => Err((e, sem.take_report())),
                    Ok(terms) => {
                        let res = self.resolve_with(request, &caps, root, &history, sem, &terms);
                        match sem.take_report() {
                            Some(rep) if rep.failed() => Err((
                                rep.error.clone().unwrap_or_else(|| "ranking failed".into()),
                                Some(rep),
                            )),
                            Some(rep) => Ok((res, rep)),
                            // An exact match decided the request; the model was not needed.
                            None => Ok((
                                res,
                                SemanticReport {
                                    schema_version: SemanticReport::SCHEMA,
                                    provider: PROVIDER_ID.into(),
                                    bridge_version: None,
                                    bridge_request_id: String::new(),
                                    expansion_terms: terms,
                                    submitted: vec![],
                                    order_before: vec![],
                                    order_after: vec![],
                                    status: "not_needed".into(),
                                    validation: "not_run".into(),
                                    rankings: vec![],
                                    clarification: None,
                                    availability: None,
                                    latency_ms: 0,
                                    error: None,
                                },
                            )),
                        }
                    }
                },
            };

        match attempt {
            Ok((resolution, report)) => {
                let ordered_by = if report.status == "not_needed" {
                    "deterministic"
                } else {
                    PROVIDER_ID
                };
                let mut notes = stale_notes.clone();
                if ordered_by == PROVIDER_ID {
                    notes.push(format!(
                        "Candidate ordering was produced with {PROVIDER_ID} (on-device) over {} retrieved candidates; the runtime validated the ranking and built any plan.",
                        report.submitted.len()
                    ));
                }
                Ok(Resolved {
                    resolution,
                    event: Some(SemanticEvent {
                        schema_version: "cli2.semantic_event.v1",
                        resolver_mode: mode.as_str(),
                        ordered_by: ordered_by.into(),
                        semantic_consulted: true,
                        fallback: false,
                        fallback_reason: None,
                        report: Some(report),
                    }),
                    notes,
                })
            }
            Err((why, report)) => {
                let event = SemanticEvent {
                    schema_version: "cli2.semantic_event.v1",
                    resolver_mode: mode.as_str(),
                    ordered_by: "deterministic".into(),
                    semantic_consulted: true,
                    fallback: true,
                    fallback_reason: Some(why.clone()),
                    report,
                };
                let may_fall_back = mode == ResolverMode::Auto
                    || (self.settings.fallback == Fallback::Deterministic
                        && matches!(det, Resolution::Planned(_)));
                if may_fall_back {
                    Ok(Resolved {
                        resolution: det,
                        event: Some(event),
                        notes: {
                            let mut n = stale_notes.clone();
                            n.push(format!(
                                "Semantic ranking with {PROVIDER_ID} was not used ({why}); the deterministic resolver produced the candidate ordering."
                            ));
                            n
                        },
                    })
                } else {
                    Err((
                        EngineError::SemanticUnavailable(format!(
                            "semantic resolution with {PROVIDER_ID} failed ({why}) and the deterministic resolver cannot resolve {:?} on its own",
                            request.intent
                        )),
                        Some(event),
                    ))
                }
            }
        }
    }

    /// Execute a previously recorded plan (e.g. after reviewing `cli2 plan`).
    pub fn exec_stored(
        &mut self,
        plan_id: &str,
        opts: &RunOptions,
        ui: &mut dyn Interaction,
    ) -> Result<ExecutionResult, EngineError> {
        let plan = self.journal.plan(plan_id)?;
        let request = self.journal.request(&plan.request_id)?;
        ui.preview(&plan);
        if !plan.is_intact() {
            return self.finish_without_execution(
                &request,
                &plan,
                Status::Rejected,
                vec!["the stored plan no longer matches its digest; it was modified after it was created".into()],
                None,
                "Rejected: stored plan was modified".into(),
            );
        }
        self.execute_plan(&request, plan, opts, ui)
    }

    pub(crate) fn finish_without_execution(
        &mut self,
        request: &RecordedRequest,
        plan: &Plan,
        status: Status,
        rejection: Vec<String>,
        approval: Option<ApprovalSummary>,
        message: String,
    ) -> Result<ExecutionResult, EngineError> {
        let approval = approval.unwrap_or_else(|| ApprovalSummary {
            required: plan.approval.level,
            granted: false,
            method: None,
            approval_id: None,
            plan_digest: plan.digest.clone(),
            reasons: plan
                .approval
                .requirements
                .iter()
                .map(|r| r.reason.clone())
                .collect(),
        });
        let result = ExecutionResult {
            schema_version: RESULT_SCHEMA_VERSION.into(),
            request_id: request.request_id.clone(),
            plan_id: Some(plan.plan_id.clone()),
            status,
            process_status: ProcessStatus::NotRun,
            steps: vec![],
            verification: Verification::not_run(),
            approval: Some(approval),
            clarification: None,
            rejection,
            limitations: plan.limitations.clone(),
            evidence_ref: evidence_ref(&request.request_id),
            message,
            plan: Some(plan.clone()),
        };
        self.journal.record_result(&result, None)?;
        Ok(result)
    }

    /// Check that what the plan references is still what exists.
    pub(crate) fn stale_reasons(&mut self, plan: &Plan) -> Vec<String> {
        let mut reasons = Vec::new();
        for step in &plan.steps {
            let step = match step {
                Step::Process(p) => p,
                Step::FileEdit(f) => {
                    file_edit_stale(f, &mut reasons);
                    continue;
                }
            };
            match cli2_discovery::executable_fingerprint(Path::new(&step.resolved_executable)) {
                Ok(fp) if fp == step.executable_fingerprint => {}
                Ok(_) => reasons.push(format!(
                    "{} changed since the plan was made",
                    step.resolved_executable
                )),
                Err(e) => reasons.push(format!(
                    "{} is no longer available: {e}",
                    step.resolved_executable
                )),
            }
            if step.capability_id.starts_with("executable:") {
                continue;
            }
            let root = Path::new(&step.working_directory);
            let scan = scan_project(root, &now_rfc3339());
            match scan
                .capabilities
                .iter()
                .find(|c| c.id == step.capability_id)
            {
                Some(c) if c.fingerprint == step.capability_fingerprint => {}
                Some(_) => reasons.push(format!(
                    "{} was redefined in its manifest since the plan was made",
                    step.capability_id
                )),
                None => reasons.push(format!("{} no longer exists", step.capability_id)),
            }
        }
        reasons
    }

    fn execute_plan(
        &mut self,
        request: &RecordedRequest,
        plan: Plan,
        opts: &RunOptions,
        ui: &mut dyn Interaction,
    ) -> Result<ExecutionResult, EngineError> {
        // 1. Freshness: never run a plan against capabilities that changed.
        let stale = self.stale_reasons(&plan);
        if !stale.is_empty() {
            let wd = PathBuf::from(&request.working_directory);
            let _ = self.ensure_fresh(&wd);
            let mut reasons = stale;
            reasons.push("the plan is stale; resolve the intent again to get a new plan (and a new approval)".into());
            return self.finish_without_execution(
                request,
                &plan,
                Status::Rejected,
                reasons,
                None,
                "Rejected: the environment changed since this plan was made".into(),
            );
        }

        // 2. Authority.
        let required = plan.approval.level;
        let mut approval: Option<ApprovalRecord> = None;
        let mut preapproved_used = false;
        let mut denial_note: Option<String> = None;
        match required {
            ApprovalLevel::Forbidden => {
                let reasons = plan
                    .approval
                    .requirements
                    .iter()
                    .filter(|r| r.level == ApprovalLevel::Forbidden)
                    .map(|r| r.reason.clone())
                    .collect();
                return self.finish_without_execution(
                    request,
                    &plan,
                    Status::Rejected,
                    reasons,
                    None,
                    "Rejected: this operation cannot be approved under the active policy".into(),
                );
            }
            ApprovalLevel::None => {
                approval = Some(grant(
                    &plan,
                    ApprovalMethod::PolicyAutomatic,
                    ApprovalDecision::Granted,
                ));
            }
            ApprovalLevel::Confirm | ApprovalLevel::Explicit => {
                if let Some(pre) = &opts.preapproved {
                    approval = Some(pre.clone());
                    preapproved_used = true;
                } else if let Some(token) = &opts.approve_token {
                    if digest_matches(token, &plan.digest) {
                        approval = Some(grant(
                            &plan,
                            ApprovalMethod::DigestToken,
                            ApprovalDecision::Granted,
                        ));
                    } else {
                        denial_note = Some(format!(
                            "the --approve token does not match this plan's digest ({}); approvals apply only to the exact plan presented",
                            plan.short_digest()
                        ));
                    }
                } else if opts.yes && required == ApprovalLevel::Confirm {
                    approval = Some(grant(
                        &plan,
                        ApprovalMethod::YesFlag,
                        ApprovalDecision::Granted,
                    ));
                } else {
                    if opts.yes {
                        denial_note = Some(
                            "--yes does not approve high-impact operations; explicit approval of this plan is required".into(),
                        );
                    }
                    match ui.ask(&plan) {
                        ApprovalAnswer::Granted => {
                            approval = Some(grant(
                                &plan,
                                ApprovalMethod::Interactive,
                                ApprovalDecision::Granted,
                            ));
                            denial_note = None;
                        }
                        ApprovalAnswer::Denied => {
                            let rec =
                                grant(&plan, ApprovalMethod::Interactive, ApprovalDecision::Denied);
                            self.journal.record_approval(&request.request_id, &rec)?;
                            let summary = summary_from(&plan, Some(&rec), false);
                            return self.finish_without_execution(
                                request,
                                &plan,
                                Status::Cancelled,
                                vec![],
                                Some(summary),
                                "Cancelled: approval was declined".into(),
                            );
                        }
                        ApprovalAnswer::Unavailable => {}
                    }
                }
            }
        }
        if let Some(a) = approval.as_ref().filter(|_| !preapproved_used) {
            self.journal.record_approval(&request.request_id, a)?;
        }
        if let Err(err) = authorize(&plan, approval.as_ref()) {
            let (status, rejection) = match err {
                AuthorizationError::Forbidden | AuthorizationError::PlanModified => {
                    (Status::Rejected, vec![err.to_string()])
                }
                _ => (Status::AwaitingApproval, vec![]),
            };
            let mut message = format!(
                "Awaiting approval ({}): {}. To approve this exact plan run `cli2 exec {} --approve {}`{}.",
                level_name(required),
                plan.approval
                    .requirements
                    .iter()
                    .map(|r| r.reason.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
                plan.plan_id,
                plan.short_digest(),
                if required == ApprovalLevel::Confirm { " (or re-run with --yes)" } else { "" }
            );
            if let Some(n) = &denial_note {
                message = format!("{message} Note: {n}.");
            }
            if status == Status::Rejected {
                message = format!("Rejected: {err}");
            }
            let summary = summary_from(&plan, approval.as_ref(), false);
            return self.finish_without_execution(
                request,
                &plan,
                status,
                rejection,
                Some(summary),
                message,
            );
        }

        // 3. Execution.
        let mut step_results: Vec<StepResult> = Vec::new();
        let mut outcomes: Vec<(std::time::SystemTime, Vec<u8>)> = Vec::new();
        let mut stopped = false;
        for planned in &plan.steps {
            if stopped {
                step_results.push(not_started(planned));
                continue;
            }
            self.journal.event(
                &request.request_id,
                "step_started",
                &serde_json::json!({
                    "plan_id": plan.plan_id,
                    "step": planned,
                }),
            )?;
            let step = match planned {
                Step::Process(p) => p,
                Step::FileEdit(f) => {
                    let sr = self.apply_file_edit(f)?;
                    self.journal
                        .event(&request.request_id, "step_finished", &sr)?;
                    if let Some(o) = &opts.observer {
                        o.0.step_finished(&sr);
                    }
                    if !sr.process_succeeded() {
                        stopped = true;
                    }
                    outcomes.push((std::time::SystemTime::now(), Vec::new()));
                    step_results.push(sr);
                    continue;
                }
            };
            let mut spec = ProcessSpec::new(
                &step.resolved_executable,
                step.arguments.clone(),
                &step.working_directory,
                Duration::from_secs(plan.timeout_secs),
            );
            spec.max_output_bytes = self.output_max_bytes;
            if let Some(o) = &opts.observer {
                spec.observer = Some(cli2_execution::Observer(std::sync::Arc::new(
                    StepObserver {
                        step: planned.clone(),
                        inner: o.0.clone(),
                    },
                )));
            }
            let out = cli2_execution::run(&spec, &self.cancel);
            let stdout_ref = self.journal.store_blob(&out.stdout)?;
            let stderr_ref = self.journal.store_blob(&out.stderr)?;
            let sr = StepResult {
                index: step.index,
                capability_id: step.capability_id.clone(),
                executable: step.resolved_executable.clone(),
                arguments: step.arguments.clone(),
                working_directory: step.working_directory.clone(),
                termination: out.termination,
                exit_code: out.exit_code,
                signal: out.signal,
                pid: out.pid,
                started_at: out.started_at.clone(),
                finished_at: out.finished_at.clone(),
                duration_ms: out.duration_ms,
                stdout_ref: Some(stdout_ref),
                stderr_ref: Some(stderr_ref),
                stdout_bytes: out.stdout_total,
                stderr_bytes: out.stderr_total,
                stdout_truncated: out.stdout_truncated(),
                stderr_truncated: out.stderr_truncated(),
                error: out.error.clone(),
                file_edit: None,
            };
            self.journal
                .event(&request.request_id, "step_finished", &sr)?;
            if let Some(o) = &opts.observer {
                o.0.step_finished(&sr);
            }
            if !sr.process_succeeded() {
                stopped = true;
            }
            step_results.push(sr);
            outcomes.push((out.started, out.stdout));
        }

        // 4. Verification.
        let observations: Vec<StepObservation<'_>> = step_results
            .iter()
            .zip(outcomes.iter())
            .map(|(r, (started, stdout))| StepObservation {
                result: r,
                started: *started,
                stdout,
            })
            .collect();
        let verification = cli2_evidence::verify(&plan.verification, &observations);
        self.journal
            .event(&request.request_id, "verification_completed", &verification)?;
        if let Some(o) = &opts.observer {
            o.0.verification(&verification);
        }

        let ran: Vec<&StepResult> = step_results
            .iter()
            .filter(|s| s.termination != Termination::NotStarted)
            .collect();
        let all_ok = !ran.is_empty()
            && ran.len() == plan.steps.len()
            && ran.iter().all(|s| s.process_succeeded());
        let any_ok = ran.iter().any(|s| s.process_succeeded());
        let cancelled = ran.iter().any(|s| s.termination == Termination::Cancelled);
        let status = if cancelled {
            Status::Cancelled
        } else if all_ok {
            if verification.status == VerificationStatus::Failed {
                Status::VerificationFailed
            } else {
                Status::Succeeded
            }
        } else if any_ok {
            Status::PartiallySucceeded
        } else {
            Status::Failed
        };
        let process_status = if all_ok {
            ProcessStatus::Succeeded
        } else {
            ProcessStatus::Failed
        };
        let last = ran.last();
        let message = match status {
            Status::Succeeded if plan.steps.iter().all(|s| s.as_file_edit().is_some()) => {
                "Succeeded: the file was edited and read back as planned. This shows the change was made, not that it achieves anything; run the project's checks to find out.".to_string()
            }
            Status::Succeeded if verification.status == VerificationStatus::Passed => {
                "Succeeded: the process completed and task-level verification passed.".to_string()
            }
            Status::Succeeded => {
                "Succeeded (unverified): the process exited 0, but no task-level postcondition could be checked.".to_string()
            }
            Status::VerificationFailed => {
                "Verification failed: the process exited 0, but a task-level postcondition did not hold.".to_string()
            }
            Status::Cancelled => "Cancelled: execution was interrupted and the process was terminated.".to_string(),
            _ => match last {
                Some(s) if s.file_edit.is_some() => format!(
                    "Failed: the file was not edited as planned ({}).",
                    s.file_edit.as_ref().map(|o| o.message.clone()).unwrap_or_default()
                ),
                Some(s) if s.termination == Termination::TimedOut => format!(
                    "Failed: the process timed out after {}s and was terminated.",
                    plan.timeout_secs
                ),
                Some(s) if s.termination == Termination::SpawnFailed => format!(
                    "Failed: the process could not be started ({}).",
                    s.error.clone().unwrap_or_default()
                ),
                Some(s) => format!(
                    "Failed: the process ended with {}.",
                    match (s.exit_code, s.signal) {
                        (Some(c), _) => format!("exit status {c}"),
                        (None, Some(sig)) => format!("signal {sig}"),
                        _ => "an unknown status".to_string(),
                    }
                ),
                None => "Failed: nothing was executed.".to_string(),
            },
        };
        let mut limitations = plan.limitations.clone();
        if verification.status == VerificationStatus::Unverified {
            limitations.push("Task-level success was not independently established.".into());
        }
        let result = ExecutionResult {
            schema_version: RESULT_SCHEMA_VERSION.into(),
            request_id: request.request_id.clone(),
            plan_id: Some(plan.plan_id.clone()),
            status,
            process_status,
            steps: step_results,
            verification,
            approval: Some(summary_from(&plan, approval.as_ref(), true)),
            clarification: None,
            rejection: vec![],
            limitations,
            evidence_ref: evidence_ref(&request.request_id),
            message,
            plan: Some(plan.clone()),
        };
        self.journal.record_result(&result, Some(&plan))?;
        if let Some(o) = &opts.observer {
            o.0.finished(&result);
        }
        Ok(result)
    }

    /// Commit a file-edit step and describe, as evidence, what happened.
    fn apply_file_edit(&mut self, f: &FileEditStep) -> Result<StepResult, EngineError> {
        let started_at = now_rfc3339();
        let t0 = std::time::Instant::now();
        let cancelled = self.cancel.is_cancelled();
        let outcome = if cancelled {
            FileEditOutcome {
                status: FileEditStatus::Refused,
                path: f.path.clone(),
                root: f.root.clone(),
                expected_sha256: f.expected_sha256.clone(),
                before_sha256: None,
                planned_sha256: f.result_sha256.clone(),
                after_sha256: None,
                bytes_before: None,
                bytes_after: None,
                message: "execution was cancelled before the edit was applied; nothing was written"
                    .into(),
            }
        } else {
            cli2_execution::fsedit::apply(f)
        };
        // The exact change that was approved is kept as the step's stdout.
        let diff = f.unified_diff().into_bytes();
        let note = if outcome.applied() {
            Vec::new()
        } else {
            outcome.message.clone().into_bytes()
        };
        let stdout_ref = self.journal.store_blob(&diff)?;
        let stderr_ref = self.journal.store_blob(&note)?;
        Ok(StepResult {
            index: f.index,
            capability_id: f.capability_id.clone(),
            executable: f.capability_id.clone(),
            arguments: vec![f.path.clone()],
            working_directory: f.root.clone(),
            termination: if outcome.applied() {
                Termination::Applied
            } else if cancelled {
                Termination::Cancelled
            } else {
                Termination::Refused
            },
            exit_code: None,
            signal: None,
            pid: None,
            started_at,
            finished_at: now_rfc3339(),
            duration_ms: t0.elapsed().as_millis() as u64,
            stdout_ref: Some(stdout_ref),
            stderr_ref: Some(stderr_ref),
            stdout_bytes: diff.len() as u64,
            stderr_bytes: note.len() as u64,
            stdout_truncated: false,
            stderr_truncated: false,
            error: (!outcome.applied()).then(|| outcome.message.clone()),
            file_edit: Some(Box::new(outcome)),
        })
    }

    /// Plan a modification of one file. The patch is checked against the file
    /// as it is now, the result is computed, and everything that determines
    /// the change goes into the plan the approval will be bound to. Nothing is
    /// written. Whatever the request's constraints say, a file edit never runs
    /// without at least a confirmation.
    pub fn plan_file_edit(
        &mut self,
        input: FileEditInput,
        opts: &RunOptions,
        ui: &mut dyn Interaction,
    ) -> Result<ExecutionResult, EngineError> {
        use cli2_execution::fsedit::{read_file, ReadError};
        let invalid = |m: String| EngineError::Invalid(m);
        let root = PathBuf::from(&input.project_root)
            .canonicalize()
            .map_err(|e| invalid(format!("project root {}: {e}", input.project_root)))?;
        if !root.is_dir() {
            return Err(invalid(format!("{} is not a directory", root.display())));
        }
        let path = normalize_rel_path(&input.path).map_err(invalid)?;
        validate_hunks(&input.hunks).map_err(invalid)?;
        if let Some(h) = &input.expected_sha256 {
            if !h.starts_with("sha256:") {
                return Err(invalid("expected_sha256 must be `sha256:<hex>`".into()));
            }
        }
        let current = match read_file(&root, &path) {
            Ok(snap) => Some(snap),
            Err(ReadError::Missing(_)) => None,
            Err(ReadError::Refused(m)) => return Err(invalid(m)),
        };
        match (&input.expected_sha256, &current) {
            (Some(h), Some(c)) if &c.sha256 == h => {}
            (Some(_), Some(c)) => {
                return Err(EngineError::Refused {
                    code: "stale_patch",
                    message: format!(
                        "{path} is not the version the patch was prepared against (it is {}); read it again and prepare a new patch",
                        c.sha256
                    ),
                })
            }
            (Some(_), None) => {
                return Err(EngineError::Refused {
                    code: "stale_patch",
                    message: format!("{path} does not exist; the patch was prepared against an existing file"),
                })
            }
            (None, Some(_)) => {
                return Err(EngineError::Refused {
                    code: "already_exists",
                    message: format!("{path} already exists; to change it, prepare a patch against its current content"),
                })
            }
            (None, None) => {}
        }
        let before = current.map(|c| c.bytes).unwrap_or_default();
        let before_text = String::from_utf8(before).map_err(|_| {
            invalid(format!(
                "{path} is not valid UTF-8; only text files can be edited"
            ))
        })?;
        let after = apply_hunks(&before_text, &input.hunks).map_err(invalid)?;

        let request = RecordedRequest {
            schema_version: REQUEST_SCHEMA_VERSION.into(),
            request_id: new_id("req"),
            received_at: now_rfc3339(),
            intent: input.intent.trim().to_string(),
            constraints: input.constraints.clone(),
            working_directory: root.display().to_string(),
            caller: Caller {
                kind: if opts.interactive {
                    CallerKind::Interactive
                } else {
                    CallerKind::NonInteractive
                },
                #[cfg(unix)]
                // SAFETY: getuid has no failure modes.
                uid: Some(unsafe { libc::getuid() }),
                #[cfg(not(unix))]
                uid: None,
                established_by: "runtime:local_process".into(),
            },
            selection: None,
            bindings: Default::default(),
            expectations: vec![],
        };
        self.journal.record_request(&request)?;

        let step = FileEditStep {
            kind: FileEditKind::FileEdit,
            index: 0,
            capability_id: FILE_EDIT_CAPABILITY_ID.into(),
            root: root.display().to_string(),
            path: path.clone(),
            expected_sha256: input.expected_sha256.clone(),
            hunks: input.hunks.clone(),
            result_sha256: content_hash(after.as_bytes()),
            result_bytes: after.len() as u64,
        };
        let effects = Effects {
            classes: vec![EffectClass::ModifiesFiles],
            certainty: EffectCertainty::Declared,
            source: "file_edit".into(),
            confidence: 1.0,
            notes: vec![format!("writes {path} below {}", root.display())],
        };
        // A caller-supplied `modify_files=true` waives confirmation for
        // *declared* project tasks. It must not waive review of a patch.
        let mut judged = request.constraints.clone();
        if judged.modify_files == Some(true) {
            judged.modify_files = None;
        }
        let assessment = assess(&effects, &judged, &self.config.policy);
        let mut plan = Plan {
            schema_version: PLAN_SCHEMA_VERSION.into(),
            plan_id: new_id("plan"),
            request_id: request.request_id.clone(),
            created_at: now_rfc3339(),
            intent: request.intent.clone(),
            candidates: vec![],
            selection_reason: "an explicit file edit; no capability was searched for".into(),
            steps: vec![Step::FileEdit(step.clone())],
            constraints: request.constraints.clone(),
            timeout_secs: opts
                .timeout_secs
                .unwrap_or(self.config.default_timeout_secs),
            effects,
            risk: assessment.risk,
            approval: assessment.approval.clone(),
            verification: VerificationProcedure {
                checks: vec![PlannedCheck {
                    check: Check::FileSha256 {
                        path: path.clone(),
                        sha256: step.result_sha256.clone(),
                    },
                    task_level: true,
                    source: "runtime:file_edit".into(),
                }],
                success_criteria: vec![format!(
                    "{path} reads back from disk with the planned content"
                )],
            },
            rationale: format!(
                "Edits {path} by applying {} hunk(s) to the version with hash {}.",
                step.hunks.len(),
                step.expected_sha256.as_deref().unwrap_or("(new file)")
            ),
            limitations: {
                let mut l = assessment.limitations.clone();
                l.push("Verification shows the file was written as planned. It does not show the change is correct; run the project's checks for that.".into());
                l
            },
            digest: String::new(),
        };
        plan.seal();
        self.journal.record_plan(&plan)?;
        ui.preview(&plan);

        if !assessment.violations.is_empty() {
            return self.finish_without_execution(
                &request,
                &plan,
                Status::Rejected,
                assessment.violations.clone(),
                None,
                "Rejected: the plan violates the request's constraints or the active policy."
                    .into(),
            );
        }
        if !opts.execute {
            return self.finish_without_execution(
                &request,
                &plan,
                Status::Planned,
                vec![],
                None,
                format!(
                    "Plan {} created and not executed (approval required: {:?}); approve its digest to apply it.",
                    plan.plan_id, plan.approval.level
                ),
            );
        }
        self.execute_plan(&request, plan, opts, ui)
    }

    /// Record an explicit approval for exactly this plan.
    ///
    /// The caller must present the plan's full digest, proving they are
    /// approving the plan as made. Nothing is executed. The approval is
    /// bound to this plan ID and digest and is refused if the plan was
    /// modified, is forbidden by policy, is stale, or was rejected. It is
    /// only a decision: `exec_stored` with `preapproved` still runs the
    /// ordinary authorization check at execution time.
    pub fn approve_plan(
        &mut self,
        plan_id: &str,
        plan_digest: &str,
    ) -> Result<ApprovalRecord, EngineError> {
        let refuse = |code: &'static str, message: String| EngineError::Refused { code, message };
        let plan = self.journal.plan(plan_id)?;
        if !plan.is_intact() {
            return Err(refuse(
                "plan_modified",
                "the stored plan no longer matches its digest".into(),
            ));
        }
        if plan_digest != plan.digest {
            return Err(refuse(
                "approval_mismatch",
                format!("the digest presented is not this plan's digest ({}); approvals apply only to the exact plan", plan.short_digest()),
            ));
        }
        if plan.approval.level == ApprovalLevel::Forbidden {
            return Err(refuse(
                "forbidden",
                "this operation cannot be approved under the active policy".into(),
            ));
        }
        if self
            .journal
            .latest_event_data(&plan.request_id, "plan_rejected")?
            .is_some()
        {
            return Err(refuse(
                "plan_rejected",
                "this plan was rejected and cannot be approved".into(),
            ));
        }
        let stale = self.stale_reasons(&plan);
        if !stale.is_empty() {
            return Err(refuse(
                "stale_plan",
                format!(
                    "the plan is stale ({}); plan the intent again",
                    stale.join("; ")
                ),
            ));
        }
        let method = if plan.approval.level == ApprovalLevel::None {
            ApprovalMethod::PolicyAutomatic
        } else {
            ApprovalMethod::DigestToken
        };
        let rec = grant(&plan, method, ApprovalDecision::Granted);
        self.journal.record_approval(&plan.request_id, &rec)?;
        Ok(rec)
    }

    /// Record that an execution ended without an outcome being recorded (its
    /// runtime died). Nothing is claimed about whether the command finished.
    pub fn record_interrupted(
        &mut self,
        plan_id: &str,
        message: String,
    ) -> Result<ExecutionResult, EngineError> {
        let plan = self.journal.plan(plan_id)?;
        let request = self.journal.request(&plan.request_id)?;
        self.finish_without_execution(&request, &plan, Status::Cancelled, vec![], None, message)
    }

    /// Reject a plan: record the decision and leave it unexecuted. A rejected
    /// plan can no longer be approved.
    pub fn reject_plan(
        &mut self,
        plan_id: &str,
        reason: Option<String>,
    ) -> Result<ExecutionResult, EngineError> {
        let plan = self.journal.plan(plan_id)?;
        let request = self.journal.request(&plan.request_id)?;
        if !plan.is_intact() {
            return Err(EngineError::Refused {
                code: "plan_modified",
                message: "the stored plan no longer matches its digest".into(),
            });
        }
        let already_ran = self.journal.result_rows(100_000)?.iter().any(|(_, r)| {
            r.plan_id.as_deref() == Some(plan_id)
                && crate::output::started_steps(r).next().is_some()
        });
        if already_ran {
            return Err(EngineError::Refused {
                code: "already_executed",
                message: "this plan has already been executed; it cannot be rejected".into(),
            });
        }
        let rec = grant(&plan, ApprovalMethod::DigestToken, ApprovalDecision::Denied);
        self.journal.record_approval(&request.request_id, &rec)?;
        self.journal.event(
            &request.request_id,
            "plan_rejected",
            &serde_json::json!({"plan_id": plan_id, "reason": reason}),
        )?;
        let summary = summary_from(&plan, Some(&rec), false);
        self.finish_without_execution(
            &request,
            &plan,
            Status::Cancelled,
            vec![],
            Some(summary),
            format!(
                "Rejected: the plan was declined before execution; nothing was run.{}",
                reason.map(|r| format!(" Reason: {r}")).unwrap_or_default()
            ),
        )
    }

    // ---------------------------------------------------------------- evidence

    // ----------------------------------------------------------------- outputs

    /// Resolve `latest`, `out_N[.S]` or `exec_N` to a stored execution and step.
    fn locate_output(&self, id: &str) -> Result<(i64, ExecutionResult, usize), EngineError> {
        use crate::output::{OutputId, ParsedId};
        let not_found = || EngineError::NotFound(format!("no captured output with ID {id}"));
        let (seq, step) = if id == "latest" {
            // Deterministic: the newest recorded execution that started a process;
            // for several steps, its last started step.
            let rows = self.journal.result_rows(200)?;
            let (seq, r) = rows
                .into_iter()
                .find(|(_, r)| crate::output::started_steps(r).next().is_some())
                .ok_or_else(|| {
                    EngineError::NotFound("no executed command has been recorded yet".into())
                })?;
            let last = crate::output::started_steps(&r)
                .last()
                .map(|s| s.index)
                .unwrap_or(0);
            return Ok((seq, r, last));
        } else {
            match OutputId::parse(id).ok_or_else(|| {
                EngineError::Invalid(format!(
                    "{id:?} is not an output ID (expected out_<n>, out_<n>.<step>, exec_<n> or latest)"
                ))
            })? {
                ParsedId::Output(o) => (o.seq, Some(o.step)),
                ParsedId::Execution(seq) => (seq, None),
            }
        };
        let r = self.journal.result_row(seq).map_err(|_| not_found())?;
        let step = match step {
            Some(s) => s,
            None => crate::output::started_steps(&r)
                .last()
                .map(|s| s.index)
                .ok_or_else(not_found)?,
        };
        match r.steps.iter().find(|s| s.index == step) {
            Some(s) if s.termination != Termination::NotStarted => Ok((seq, r, step)),
            _ => Err(not_found()),
        }
    }

    /// The stable execution sequence number of a request's newest result.
    pub fn result_seq(&self, request_id: &str) -> Result<Option<i64>, EngineError> {
        Ok(self.journal.latest_result_seq(request_id)?)
    }

    /// Record a batch summary in the hash-chained journal under its batch ID.
    pub fn record_batch(
        &self,
        batch_id: &str,
        data: &serde_json::Value,
    ) -> Result<(), EngineError> {
        Ok(self.journal.event(batch_id, "batch_recorded", data)?)
    }

    pub fn batch_record(&self, batch_id: &str) -> Result<serde_json::Value, EngineError> {
        self.journal
            .latest_event_data(batch_id, "batch_recorded")?
            .ok_or_else(|| EngineError::NotFound(format!("no batch with ID {batch_id}")))
    }

    /// Recent executions that ran a process, newest first.
    pub fn outputs(&self, limit: usize) -> Result<Vec<crate::output::OutputSummary>, EngineError> {
        let mut out = Vec::new();
        for (seq, r) in self.journal.result_rows(limit.saturating_mul(4).max(50))? {
            for s in crate::output::started_steps(&r) {
                out.push(crate::output::summarize(seq, &r, s));
            }
            if out.len() >= limit {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    pub fn output_record(
        &self,
        id: &str,
        inline: bool,
    ) -> Result<crate::output::OutputRecord, EngineError> {
        let (seq, r, step) = self.locate_output(id)?;
        let s = r.steps.iter().find(|s| s.index == step).expect("located");
        Ok(crate::output::OutputRecord::build(
            &self.journal,
            seq,
            &r,
            s,
            inline,
        ))
    }

    /// The bytes or text of one representation. Never re-runs anything.
    pub fn output_part(&self, id: &str, part: crate::output::Part) -> Result<Vec<u8>, EngineError> {
        use crate::output::Part;
        let (_, r, step) = self.locate_output(id)?;
        let s = r.steps.iter().find(|s| s.index == step).expect("located");
        Ok(match part {
            Part::Response | Part::Stdout => crate::output::response_bytes(&self.journal, s)?,
            Part::Stderr => crate::output::stderr_bytes(&self.journal, s)?,
            Part::CommandAndResponse => {
                crate::output::command_and_response(&self.journal, s)?.into_bytes()
            }
        })
    }

    /// Delete the captured content of executions started before `cutoff`
    /// (RFC 3339). Records, IDs and the hash chain remain; the content just
    /// stops being retrievable. Content still referenced by a newer result
    /// (identical output is stored once) or by non-result events is kept.
    pub fn prune_outputs(
        &mut self,
        cutoff: &str,
        dry_run: bool,
    ) -> Result<PruneReport, EngineError> {
        let rows = self.journal.result_rows(1_000_000)?;
        let old: Vec<i64> = rows
            .iter()
            .filter(|(_, r)| {
                let started = crate::output::started_steps(r)
                    .map(|s| s.started_at.as_str())
                    .min();
                started.is_some_and(|t| t < cutoff)
            })
            .map(|(s, _)| *s)
            .collect();
        let old_requests: Vec<String> = rows
            .iter()
            .filter(|(s, _)| old.contains(s))
            .map(|(_, r)| r.request_id.clone())
            .collect();
        let mut candidates = std::collections::BTreeSet::new();
        for (seq, r) in &rows {
            if old.contains(seq) {
                for s in crate::output::started_steps(r) {
                    candidates.extend(s.stdout_ref.iter().chain(s.stderr_ref.iter()).cloned());
                }
            }
        }
        let mut report = PruneReport {
            executions: old.len(),
            dry_run,
            ..Default::default()
        };
        for blob in candidates {
            if !self.journal.blob_exists(&blob) {
                continue;
            }
            let shared = self.journal.results_mentioning(&blob, &old)?
                || self.journal.blob_mentioned_outside(&blob, &old_requests)?;
            if shared {
                report.kept_shared += 1;
                continue;
            }
            let size = self
                .journal
                .read_blob(&blob)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            if !dry_run {
                self.journal.delete_blob(&blob)?;
            }
            report.blobs_removed += 1;
            report.bytes_freed += size;
        }
        if !dry_run {
            self.journal.event(
                "system",
                "outputs_pruned",
                &serde_json::json!({
                    "cutoff": cutoff,
                    "executions": report.executions,
                    "blobs_removed": report.blobs_removed,
                    "bytes_freed": report.bytes_freed,
                }),
            )?;
        }
        Ok(report)
    }

    pub fn task(&self, id: &str) -> Result<TaskRecord, EngineError> {
        Ok(self.journal.task(id)?)
    }

    pub fn tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, EngineError> {
        Ok(self.journal.recent_tasks(limit)?)
    }

    pub fn read_evidence(&self, reference: &str) -> Result<Vec<u8>, EngineError> {
        Ok(self.journal.read_blob(reference)?)
    }
}

pub fn evidence_ref(request_id: &str) -> String {
    format!("evidence:task:{request_id}")
}

fn level_name(l: ApprovalLevel) -> &'static str {
    match l {
        ApprovalLevel::None => "none",
        ApprovalLevel::Confirm => "confirm",
        ApprovalLevel::Explicit => "explicit",
        ApprovalLevel::Forbidden => "forbidden",
    }
}

/// Accept the full digest, the hex part, or a prefix of at least 12 hex chars.
pub fn digest_matches(token: &str, digest: &str) -> bool {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let t = token
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(token.trim())
        .to_lowercase();
    t.len() >= 12 && hex.starts_with(&t)
}

fn summary_from(plan: &Plan, approval: Option<&ApprovalRecord>, granted: bool) -> ApprovalSummary {
    ApprovalSummary {
        required: plan.approval.level,
        granted: granted && approval.is_some_and(|a| a.decision == ApprovalDecision::Granted),
        method: approval.map(|a| a.method),
        approval_id: approval.map(|a| a.approval_id.clone()),
        plan_digest: plan.digest.clone(),
        reasons: plan
            .approval
            .requirements
            .iter()
            .map(|r| r.reason.clone())
            .collect(),
    }
}

fn not_started(step: &Step) -> StepResult {
    let now = now_rfc3339();
    let (executable, arguments, working_directory) = match step {
        Step::Process(p) => (
            p.resolved_executable.clone(),
            p.arguments.clone(),
            p.working_directory.clone(),
        ),
        Step::FileEdit(f) => (
            f.capability_id.clone(),
            vec![f.path.clone()],
            f.root.clone(),
        ),
    };
    StepResult {
        index: step.index(),
        capability_id: step.capability_id().to_string(),
        executable,
        arguments,
        working_directory,
        termination: Termination::NotStarted,
        exit_code: None,
        signal: None,
        pid: None,
        started_at: now.clone(),
        finished_at: now,
        duration_ms: 0,
        stdout_ref: None,
        stderr_ref: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
        stdout_truncated: false,
        stderr_truncated: false,
        error: Some("not started because an earlier step did not succeed".into()),
        file_edit: None,
    }
}

/// For each file edit of a plan, what the file looks like now relative to the
/// patch. Used to describe an interrupted execution; read-only.
pub fn file_edit_state_notes(plan: &Plan) -> Vec<String> {
    use cli2_execution::fsedit::read_file;
    plan.steps
        .iter()
        .filter_map(|s| s.as_file_edit())
        .map(|f| match read_file(Path::new(&f.root), &f.path) {
            Ok(snap) if snap.sha256 == f.result_sha256 => format!(
                "{} currently has the content this plan would have produced, so the edit may have been committed; it was not confirmed and will not be applied again.",
                f.path
            ),
            Ok(snap) if Some(&snap.sha256) == f.expected_sha256.as_ref() => format!(
                "{} currently has the content the patch was prepared against, so the edit was probably not committed.",
                f.path
            ),
            Ok(_) => format!(
                "{} currently has neither the prepared-against content nor the planned result; it was changed by something else.",
                f.path
            ),
            Err(cli2_execution::fsedit::ReadError::Missing(_)) if f.expected_sha256.is_none() => {
                format!("{} does not exist, so the creation was probably not committed.", f.path)
            }
            Err(e) => format!("{} could not be inspected: {e}.", f.path),
        })
        .collect()
}

/// Reasons a planned file edit no longer applies to what is on disk now.
fn file_edit_stale(f: &FileEditStep, reasons: &mut Vec<String>) {
    use cli2_execution::fsedit::{read_file, ReadError};
    match (read_file(Path::new(&f.root), &f.path), &f.expected_sha256) {
        (Ok(snap), Some(h)) if &snap.sha256 == h => {}
        (Ok(_), Some(_)) => {
            reasons.push(format!("{} changed since the patch was prepared", f.path))
        }
        (Ok(_), None) => reasons.push(format!("{} now exists; the plan creates it", f.path)),
        (Err(ReadError::Missing(_)), None) => {}
        (Err(ReadError::Missing(_)), Some(_)) => {
            reasons.push(format!("{} no longer exists", f.path))
        }
        (Err(ReadError::Refused(e)), _) => reasons.push(e),
    }
}

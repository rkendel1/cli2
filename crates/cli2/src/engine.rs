//! Orchestration of the CLI 2.0 loop:
//! intent → discovery/index → resolution → authority/execution → verification/evidence.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cli2_core::*;
use cli2_discovery::{find_project_root, parse_help, scan_path, scan_project, ShellContext};
use cli2_evidence::{Journal, JournalError, Observation, StepObservation, TaskRecord, TaskSummary};
use cli2_execution::{
    assess, authorize, grant, AuthorizationError, CancelToken, Policy, ProcessSpec,
};
use cli2_index::{Index, IndexError, ReconcileReport, ENV_SCOPE};
use cli2_resolver::{
    resolve, HistoryStats, NoInference, Resolution, ResolveContext, SemanticRanker, SemanticReport,
};
use serde::Serialize;

use crate::semantic::{AppleFmRanker, Fallback, Provider, ResolverMode, ResolverSettings, PROVIDER_ID};

/// Default per-step timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
const HELP_TIMEOUT: Duration = Duration::from_secs(5);
const HELP_MAX_BYTES: usize = 64 * 1024;
const HELP_EXCERPT_CHARS: usize = 4000;

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the index, journal and evidence live.
    pub home: PathBuf,
    /// The `PATH` value to discover executables from.
    pub path_var: OsString,
    pub default_timeout_secs: u64,
    pub policy: Policy,
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
            policy,
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
        }
    }

    /// Documented process exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            EngineError::IndexCorrupt(_) => 9,
            EngineError::Invalid(_) | EngineError::NotFound(_) => 2,
            EngineError::SemanticUnavailable(_) => 11,
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

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
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
    index: Index,
    journal: Journal,
    ranker: Box<dyn SemanticRanker>,
    cancel: CancelToken,
    settings: ResolverSettings,
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

/// A resolution plus how it was produced.
struct Resolved {
    resolution: Resolution,
    event: Option<SemanticEvent>,
    notes: Vec<String>,
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
        Ok(Self::assemble(config, index, journal, settings))
    }

    fn assemble(config: Config, index: Index, journal: Journal, settings: ResolverSettings) -> Self {
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
        Ok((Self::assemble(config, index, journal, settings), moved))
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

    /// Capture bounded help output for an executable (explicit, on demand)
    /// and record what it reveals, with provenance.
    pub fn capture_help(&mut self, id: &str, cwd: &Path) -> Result<Capability, EngineError> {
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
        let mut spec = ProcessSpec::new(path, vec!["--help".into()], cwd, HELP_TIMEOUT);
        spec.max_output_bytes = HELP_MAX_BYTES;
        let out = cli2_execution::run(&spec, &self.cancel);
        let raw = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let text = String::from_utf8_lossy(raw).to_string();
        self.journal.event(
            "system",
            "help_captured",
            &serde_json::json!({
                "capability_id": id,
                "fingerprint": cap.fingerprint,
                "termination": out.termination,
                "exit_code": out.exit_code,
                "output_ref": self.journal.store_blob(raw)?,
            }),
        )?;
        if text.trim().is_empty() || out.termination != Termination::Exited {
            return Err(EngineError::Invalid(format!(
                "`{} --help` produced no usable help output ({:?}, exit {:?})",
                cap.name, out.termination, out.exit_code
            )));
        }
        let parsed = parse_help(&text, &cap.name);
        cap.help = Some(HelpCapture {
            captured_at: now_rfc3339(),
            arguments: vec!["--help".into()],
            exit_code: out.exit_code,
            truncated: out.stdout_truncated() || out.stderr_truncated(),
            excerpt: text.chars().take(HELP_EXCERPT_CHARS).collect(),
        });
        if let Some(p) = parsed.purpose {
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
        if parsed.usage_complete {
            cap.interface.parameters = parsed.parameters;
            cap.interface.known_flags = parsed.flags;
            cap.interface.completeness = InterfaceCompleteness::Inferred;
            cap.interface.source = "help_output:usage".into();
            cap.enrichment = cap.enrichment.max(EnrichmentState::Characterized);
        } else {
            cap.interface.known_flags = parsed.flags;
        }
        self.index.put(&cap)?;
        Ok(cap)
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
                // Only the single best uncharacterized executable is probed.
                let mut target = None;
                for o in &clarification.options {
                    let cap = self.get_capability(&o.capability_id, &wd)?;
                    if cap.kind == CapabilityKind::Executable
                        && cap.interface.completeness == InterfaceCompleteness::Unknown
                        && cap.help.is_none()
                    {
                        target = Some(cap);
                        break;
                    }
                }
                if let Some(cap) = target {
                    let id = cap.id.clone();
                    {
                        match self.capture_help(&id, &wd) {
                            Ok(_) => notes.push(format!(
                                "Captured `{} --help` to characterize {id} (requested with --capture-help).",
                                cap.name
                            )),
                            Err(e) => notes.push(format!("Help capture for {id} failed: {e}")),
                        }
                        resolution = self.resolve_recorded(&request, root.as_deref(), &mut notes)?;
                    }
                }
            }
        }

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

    #[allow(clippy::type_complexity)]
    fn resolve_request(
        &self,
        request: &RecordedRequest,
        root: Option<&str>,
    ) -> Result<Resolved, (EngineError, Option<SemanticEvent>)> {
        let wrap = |e: IndexError| (EngineError::from(e), None);
        let caps = self.index.visible(root).map_err(wrap)?;
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
                notes: vec![],
            });
        };

        let deterministic = || {
            self.resolve_with(request, &caps, root, &history, &NoInference, &[])
        };
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
                notes: vec![],
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
                notes: vec![],
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
                let mut notes = vec![];
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
                        notes: vec![format!(
                            "Semantic ranking with {PROVIDER_ID} was not used ({why}); the deterministic resolver produced the candidate ordering."
                        )],
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

    fn finish_without_execution(
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
    fn stale_reasons(&mut self, plan: &Plan) -> Vec<String> {
        let mut reasons = Vec::new();
        for step in &plan.steps {
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
                if let Some(token) = &opts.approve_token {
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
        if let Some(a) = &approval {
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
        let mut outcomes = Vec::new();
        let mut stopped = false;
        for step in &plan.steps {
            if stopped {
                step_results.push(not_started(step));
                continue;
            }
            self.journal.event(
                &request.request_id,
                "step_started",
                &serde_json::json!({
                    "plan_id": plan.plan_id,
                    "step": step,
                }),
            )?;
            let spec = ProcessSpec::new(
                &step.resolved_executable,
                step.arguments.clone(),
                &step.working_directory,
                Duration::from_secs(plan.timeout_secs),
            );
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
            };
            self.journal
                .event(&request.request_id, "step_finished", &sr)?;
            if !sr.process_succeeded() {
                stopped = true;
            }
            step_results.push(sr);
            outcomes.push(out);
        }

        // 4. Verification.
        let observations: Vec<StepObservation<'_>> = step_results
            .iter()
            .zip(outcomes.iter())
            .map(|(r, o)| StepObservation {
                result: r,
                started: o.started,
                stdout: &o.stdout,
            })
            .collect();
        let verification = cli2_evidence::verify(&plan.verification, &observations);
        self.journal
            .event(&request.request_id, "verification_completed", &verification)?;

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
        Ok(result)
    }

    // ---------------------------------------------------------------- evidence

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
    StepResult {
        index: step.index,
        capability_id: step.capability_id.clone(),
        executable: step.resolved_executable.clone(),
        arguments: step.arguments.clone(),
        working_directory: step.working_directory.clone(),
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
    }
}

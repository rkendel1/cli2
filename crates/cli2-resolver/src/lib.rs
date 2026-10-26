//! Intent resolution.
//!
//! Resolution order:
//! 1. **Deterministic match** — the intent names a capability ID, the caller
//!    selected one, or the intent is exactly a known invocation (`npm run test`).
//! 2. **Capability retrieval** — lexical/metadata scoring over the local index.
//! 3. **Ranking** — fit, locality, interface completeness, history; an optional
//!    [`SemanticRanker`] may adjust scores but cannot add candidates, change
//!    viability, or construct invocations.
//! 4. **Plan construction** — exact executable + argument vector from known
//!    interfaces only; parameters are bound from explicit bindings or values
//!    present in the intent. Nothing is invented.
//! 5. **Ambiguity handling** — materially different close candidates produce a
//!    clarification instead of a guess.
//!
//! Policy validation (step 6) is performed by the runtime with
//! `cli2-execution` on the draft this crate returns.

pub mod terms;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use cli2_core::{
    Candidate, Capability, CapabilityKind, Check, Clarification, ClarificationOption,
    EffectCertainty, Effects, InterfaceCapture, InterfaceCompleteness, ParameterRole, PlannedCheck,
    RecordedRequest, Step, VerificationProcedure,
};

use terms::{
    content_stems, equivalents, extract_values, is_path_like, search_subject, stem, words,
};

/// Minimum score for a capability to be selected.
pub const MIN_SCORE: f64 = 3.0;
/// Candidates within this margin of the best are considered competing.
pub const AMBIGUITY_MARGIN: f64 = 1.0;
/// When the best match cannot be planned, comparably scored alternatives are
/// offered (and are candidates for help capture).
pub const ALTERNATIVE_MARGIN: f64 = 4.0;
/// Score for a plannable capability that structurally fits a search request
/// but has no lexical match, and the bonus when it also has one.
const STRUCTURAL_SCORE: f64 = 6.0;
const STRUCTURAL_BONUS: f64 = 3.0;

/// Whether the capability's verified interface takes a search term and its
/// own description (captured help) says it searches.
fn takes_search_term(cap: &Capability) -> bool {
    if !cap.interface.source.starts_with("help_output")
        || cap.interface.completeness < InterfaceCompleteness::Inferred
        || !cap
            .interface
            .parameters
            .iter()
            .any(|p| p.role() == ParameterRole::SearchTerm)
    {
        return false;
    }
    let mut text = cap
        .purpose
        .as_ref()
        .map(|p| p.value.clone())
        .unwrap_or_default();
    if let Some(h) = &cap.help {
        text.push(' ');
        text.extend(h.excerpt.chars().take(600));
    }
    let toks = content_stems(&text);
    equivalents("search").iter().any(|e| toks.contains(e))
}

const OUTPUT_PARAMETER_NAMES: &[&str] = &[
    "out",
    "output",
    "outfile",
    "output_file",
    "output-file",
    "dest",
    "destination",
    "target",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub successes: u32,
    pub failures: u32,
}

/// Extension point for model-assisted ranking. Implementations may only
/// re-score the candidates they are given.
pub trait SemanticRanker {
    fn name(&self) -> &str;
    fn rerank(
        &self,
        intent: &str,
        candidates: &mut [Candidate],
        capabilities: &[&Capability],
    ) -> Result<(), String>;

    /// Like [`rerank`](Self::rerank) but with the whole request, so a ranker
    /// can honour its constraints. Defaults to `rerank` on the intent.
    fn rerank_request(
        &self,
        request: &RecordedRequest,
        candidates: &mut [Candidate],
        capabilities: &[&Capability],
    ) -> Result<(), String> {
        self.rerank(&request.intent, candidates, capabilities)
    }

    /// Whether the resolver should widen retrieval beyond lexical matches
    /// (see [`ResolveContext::extra_terms`]) so the ranker can see candidates
    /// whose names or descriptions do not share words with the intent.
    fn wants_wide_retrieval(&self) -> bool {
        false
    }

    /// Metadata about the most recent `rerank`, for evidence. Taking it clears it.
    fn take_report(&self) -> Option<SemanticReport> {
        None
    }
}

/// What a semantic ranker did, recorded as task evidence. The model's reasons
/// are explanatory metadata, never proof that a plan is valid.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SemanticReport {
    pub schema_version: &'static str,
    pub provider: String,
    pub bridge_version: Option<String>,
    pub bridge_request_id: String,
    /// Search terms the model proposed for retrieval (matched against the
    /// real inventory by the runtime; never capability IDs).
    pub expansion_terms: Vec<String>,
    pub submitted: Vec<String>,
    pub order_before: Vec<String>,
    pub order_after: Vec<String>,
    /// ranked | no_match | clarification | failed
    pub status: String,
    /// `valid`, or why the response was rejected.
    pub validation: String,
    pub rankings: Vec<ReportedRanking>,
    pub clarification: Option<String>,
    pub availability: Option<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportedRanking {
    pub capability_id: String,
    pub rank: u32,
    pub relevance: String,
    pub reason: String,
}

impl SemanticReport {
    pub const SCHEMA: &'static str = "cli2.semantic_resolution.v1";

    pub fn failed(&self) -> bool {
        self.status == "failed"
    }
}

/// The default: no inference provider. Deterministic ranking only.
pub struct NoInference;

impl SemanticRanker for NoInference {
    fn name(&self) -> &str {
        "none"
    }
    fn rerank(&self, _: &str, _: &mut [Candidate], _: &[&Capability]) -> Result<(), String> {
        Ok(())
    }
}

pub struct ResolveContext<'a> {
    pub request: &'a RecordedRequest,
    /// Capabilities visible from the request's working context.
    pub capabilities: &'a [Capability],
    /// Root of the project the request runs in, if any.
    pub project_root: Option<&'a str>,
    pub history: &'a BTreeMap<String, HistoryStats>,
    pub ranker: &'a dyn SemanticRanker,
    /// Search terms (from the semantic layer) matched against capability names
    /// to widen retrieval. Only used when the ranker asks for wide retrieval,
    /// and only ever admits capabilities that exist in `capabilities`.
    pub extra_terms: &'a [String],
}

/// Whether `name` matches a retrieval term: equal, or a whole name segment
/// (`ripgrep` matches `ripgrep`, `rg-all`; `test` matches `test:fail`).
fn name_matches_term(name: &str, term: &str) -> bool {
    let name = name.to_lowercase();
    let term = term.to_lowercase();
    name == term
        || name
            .split(|c: char| !c.is_alphanumeric())
            .any(|seg| !seg.is_empty() && seg == term)
}

/// Retrieval is bounded: wide retrieval admits at most this many
/// zero-lexical-score capabilities, project tasks first.
pub const WIDE_RETRIEVAL_LIMIT: usize = 40;

/// A resolved plan before policy assessment and sealing.
#[derive(Debug, Clone)]
pub struct Draft {
    pub candidates: Vec<Candidate>,
    pub selected: Capability,
    pub selection_reason: String,
    pub steps: Vec<Step>,
    pub effects: Effects,
    pub verification: VerificationProcedure,
    pub rationale: String,
    pub limitations: Vec<String>,
    /// Provenance of the interface the invocation was built from, when it
    /// came from captured help output.
    pub interface_capture: Option<InterfaceCapture>,
}

#[derive(Debug, Clone)]
pub enum Resolution {
    Planned(Box<Draft>),
    Clarify {
        candidates: Vec<Candidate>,
        clarification: Clarification,
        limitations: Vec<String>,
    },
}

struct Scored<'a> {
    cap: &'a Capability,
    candidate: Candidate,
    built: Result<Vec<String>, Unbuildable>,
}

#[derive(Debug, Clone)]
enum Unbuildable {
    RunnerMissing(String),
    InterfaceUnknown,
    /// Some of the interface was recognized in help output, not enough to call it.
    InterfacePartial(String),
    /// Help was run but yielded nothing trustworthy.
    CaptureFailed(String),
    MissingInputs(Vec<String>),
    BadBinding(String),
}

impl Unbuildable {
    fn reason(&self, cap: &Capability) -> String {
        match self {
            Unbuildable::RunnerMissing(r) => format!("`{r}` was not found on PATH"),
            Unbuildable::InterfaceUnknown => format!(
                "the interface of `{}` is not characterized; cli2 will not guess its arguments",
                cap.name
            ),
            Unbuildable::InterfacePartial(d) => format!(
                "only part of the interface of `{}` was understood ({d}); cli2 will not guess the rest",
                cap.name
            ),
            Unbuildable::CaptureFailed(d) => format!(
                "the interface of `{}` could not be learned from its help output ({d})",
                cap.name
            ),
            Unbuildable::MissingInputs(m) => format!("missing required input(s): {}", m.join(", ")),
            Unbuildable::BadBinding(b) => b.clone(),
        }
    }
}

pub fn resolve(ctx: &ResolveContext<'_>) -> Resolution {
    let intent = ctx.request.intent.trim();
    let (values, unquoted) = extract_values(intent);
    let subject = search_subject(intent);
    let intent_stems = content_stems(&unquoted);
    let raw_words = words(&unquoted);
    let mentions_project = raw_words.iter().any(|w| w == "project" || w == "projects");
    let intent_lower = intent.to_lowercase();
    let mut limitations = Vec::new();

    let env_by_id: BTreeMap<&str, &Capability> = ctx
        .capabilities
        .iter()
        .filter(|c| c.kind == CapabilityKind::Executable)
        .map(|c| (c.id.as_str(), c))
        .collect();

    // Deterministic selection by the caller.
    if let Some(sel) = &ctx.request.selection {
        if !ctx.capabilities.iter().any(|c| &c.id == sel) {
            return Resolution::Clarify {
                candidates: vec![],
                clarification: Clarification {
                    question: format!("`{sel}` is not a discovered capability. Run `cli2 inventory` to see what is available."),
                    options: vec![],
                    missing_inputs: vec![],
                },
                limitations,
            };
        }
    }

    let wide = ctx.ranker.wants_wide_retrieval();
    let mut wide_admitted = 0usize;
    let mut scored: Vec<Scored<'_>> = Vec::new();
    for cap in ctx.capabilities {
        let (mut score, mut reasons) =
            score_capability(cap, &intent_stems, &raw_words, mentions_project, ctx);
        let invocation = interface_invocation(cap);
        if intent_lower == cap.id.to_lowercase()
            || intent_lower == invocation.join(" ").to_lowercase()
            || (cap.kind == CapabilityKind::Executable && intent_lower == cap.name.to_lowercase())
        {
            score = 100.0;
            reasons.insert(0, "intent exactly names this capability".into());
        }
        if ctx.request.selection.as_deref() == Some(cap.id.as_str()) {
            score = 1000.0;
            reasons.insert(0, "selected by the caller".into());
        }
        let built = build_arguments(
            cap,
            &values,
            subject.as_deref(),
            &ctx.request.bindings,
            &env_by_id,
        );
        // Structural fit: the request states a search subject, and this
        // capability's *verified* interface takes a search term and its own
        // description says it searches. Only plannable capabilities benefit.
        if subject.is_some() && built.is_ok() && takes_search_term(cap) {
            score = if score > 0.0 {
                score + STRUCTURAL_BONUS
            } else {
                STRUCTURAL_SCORE
            };
            reasons.push(
                "its verified interface takes a search term, its description says it searches, and the request names what to search for".into(),
            );
        }
        if score <= 0.0 {
            // Wide retrieval (semantic ranking only): admit project tasks and
            // capabilities whose name matches a proposed term, with a zero
            // lexical score. They still must earn a score from the ranker.
            let admitted = wide
                && wide_admitted < WIDE_RETRIEVAL_LIMIT
                && (cap.kind == CapabilityKind::ProjectTask
                    || ctx
                        .extra_terms
                        .iter()
                        .any(|t| name_matches_term(&cap.name, t)));
            if !admitted {
                continue;
            }
            wide_admitted += 1;
            score = 0.0;
            reasons.push("retrieved for semantic ranking (no lexical match)".into());
        }
        let mut candidate = Candidate {
            capability_id: cap.id.clone(),
            kind: cap.kind,
            score: round2(score),
            viable: built.is_ok(),
            reasons,
            invocation: None,
        };
        match &built {
            Ok(args) => {
                let mut argv = vec![cap.interface.executable.clone()];
                argv.extend(args.iter().cloned());
                candidate.invocation = Some(argv);
            }
            Err(why) => candidate
                .reasons
                .push(format!("not viable: {}", why.reason(cap))),
        }
        scored.push(Scored {
            cap,
            candidate,
            built,
        });
    }

    // Optional semantic re-ranking: scores only. An exact or caller-selected
    // match (score >= 100) is a deterministic decision and is never reranked.
    let decided = scored.iter().any(|s| s.candidate.score >= 100.0);
    if !scored.is_empty() && !decided {
        // The ranker sees the deterministic order, best first.
        scored.sort_by(|a, b| {
            b.candidate
                .score
                .partial_cmp(&a.candidate.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.candidate.capability_id.cmp(&b.candidate.capability_id))
        });
        let before: Vec<Candidate> = scored.iter().map(|s| s.candidate.clone()).collect();
        let mut cands: Vec<Candidate> = before.clone();
        let caps: Vec<&Capability> = scored.iter().map(|s| s.cap).collect();
        match ctx.ranker.rerank_request(ctx.request, &mut cands, &caps) {
            Ok(()) => {
                for (s, (new, old)) in scored.iter_mut().zip(cands.into_iter().zip(before)) {
                    if new.capability_id == old.capability_id && new.score.is_finite() {
                        s.candidate.score = round2(new.score);
                        if (new.score - old.score).abs() > f64::EPSILON {
                            s.candidate
                                .reasons
                                .push(format!("score adjusted by ranker `{}`", ctx.ranker.name()));
                        }
                        // Rankers may explain themselves, never change viability.
                        for r in new.reasons.iter().skip(old.reasons.len()) {
                            s.candidate.reasons.push(r.clone());
                        }
                    }
                }
            }
            Err(e) => limitations.push(format!(
                "semantic ranker `{}` failed ({e}); deterministic ranking was used",
                ctx.ranker.name()
            )),
        }
    }

    scored.sort_by(|a, b| {
        b.candidate
            .score
            .partial_cmp(&a.candidate.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.candidate.capability_id.cmp(&b.candidate.capability_id))
    });
    let candidates: Vec<Candidate> = scored.iter().take(8).map(|s| s.candidate.clone()).collect();

    let best_viable = scored.iter().position(|s| s.built.is_ok());
    let best_any = scored.first();

    // Nothing relevant at all.
    let Some(best) = best_any.filter(|s| s.candidate.score >= MIN_SCORE) else {
        let weak: Vec<&Scored<'_>> = scored
            .iter()
            .filter(|s| s.candidate.score >= 2.0)
            .take(5)
            .collect();
        let uncharacterized: Vec<String> = weak
            .iter()
            .filter(|s| matches!(s.built, Err(Unbuildable::InterfaceUnknown)))
            .map(|s| format!("`cli2 describe {} --capture-help`", s.cap.id))
            .collect();
        let hint = if uncharacterized.is_empty() {
            String::new()
        } else {
            format!(
                " Some weak matches have no recorded description; learning more about them may help: {} (or re-run with --capture-help).",
                uncharacterized.join(", ")
            )
        };
        return Resolution::Clarify {
            candidates,
            clarification: Clarification {
                question: format!(
                    "No discovered capability clearly matches {intent:?}. Rephrase the intent, name the command, or run `cli2 scan` if it was installed recently.{hint}"
                ),
                options: weak.iter().map(|s| option_for(s.cap, &s.candidate)).collect(),
                missing_inputs: vec![],
            },
            limitations,
        };
    };

    // The best match cannot be planned and nothing comparable can: ask, don't
    // fall back to a worse match.
    let viable_ok = best_viable.map(|i| &scored[i]).filter(|s| {
        s.candidate.score >= MIN_SCORE
            && s.candidate.score + AMBIGUITY_MARGIN >= best.candidate.score
    });
    let Some(chosen) = viable_ok else {
        let missing = match &best.built {
            Err(Unbuildable::MissingInputs(m)) => {
                m.iter().map(|p| format!("{}:{p}", best.cap.id)).collect()
            }
            _ => vec![],
        };
        let reason = best
            .built
            .as_ref()
            .err()
            .map(|e| e.reason(best.cap))
            .unwrap_or_default();
        let hint = match &best.built {
            Err(Unbuildable::InterfaceUnknown) => format!(
                " Capture its help with `cli2 describe {} --capture-help` (this runs `{} --help`), or re-run with --capture-help.",
                best.cap.id, best.cap.name
            ),
            Err(Unbuildable::MissingInputs(m)) => format!(
                " Provide {} with --arg NAME=VALUE (or quote the value in the intent).{}",
                m.join(", "),
                if best.cap.interface.parameters.iter().any(|p| {
                    m.contains(&p.name) && p.role() == ParameterRole::Path && !p.required && p.omitted_means.is_none()
                }) {
                    " Its help does not say what happens when the path is omitted, so cli2 will not omit it."
                } else {
                    ""
                }
            ),
            Err(Unbuildable::InterfacePartial(_)) => {
                " Another capability with a fully understood interface may fit; `cli2 describe` shows what was recognized.".to_string()
            }
            _ => String::new(),
        };
        return Resolution::Clarify {
            candidates,
            clarification: Clarification {
                question: format!(
                    "The best match, `{}`, cannot be planned yet: {reason}.{hint}",
                    best.cap.id
                ),
                options: {
                    let mut o = vec![option_for(best.cap, &best.candidate)];
                    o.extend(
                        scored
                            .iter()
                            .filter(|s| {
                                s.cap.id != best.cap.id
                                    && s.candidate.score >= MIN_SCORE
                                    && s.candidate.score + ALTERNATIVE_MARGIN
                                        >= best.candidate.score
                            })
                            .take(2)
                            .map(|s| option_for(s.cap, &s.candidate)),
                    );
                    o
                },
                missing_inputs: missing,
            },
            limitations,
        };
    };

    // Ambiguity: other viable candidates that are close and materially different.
    if ctx.request.selection.is_none() && chosen.candidate.score < 100.0 {
        let competing: Vec<&Scored<'_>> = scored
            .iter()
            .filter(|s| {
                s.built.is_ok()
                    && s.candidate.capability_id != chosen.candidate.capability_id
                    && s.candidate.score + AMBIGUITY_MARGIN > chosen.candidate.score
                    && s.candidate.invocation != chosen.candidate.invocation
            })
            .collect();
        if !competing.is_empty() {
            let mut options = vec![option_for(chosen.cap, &chosen.candidate)];
            options.extend(competing.iter().map(|s| option_for(s.cap, &s.candidate)));
            let names: Vec<String> = options
                .iter()
                .map(|o| format!("`{}`", o.capability_id))
                .collect();
            return Resolution::Clarify {
                candidates,
                clarification: Clarification {
                    question: format!(
                        "{intent:?} matches {} capabilities that would do different things: {}. Which one did you mean? (re-run with --pick ID)",
                        options.len(),
                        names.join(", ")
                    ),
                    options,
                    missing_inputs: vec![],
                },
                limitations,
            };
        }
    }

    let cap = chosen.cap;
    let args = chosen.built.clone().expect("chosen candidate is viable");
    let (resolved, exe_fp) =
        runner_location(cap, &env_by_id).expect("viable implies runner located");
    let working_directory = match (&cap.kind, &cap.location.project_root) {
        (CapabilityKind::ProjectTask, Some(root)) => root.clone(),
        _ => ctx.request.working_directory.clone(),
    };
    let step = Step {
        index: 0,
        capability_id: cap.id.clone(),
        capability_fingerprint: cap.fingerprint.clone(),
        executable: cap.interface.executable.clone(),
        resolved_executable: resolved,
        executable_fingerprint: exe_fp,
        arguments: args.clone(),
        working_directory,
    };

    // Verification procedure.
    let mut checks = vec![PlannedCheck {
        check: Check::ProcessExitStatus { expected: 0 },
        task_level: false,
        source: "runtime".into(),
    }];
    for post in &cap.postconditions {
        checks.push(PlannedCheck {
            check: post.value.clone(),
            task_level: post.value.is_task_level(),
            source: post.source.clone(),
        });
    }
    let fixed = cap.interface.arguments.len();
    for (param, value) in cap.interface.parameters.iter().zip(args.iter().skip(fixed)) {
        if OUTPUT_PARAMETER_NAMES.contains(&param.name.as_str()) {
            checks.push(PlannedCheck {
                check: Check::FileExists {
                    path: value.clone(),
                    modified_after_start: true,
                },
                task_level: true,
                source: format!("interface:output_parameter:{}", param.name),
            });
        }
    }
    let mut searched = None;
    if cap.interface.source.starts_with("help_output") {
        for (param, value) in cap.interface.parameters.iter().zip(args.iter().skip(fixed)) {
            if param.role() == ParameterRole::SearchTerm {
                checks.push(PlannedCheck {
                    check: Check::StdoutContains {
                        text: value.clone(),
                    },
                    task_level: true,
                    source: format!("interface:search_term:{}", param.name),
                });
                searched = Some(value.clone());
            }
        }
    }
    for exp in &ctx.request.expectations {
        checks.push(PlannedCheck {
            check: exp.clone(),
            task_level: exp.is_task_level(),
            source: "caller_expectation".into(),
        });
    }
    let success_criteria = checks.iter().map(|c| c.check.describe()).collect();
    let verification = VerificationProcedure {
        checks,
        success_criteria,
    };

    let effects = cap.effects.clone();
    if effects.certainty < EffectCertainty::Declared {
        limitations.push(match effects.certainty {
            EffectCertainty::Unknown => {
                "The operation's effects are unknown; they have not been characterized.".to_string()
            }
            _ => format!(
                "Effects are assessed heuristically ({}); the complete effects have not been established.",
                effects.source
            ),
        });
    }
    if !verification.has_task_level_checks() {
        limitations.push(
            "No task-level postcondition is known for this capability; a zero exit status shows only that the process completed, not that the task succeeded.".into(),
        );
    }
    if let Some(term) = &searched {
        limitations.push(format!(
            "Success is judged by the output mentioning {term:?} as well as a zero exit status; most search tools exit 1 when nothing matches, which is reported as a failure, not as success. The search is literal to what the tool does with the pattern (a regex tool treats it as a regex)."
        ));
    }
    limitations
        .push("Execution is not sandboxed; the process runs with your user's permissions.".into());

    let runner_up = scored
        .iter()
        .find(|s| s.candidate.capability_id != cap.id)
        .map(|s| {
            format!(
                "; next best `{}` scored {}",
                s.candidate.capability_id, s.candidate.score
            )
        })
        .unwrap_or_else(|| "; it was the only candidate".into());
    let selection_reason = format!(
        "`{}` scored {} ({}){}",
        cap.id,
        chosen.candidate.score,
        chosen
            .candidate
            .reasons
            .iter()
            .filter(|r| !r.starts_with("not viable"))
            .cloned()
            .collect::<Vec<_>>()
            .join("; "),
        runner_up
    );
    let where_from = match cap.kind {
        CapabilityKind::ProjectTask => format!("defined in {}", cap.location.path),
        _ => format!("found at {}", cap.location.path),
    };
    let interface_note = match &cap.interface.capture {
        Some(c) if cap.interface.source.starts_with("help_output") => format!(
            " Interface learned from `{} {}` captured at {} (parser v{}; usage: {}).",
            cap.name,
            c.method.join(" "),
            c.captured_at,
            c.parser_version,
            c.usage_line.as_deref().unwrap_or("unknown")
        ),
        _ => String::new(),
    };
    let rationale = format!(
        "{} {} ({where_from}){}.{interface_note}",
        cap.id,
        cap.purpose
            .as_ref()
            .map(|p| format!("— {}", p.value))
            .unwrap_or_else(|| "has no recorded description".into()),
        if args.len() > fixed {
            format!("; inputs bound: {}", args[fixed..].join(", "))
        } else {
            String::new()
        }
    );

    Resolution::Planned(Box::new(Draft {
        candidates,
        selected: cap.clone(),
        selection_reason,
        steps: vec![step],
        effects,
        verification,
        rationale,
        limitations,
        interface_capture: cap
            .interface
            .capture
            .clone()
            .filter(|_| cap.interface.source.starts_with("help_output")),
    }))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn option_for(cap: &Capability, cand: &Candidate) -> ClarificationOption {
    ClarificationOption {
        capability_id: cap.id.clone(),
        summary: cap
            .purpose
            .as_ref()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| format!("{} at {}", cap.name, cap.location.path)),
        invocation: cand.invocation.clone(),
    }
}

fn interface_invocation(cap: &Capability) -> Vec<String> {
    let mut v = vec![cap.interface.executable.clone()];
    v.extend(cap.interface.arguments.iter().cloned());
    v
}

fn name_stems(name: &str) -> Vec<String> {
    words(name).iter().map(|w| stem(w)).collect()
}

fn score_capability(
    cap: &Capability,
    intent: &BTreeSet<String>,
    raw_words: &[String],
    mentions_project: bool,
    ctx: &ResolveContext<'_>,
) -> (f64, Vec<String>) {
    let mut score = 0.0;
    let mut reasons = Vec::new();
    let names = name_stems(&cap.name);
    let mut counted: BTreeSet<String> = BTreeSet::new();

    let mut direct = 0;
    let mut via_syn = 0;
    for n in &names {
        if intent.contains(n) {
            direct += 1;
            counted.insert(n.clone());
        } else if let Some(m) = equivalents(n).into_iter().find(|e| intent.contains(e)) {
            via_syn += 1;
            counted.insert(m);
        }
    }
    let matched = direct + via_syn;
    if matched > 0 && matched == names.len() {
        // More specific names that are fully matched rank higher.
        score += 6.0 + (names.len() - 1) as f64;
        reasons.push(if via_syn > 0 {
            format!("intent term matches name `{}` (synonym)", cap.name)
        } else {
            format!("intent names `{}`", cap.name)
        });
    } else if matched > 0 {
        score += 3.0 * direct as f64 + 2.0 * via_syn as f64 - (names.len() - matched) as f64;
        reasons.push(format!("intent partially matches name `{}`", cap.name));
    } else {
        let lname = cap.name.to_lowercase();
        if let Some(s) = intent
            .iter()
            .filter(|s| s.len() >= 4 && lname.contains(s.as_str()))
            .max_by_key(|s| s.len())
        {
            // Scaled by how much of the name the term covers.
            score += 1.0 + 2.0 * (s.len() as f64 / lname.len().max(1) as f64);
            counted.insert(s.clone());
            reasons.push(format!("name `{}` contains `{s}`", cap.name));
        }
    }

    // Description / help text.
    let mut text = cap
        .purpose
        .as_ref()
        .map(|p| p.value.clone())
        .unwrap_or_default();
    if let Some(h) = &cap.help {
        text.push(' ');
        text.extend(h.excerpt.chars().take(600));
    }
    if !text.is_empty() {
        let ptoks = content_stems(&text);
        let mut hits = Vec::new();
        for s in intent.iter().filter(|s| !counted.contains(*s)) {
            if equivalents(s).iter().any(|e| ptoks.contains(e)) {
                hits.push(s.clone());
            }
        }
        // Without any name match, the description must cover at least half
        // of what the intent asks for; one shared word is not enough.
        let name_matched = score > 0.0;
        let covers = !intent.is_empty() && hits.len() * 2 >= intent.len();
        if !hits.is_empty() && (name_matched || covers) {
            let add = (1.5 * hits.len() as f64).min(4.5);
            score += add;
            reasons.push(format!("description mentions {}", hits.join(", ")));
        }
    }

    let relevant = score > 0.0;
    if cap.kind == CapabilityKind::ProjectTask {
        if raw_words.contains(&cap.interface.executable) && relevant {
            score += 2.0;
            reasons.push(format!("intent mentions `{}`", cap.interface.executable));
        }
        let local =
            ctx.project_root.is_some() && cap.location.project_root.as_deref() == ctx.project_root;
        if local && relevant {
            score += 1.5;
            reasons.push("defined by the current project".into());
            if mentions_project {
                score += 1.0;
                reasons.push("intent refers to the project".into());
            }
        }
    }
    if relevant {
        if let Some(h) = ctx.history.get(&cap.id) {
            if h.successes > 0 {
                score += 0.25 * h.successes.min(4) as f64;
                reasons.push(format!("observed {} successful run(s)", h.successes));
            }
        }
    }
    (score, reasons)
}

fn runner_location(
    cap: &Capability,
    env: &BTreeMap<&str, &Capability>,
) -> Option<(String, String)> {
    match cap.kind {
        CapabilityKind::Executable => Some((cap.location.path.clone(), cap.fingerprint.clone())),
        _ => env
            .get(format!("executable:{}", cap.interface.executable).as_str())
            .map(|r| (r.location.path.clone(), r.fingerprint.clone())),
    }
}

fn build_arguments(
    cap: &Capability,
    values: &[String],
    subject: Option<&str>,
    bindings: &BTreeMap<String, String>,
    env: &BTreeMap<&str, &Capability>,
) -> Result<Vec<String>, Unbuildable> {
    if runner_location(cap, env).is_none() {
        return Err(Unbuildable::RunnerMissing(cap.interface.executable.clone()));
    }
    match cap.interface.completeness {
        InterfaceCompleteness::Unknown => {
            return Err(match &cap.interface.capture {
                Some(c) => Unbuildable::CaptureFailed(format!(
                    "{:?}: {}",
                    c.status,
                    c.uncertainties.join("; ")
                )),
                None => Unbuildable::InterfaceUnknown,
            })
        }
        InterfaceCompleteness::Partial => {
            let uncertain = cap
                .interface
                .capture
                .as_ref()
                .map(|c| c.uncertainties.join("; "))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "the usage form was not recognized".into());
            return Err(Unbuildable::InterfacePartial(format!(
                "{} option(s) recognized; {uncertain}",
                cap.interface.options.len()
            )));
        }
        _ => {}
    }
    for name in bindings.keys() {
        if !cap.interface.parameters.iter().any(|p| &p.name == name) {
            return Err(Unbuildable::BadBinding(format!(
                "`{}` has no parameter named `{name}`",
                cap.id
            )));
        }
    }
    // Values from help-derived interfaces are bound by what the help says each
    // positional is; nothing is added that the request did not supply.
    let from_help = cap.interface.source.starts_with("help_output");
    let mut args = cap.interface.arguments.clone();
    let mut pool: Vec<&String> = values.iter().collect();
    let mut missing = Vec::new();
    let mut subject_used = false;
    for p in &cap.interface.parameters {
        if let Some(v) = bindings.get(&p.name) {
            pool.retain(|x| *x != v);
            args.push(v.clone());
            continue;
        }
        if from_help && p.role() == ParameterRole::SearchTerm {
            // A quoted value wins; otherwise a subject the request states plainly.
            let quoted = pool.iter().position(|v| !is_path_like(v));
            if let Some(i) = quoted {
                args.push(pool.remove(i).clone());
            } else if let (Some(sub), false) = (subject, subject_used) {
                subject_used = true;
                args.push(sub.to_string());
            } else {
                missing.push(p.name.clone());
            }
            continue;
        }
        if p.variadic {
            if pool.is_empty()
                && from_help
                && p.role() == ParameterRole::Path
                && p.omitted_means.is_none()
                && !p.required
            {
                missing.push(p.name.clone());
            }
            args.extend(pool.drain(..).cloned());
            continue;
        }
        if !pool.is_empty() {
            args.push(pool.remove(0).clone());
        } else if p.required {
            missing.push(p.name.clone());
        } else if from_help && p.role() == ParameterRole::Path && p.omitted_means.is_none() {
            // Omitting an optional input path may make the tool read stdin or
            // do something the help never described. Do not assume.
            missing.push(p.name.clone());
        } else {
            break; // optional positionals after a gap cannot be supplied
        }
    }
    if !missing.is_empty() {
        return Err(Unbuildable::MissingInputs(missing));
    }
    if from_help && !pool.is_empty() {
        return Err(Unbuildable::BadBinding(format!(
            "the value(s) {} were given but `{}` has no positional parameter to receive them",
            pool.iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            cap.name
        )));
    }
    for a in &args[cap.interface.arguments.len()..] {
        if a.starts_with('-') {
            return Err(Unbuildable::BadBinding(format!(
                "value {a:?} looks like a flag; cli2 only passes documented flags"
            )));
        }
    }
    Ok(args)
}

//! `cli2 model benchmark`: runs `evals/apple-foundation-models.jsonl` against
//! real discovered inventories (real PATH plus the fixture projects), comparing
//! deterministic and Apple ranking over the *same* candidate set. Every number
//! comes from an actual run; nothing is estimated.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::engine::{Config, Engine, EngineError, NonInteractive, RunOptions};
use crate::semantic::status::model_status;
use crate::semantic::{ResolverMode, ResolverSettings};
use cli2_core::RequestInput;

pub const BENCH_SCHEMA: &str = "cli2.model_benchmark.v1";

#[derive(Debug, Clone, Serialize)]
pub struct CaseReport {
    pub id: String,
    pub intent: String,
    pub outcome_expected: String,
    pub skipped: Option<String>,
    pub semantic_status: Option<String>,
    pub deterministic_status: Option<String>,
    pub fallback: bool,
    pub model_state: Option<String>,
    pub deterministic_order: Vec<String>,
    pub semantic_order: Vec<String>,
    pub deterministic_top1: Option<bool>,
    pub semantic_top1: Option<bool>,
    pub deterministic_top3: Option<bool>,
    pub semantic_top3: Option<bool>,
    /// Ranking quality (did the ordering/outcome match expectation).
    pub ranking_ok: Option<bool>,
    /// Plan-construction quality, separate from ranking.
    pub plan_ok: Option<bool>,
    pub selected: Option<String>,
    pub invalid_response: bool,
    pub invalid_plan: bool,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub schema_version: &'static str,
    pub model_ready: bool,
    pub model_state: String,
    pub cases_total: usize,
    pub cases_run: usize,
    pub cases_skipped: usize,
    pub top1_accuracy_deterministic: Option<f64>,
    pub top1_accuracy_semantic: Option<f64>,
    pub top3_recall_deterministic: Option<f64>,
    pub top3_recall_semantic: Option<f64>,
    pub invalid_capability_reference_rate: Option<f64>,
    pub invalid_plan_rate: Option<f64>,
    pub unnecessary_clarification_rate: Option<f64>,
    pub missed_ambiguity_rate: Option<f64>,
    pub ranking_pass_semantic: Option<f64>,
    pub latency_ms_mean: Option<f64>,
    pub latency_ms_p50: Option<u64>,
    pub inference_availability_rate: Option<f64>,
    pub deterministic_fallback_rate: Option<f64>,
    pub cases: Vec<CaseReport>,
}

fn rate(n: usize, d: usize) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

fn matches(accept: &[String], id: &str) -> bool {
    accept.iter().any(|a| match a.strip_prefix("name:") {
        Some(n) => id.strip_prefix("executable:") == Some(n),
        None => a == id,
    })
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            if e.file_name() == "target" || e.file_name() == "node_modules" {
                continue;
            }
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

fn status_of(v: &Value) -> String {
    v["status"].as_str().unwrap_or("?").to_string()
}

pub fn run_benchmark(
    repo_root: &Path,
    cases_file: &Path,
    settings: &ResolverSettings,
) -> Result<BenchReport, EngineError> {
    let text = std::fs::read_to_string(cases_file)?;
    let cases: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .map_err(|e| EngineError::Invalid(format!("{}: {e}", cases_file.display())))
        })
        .collect::<Result<_, _>>()?;

    let status = model_status(settings);
    let tmp: PathBuf = std::env::temp_dir().join(format!("cli2-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let work = tmp.join("work");
    copy_dir(&repo_root.join("fixtures"), &work)?;
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    path.push(work.join("tools"));
    let mut config = Config::from_env()?;
    config.home = tmp.join("home");
    config.path_var =
        std::env::join_paths(path).map_err(|e| EngineError::Invalid(e.to_string()))?;
    let mut engine_settings = settings.clone();
    engine_settings.provider = crate::semantic::Provider::AppleFoundationModels;
    config.resolver = Some(engine_settings);
    let mut engine = Engine::open(config)?;
    let opts = RunOptions {
        execute: false,
        yes: false,
        approve_token: None,
        timeout_secs: None,
        capture_help: false,
        interactive: false,
        ..Default::default()
    };

    let mut reports = Vec::new();
    for case in &cases {
        let id = case["id"].as_str().unwrap_or("?").to_string();
        let intent = case["intent"].as_str().unwrap_or("").to_string();
        let exp = &case["expect"];
        let outcome = exp["outcome"].as_str().unwrap_or("rank").to_string();
        let accept: Vec<String> = exp["accept"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut r = CaseReport {
            id,
            intent: intent.clone(),
            outcome_expected: outcome.clone(),
            skipped: None,
            semantic_status: None,
            deterministic_status: None,
            fallback: false,
            model_state: None,
            deterministic_order: vec![],
            semantic_order: vec![],
            deterministic_top1: None,
            semantic_top1: None,
            deterministic_top3: None,
            semantic_top3: None,
            ranking_ok: None,
            plan_ok: None,
            selected: None,
            invalid_response: false,
            invalid_plan: false,
            latency_ms: 0,
        };
        let missing: Vec<&str> = case["requires"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|t| !which(t))
                    .collect()
            })
            .unwrap_or_default();
        if !missing.is_empty() {
            r.skipped = Some(format!("missing tool(s): {}", missing.join(", ")));
            reports.push(r);
            continue;
        }
        if !status.ready {
            r.skipped = Some(format!("model not ready ({})", status.state));
            reports.push(r);
            continue;
        }
        let dir = work.join(case["fixture"].as_str().unwrap_or("node-project"));
        let mk = || {
            let mut req = RequestInput::new(intent.clone());
            req.context.working_directory = Some(dir.display().to_string());
            if let Some(m) = case["constraints"]["modify_files"].as_bool() {
                req.constraints.modify_files = Some(m);
            }
            req
        };
        engine.set_resolver_mode(ResolverMode::Deterministic);
        let det = engine.run(mk(), &opts, &mut NonInteractive)?;
        let det = serde_json::to_value(&det).unwrap_or_default();
        engine.set_resolver_mode(ResolverMode::AppleFm);
        let started = std::time::Instant::now();
        let sem = engine.run(mk(), &opts, &mut NonInteractive);
        r.latency_ms = started.elapsed().as_millis() as u64;
        r.deterministic_status = Some(status_of(&det));
        let sem = match sem {
            Ok(v) => serde_json::to_value(&v).unwrap_or_default(),
            Err(e) => {
                r.fallback = true;
                r.semantic_status = Some(format!("error: {e}"));
                reports.push(r);
                continue;
            }
        };
        r.semantic_status = Some(status_of(&sem));
        r.selected = sem["plan"]["steps"][0]["capability_id"]
            .as_str()
            .map(String::from);
        let ev = engine.last_semantic().cloned();
        if let Some(ev) = &ev {
            r.fallback = ev.fallback;
            if let Some(rep) = &ev.report {
                r.deterministic_order = rep.order_before.clone();
                r.semantic_order = rep.order_after.clone();
                r.invalid_response = rep.validation.starts_with("rejected")
                    || rep.error.as_deref().is_some_and(|e| {
                        e.contains("invalid_model_output") || e.contains("invalid_response")
                    });
                if let Some(sel) = &r.selected {
                    r.invalid_plan =
                        !rep.submitted.contains(sel) && !rep.order_before.contains(sel);
                }
            }
        }
        let top = |v: &[String], n: usize| v.iter().take(n).any(|c| matches(&accept, c));
        if !accept.is_empty() {
            r.deterministic_top1 = Some(top(&r.deterministic_order, 1));
            r.deterministic_top3 = Some(top(&r.deterministic_order, 3));
            r.semantic_top1 = Some(top(&r.semantic_order, 1));
            r.semantic_top3 = Some(top(&r.semantic_order, 3));
        }
        let planned = r.semantic_status.as_deref() == Some("planned");
        let approval = sem["plan"]["approval"]["level"].as_str().unwrap_or("");
        let clarif = r.semantic_status.as_deref() == Some("needs_clarification");
        let options = sem["clarification"]["options"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        r.ranking_ok = Some(match outcome.as_str() {
            "rank" => r.semantic_top1 == Some(true),
            "rank_or_inadequate" => r.semantic_top1 == Some(true) || !planned,
            "inadequate" => !planned,
            "ambiguous" => clarif && options >= 2,
            "gated" => !planned || matches!(approval, "explicit" | "forbidden"),
            // Unknown or uncertain effects must never run ungated, whatever the model said.
            "gated_unknown" => !planned || approval != "none",
            _ => false,
        });
        r.plan_ok = Some(match outcome.as_str() {
            "rank" => {
                planned
                    && r.selected.as_deref().is_some_and(|s| matches(&accept, s))
                    && exp["approval"].as_str().is_none_or(|a| a == approval)
            }
            _ => r.ranking_ok == Some(true),
        });
        reports.push(r);
    }
    let _ = std::fs::remove_dir_all(&tmp);

    let ran: Vec<&CaseReport> = reports.iter().filter(|c| c.skipped.is_none()).collect();
    let count = |f: &dyn Fn(&CaseReport) -> bool| ran.iter().filter(|c| f(c)).count();
    let with_accept: Vec<&&CaseReport> = ran.iter().filter(|c| c.semantic_top1.is_some()).collect();
    let rank_cases: Vec<&&CaseReport> = ran
        .iter()
        .filter(|c| c.outcome_expected == "rank")
        .collect();
    let amb: Vec<&&CaseReport> = ran
        .iter()
        .filter(|c| c.outcome_expected == "ambiguous")
        .collect();
    let mut lat: Vec<u64> = ran.iter().map(|c| c.latency_ms).collect();
    lat.sort();
    let attempted = ran.len();
    Ok(BenchReport {
        schema_version: BENCH_SCHEMA,
        model_ready: status.ready,
        model_state: status.state.clone(),
        cases_total: reports.len(),
        cases_run: attempted,
        cases_skipped: reports.len() - attempted,
        top1_accuracy_deterministic: rate(
            with_accept
                .iter()
                .filter(|c| c.deterministic_top1 == Some(true))
                .count(),
            with_accept.len(),
        ),
        top1_accuracy_semantic: rate(
            with_accept
                .iter()
                .filter(|c| c.semantic_top1 == Some(true))
                .count(),
            with_accept.len(),
        ),
        top3_recall_deterministic: rate(
            with_accept
                .iter()
                .filter(|c| c.deterministic_top3 == Some(true))
                .count(),
            with_accept.len(),
        ),
        top3_recall_semantic: rate(
            with_accept
                .iter()
                .filter(|c| c.semantic_top3 == Some(true))
                .count(),
            with_accept.len(),
        ),
        invalid_capability_reference_rate: rate(
            count(&|c: &CaseReport| c.invalid_response),
            attempted,
        ),
        invalid_plan_rate: rate(count(&|c: &CaseReport| c.invalid_plan), attempted),
        unnecessary_clarification_rate: rate(
            rank_cases
                .iter()
                .filter(|c| c.semantic_status.as_deref() == Some("needs_clarification"))
                .count(),
            rank_cases.len(),
        ),
        missed_ambiguity_rate: rate(
            amb.iter()
                .filter(|c| c.semantic_status.as_deref() != Some("needs_clarification"))
                .count(),
            amb.len(),
        ),
        ranking_pass_semantic: rate(
            count(&|c: &CaseReport| c.ranking_ok == Some(true)),
            attempted,
        ),
        latency_ms_mean: (!lat.is_empty())
            .then(|| lat.iter().sum::<u64>() as f64 / lat.len() as f64),
        latency_ms_p50: lat.get(lat.len() / 2).copied(),
        inference_availability_rate: rate(count(&|c| !c.fallback), attempted),
        deterministic_fallback_rate: rate(count(&|c: &CaseReport| c.fallback), attempted),
        cases: reports,
    })
}

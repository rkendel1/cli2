//! Evaluating history retrieval in isolation.
//!
//! Retrieval only decides which recorded requests are *potentially relevant*.
//! It never establishes validity or authorization: that is validation
//! (`workflow::validate`), re-planning and authority policy. This suite
//! therefore measures only whether the right history is surfaced and the
//! wrong history is not, for paraphrases, similar wording with a different
//! action, different targets, absent history, and requests that look alike
//! but are unsafe to reuse.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::EngineError;
use crate::workflow::search::{self, MIN_MATCH};

pub const MATCHING_SCHEMA: &str = "cli2.matching_report.v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistItem {
    label: String,
    intent: String,
    /// Informational: validity is judged elsewhere; retrieval must still surface it.
    #[serde(default)]
    #[allow(dead_code)]
    stale: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MExpect {
    /// History that should be retrieved.
    retrieve: Vec<String>,
    /// History that must not be retrieved.
    reject: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MCase {
    id: String,
    category: String,
    query: String,
    history: Vec<HistItem>,
    expect: MExpect,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchCaseResult {
    pub id: String,
    pub category: String,
    pub query: String,
    /// Retrieved labels, best first, with their scores.
    pub retrieved: Vec<(String, f64)>,
    pub missed: Vec<String>,
    pub false_positives: Vec<String>,
    pub top1_correct: Option<bool>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStats {
    pub cases: usize,
    pub passed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatcherResult {
    pub matcher: String,
    pub cases: Vec<MatchCaseResult>,
    /// Expected history retrieved / expected history.
    pub recall: super::Rate,
    /// Wrong history retrieved / history that should have been rejected.
    pub false_positives: super::Rate,
    /// The best retrieved item is an expected one, among cases that expect any.
    pub top1: super::Rate,
    pub by_category: BTreeMap<String, CategoryStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchingReport {
    pub schema_version: &'static str,
    pub threshold: f64,
    pub cases: usize,
    /// The original stem-overlap matcher, kept as the baseline.
    pub baseline: MatcherResult,
    /// The matcher cli2 uses now.
    pub current: MatcherResult,
    pub fingerprint: String,
}

type Scorer = fn(&str, &str) -> search::Similarity;

fn evaluate(name: &str, cases: &[MCase], score: Scorer) -> MatcherResult {
    let mut results = Vec::new();
    let (mut rec_ok, mut rec_of, mut fp, mut fp_of, mut t1, mut t1_of) = (0, 0, 0, 0, 0, 0);
    let mut by_category: BTreeMap<String, CategoryStats> = BTreeMap::new();
    for c in cases {
        let mut retrieved: Vec<(String, f64)> = c
            .history
            .iter()
            .map(|h| (h.label.clone(), score(&c.query, &h.intent).score))
            .filter(|(_, s)| *s >= MIN_MATCH)
            .collect();
        retrieved.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        let got = |l: &str| retrieved.iter().any(|(r, _)| r == l);
        let missed: Vec<String> = c
            .expect
            .retrieve
            .iter()
            .filter(|l| !got(l))
            .cloned()
            .collect();
        let false_positives: Vec<String> =
            c.expect.reject.iter().filter(|l| got(l)).cloned().collect();
        rec_of += c.expect.retrieve.len();
        rec_ok += c.expect.retrieve.len() - missed.len();
        fp_of += c.expect.reject.len();
        fp += false_positives.len();
        let top1 = (!c.expect.retrieve.is_empty()).then(|| {
            retrieved
                .first()
                .is_some_and(|(l, _)| c.expect.retrieve.contains(l))
        });
        if let Some(t) = top1 {
            t1_of += 1;
            t1 += t as usize;
        }
        let passed = missed.is_empty() && false_positives.is_empty();
        let e = by_category
            .entry(c.category.clone())
            .or_insert(CategoryStats {
                cases: 0,
                passed: 0,
            });
        e.cases += 1;
        e.passed += passed as usize;
        results.push(MatchCaseResult {
            id: c.id.clone(),
            category: c.category.clone(),
            query: c.query.clone(),
            retrieved,
            missed,
            false_positives,
            top1_correct: top1,
            passed,
        });
    }
    MatcherResult {
        matcher: name.into(),
        cases: results,
        recall: super::Rate {
            passed: rec_ok,
            of: rec_of,
        },
        false_positives: super::Rate {
            passed: fp,
            of: fp_of,
        },
        top1: super::Rate {
            passed: t1,
            of: t1_of,
        },
        by_category,
    }
}

pub fn run_matching(path: &std::path::Path) -> Result<MatchingReport, EngineError> {
    let text = std::fs::read_to_string(path)?;
    let mut cases = Vec::new();
    for (i, l) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        cases.push(serde_json::from_str::<MCase>(l).map_err(|e| {
            EngineError::Invalid(format!("{} line {}: {e}", path.display(), i + 1))
        })?);
    }
    let baseline = evaluate("stem_overlap_v1", &cases, search::similarity_v1);
    let current = evaluate(search::MATCHER_VERSION, &cases, search::similarity);
    let mut h = Sha256::new();
    for m in [&baseline, &current] {
        for c in &m.cases {
            h.update(
                format!(
                    "{}|{}|{:?}|{:?}|{:?}",
                    m.matcher, c.id, c.retrieved, c.missed, c.false_positives
                )
                .as_bytes(),
            );
        }
    }
    Ok(MatchingReport {
        schema_version: MATCHING_SCHEMA,
        threshold: MIN_MATCH,
        cases: cases.len(),
        baseline,
        current,
        fingerprint: hex::encode(h.finalize()),
    })
}

pub fn render(r: &MatchingReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "\nHistory matching ({} cases, threshold {}). Retrieval only: validity and authority are checked separately.", r.cases, r.threshold);
    for m in [&r.baseline, &r.current] {
        let _ = writeln!(
            s,
            "  {:<18} recall {}/{}   false positives {}/{}   top-1 {}/{}   cases passed {}/{}",
            m.matcher,
            m.recall.passed,
            m.recall.of,
            m.false_positives.passed,
            m.false_positives.of,
            m.top1.passed,
            m.top1.of,
            m.cases.iter().filter(|c| c.passed).count(),
            m.cases.len()
        );
    }
    let _ = writeln!(
        s,
        "  by category (cases passed, {} → {}):",
        r.baseline.matcher, r.current.matcher
    );
    for (cat, b) in &r.baseline.by_category {
        let c = &r.current.by_category[cat];
        let _ = writeln!(
            s,
            "    {cat:<26} {}/{} → {}/{}",
            b.passed, b.cases, c.passed, c.cases
        );
    }
    for c in r.current.cases.iter().filter(|c| !c.passed) {
        let _ = writeln!(
            s,
            "    still failing: {:<18} {:?}  missed {:?}  false positives {:?}",
            c.id, c.query, c.missed, c.false_positives
        );
    }
    s
}

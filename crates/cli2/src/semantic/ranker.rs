//! [`SemanticRanker`] backed by Apple Foundation Models via the Swift bridge.
//!
//! The ranker can only *re-score candidates it was handed*. A relevance
//! judgment becomes a bounded score adjustment; viability, invocations,
//! effects and approval are never read from the model and cannot be changed
//! by it. Any failure leaves scores untouched and is reported as an error so
//! the caller can fall back or fail visibly.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use cli2_core::{Candidate, Capability, EffectCertainty, NetworkConstraint, RecordedRequest};
use cli2_resolver::{ReportedRanking, SemanticRanker, SemanticReport, MIN_SCORE};

use super::bridge::{
    BridgeCandidate, BridgeClient, BridgeConstraints, BridgeRequest, BridgeResponse, REQUEST_SCHEMA,
};
use super::validate::{validate_ranking, validate_terms, Outcome, Relevance, Validated};
use super::{ResolverSettings, PROVIDER_ID};

/// Score added for each relevance judgment.
const BONUS_HIGH: f64 = 8.0;
const BONUS_MEDIUM: f64 = 5.0;
const BONUS_LOW: f64 = 1.0;
/// A candidate the model judged irrelevant, or all candidates when nothing
/// fits, cannot stay selectable through lexical overlap alone.
const DEMOTED_CEILING: f64 = MIN_SCORE - 1.0;
/// Keeps the model's order among equal relevance.
const RANK_STEP: f64 = 0.05;
/// Scores at or above this are deterministic decisions (exact match, caller
/// selection) and are never altered.
const DECIDED: f64 = 100.0;

struct Inner {
    client: BridgeClient,
    max_candidates: usize,
    report: RefCell<Option<SemanticReport>>,
    expansion: RefCell<Vec<String>>,
    seq: RefCell<u32>,
}

#[derive(Clone)]
pub struct AppleFmRanker(Rc<Inner>);

impl AppleFmRanker {
    pub fn new(settings: &ResolverSettings) -> Self {
        Self::with_client(
            BridgeClient::new(
                settings.bridge_path.clone(),
                settings.timeout_secs,
                settings.max_response_bytes,
            ),
            settings.max_candidates,
        )
    }

    pub fn with_client(client: BridgeClient, max_candidates: usize) -> Self {
        Self(Rc::new(Inner {
            client,
            max_candidates,
            report: RefCell::new(None),
            expansion: RefCell::new(vec![]),
            seq: RefCell::new(0),
        }))
    }

    fn bridge_id(&self, request_id: &str, op: &str) -> String {
        let mut s = self.0.seq.borrow_mut();
        *s += 1;
        format!("{request_id}:{op}:{}", *s)
    }

    /// Ask the model for search terms to widen retrieval. The terms are only
    /// matched against names in the real inventory. On failure, a failed
    /// report is stored so evidence records why.
    pub fn expand_terms(&self, request: &RecordedRequest) -> Result<Vec<String>, String> {
        let started = Instant::now();
        let id = self.bridge_id(&request.request_id, "expand");
        let req = BridgeRequest {
            schema_version: REQUEST_SCHEMA,
            request_id: id.clone(),
            operation: "expand",
            intent: Some(request.intent.clone()),
            constraints: None,
            candidates: vec![],
        };
        let result = self
            .0
            .client
            .call(&req)
            .map_err(|e| (e.code().to_string(), e.to_string(), None))
            .and_then(|resp| {
                validate_terms(&resp, &id)
                    .map(|t| (t, resp.bridge_version.clone()))
                    .map_err(|e| {
                        (
                            "invalid_response".to_string(),
                            e.to_string(),
                            resp.availability.map(|a| a.state),
                        )
                    })
            });
        match result {
            Ok((terms, version)) => {
                *self.0.expansion.borrow_mut() = terms.clone();
                let mut report = self.0.report.borrow_mut();
                let r = report.get_or_insert_with(|| blank_report(&id, version.clone()));
                r.expansion_terms = terms.clone();
                r.latency_ms += started.elapsed().as_millis() as u64;
                r.bridge_version = version.or(r.bridge_version.take());
                Ok(terms)
            }
            Err((code, msg, availability)) => {
                let mut r = blank_report(&id, None);
                r.status = "failed".into();
                r.validation = "not_run".into();
                r.error = Some(format!("{code}: {msg}"));
                r.availability = availability;
                r.latency_ms = started.elapsed().as_millis() as u64;
                *self.0.report.borrow_mut() = Some(r);
                Err(msg)
            }
        }
    }

    fn fail(
        &self,
        mut report: SemanticReport,
        started: Instant,
        code: &str,
        msg: String,
        validation: &str,
    ) -> Result<(), String> {
        report.status = "failed".into();
        report.validation = validation.into();
        report.error = Some(format!("{code}: {msg}"));
        report.latency_ms += started.elapsed().as_millis() as u64;
        merge_report(&self.0.report, report);
        Err(msg)
    }
}

fn blank_report(bridge_request_id: &str, bridge_version: Option<String>) -> SemanticReport {
    SemanticReport {
        schema_version: SemanticReport::SCHEMA,
        provider: PROVIDER_ID.into(),
        bridge_version,
        bridge_request_id: bridge_request_id.into(),
        expansion_terms: vec![],
        submitted: vec![],
        order_before: vec![],
        order_after: vec![],
        status: "ranked".into(),
        validation: "valid".into(),
        rankings: vec![],
        clarification: None,
        availability: None,
        latency_ms: 0,
        error: None,
    }
}

/// Keep expansion data and latency from an earlier phase of the same request.
fn merge_report(slot: &RefCell<Option<SemanticReport>>, mut new: SemanticReport) {
    if let Some(prev) = slot.borrow_mut().take() {
        if new.expansion_terms.is_empty() {
            new.expansion_terms = prev.expansion_terms;
        }
        new.latency_ms += prev.latency_ms;
        if new.bridge_version.is_none() {
            new.bridge_version = prev.bridge_version;
        }
    }
    *slot.borrow_mut() = Some(new);
}

fn effects_summary(cap: &Capability) -> String {
    match cap.effects.certainty {
        EffectCertainty::Unknown => "unknown (not characterized)".into(),
        c => {
            let classes: Vec<String> = cap
                .effects
                .classes
                .iter()
                .map(|e| {
                    serde_json::to_value(e)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                })
                .collect();
            format!(
                "{} ({})",
                classes.join(", "),
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            )
        }
    }
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn bridge_candidate(cap: &Capability) -> BridgeCandidate {
    BridgeCandidate {
        id: cap.id.clone(),
        name: cap.name.clone(),
        kind: enum_str(&cap.kind),
        purpose: cap.purpose.as_ref().map(|p| p.value.clone()),
        interface_completeness: enum_str(&cap.interface.completeness),
        effects: effects_summary(cap),
        help_excerpt: cap
            .help
            .as_ref()
            .map(|h| h.excerpt.chars().take(400).collect()),
    }
}

/// One line of explanation from the model, safe to print.
fn clean_reason(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

impl SemanticRanker for AppleFmRanker {
    fn name(&self) -> &str {
        PROVIDER_ID
    }

    fn rerank(&self, _: &str, _: &mut [Candidate], _: &[&Capability]) -> Result<(), String> {
        Err("semantic ranking needs the full request; use rerank_request".into())
    }

    fn wants_wide_retrieval(&self) -> bool {
        true
    }

    fn take_report(&self) -> Option<SemanticReport> {
        self.0.report.borrow_mut().take()
    }

    fn rerank_request(
        &self,
        request: &RecordedRequest,
        candidates: &mut [Candidate],
        capabilities: &[&Capability],
    ) -> Result<(), String> {
        let started = Instant::now();
        let order_before: Vec<String> =
            candidates.iter().map(|c| c.capability_id.clone()).collect();
        // Candidates arrive best-first; the model sees only the top slice.
        let n = candidates.len().min(self.0.max_candidates);
        let submitted: Vec<String> = order_before[..n].to_vec();
        let bridge_id = self.bridge_id(&request.request_id, "rank");
        let mut report = blank_report(&bridge_id, None);
        report.submitted = submitted.clone();
        report.order_before = order_before.clone();
        report.order_after = order_before.clone();

        let req = BridgeRequest {
            schema_version: REQUEST_SCHEMA,
            request_id: bridge_id.clone(),
            operation: "rank",
            intent: Some(request.intent.clone()),
            constraints: Some(BridgeConstraints {
                modify_files: request.constraints.modify_files,
                network: match request.constraints.network {
                    Some(NetworkConstraint::Allow) => "allow",
                    Some(NetworkConstraint::Deny) => "deny",
                    _ => "unknown",
                }
                .into(),
            }),
            candidates: capabilities[..n]
                .iter()
                .map(|c| bridge_candidate(c))
                .collect(),
        };

        let resp: BridgeResponse = match self.0.client.call(&req) {
            Ok(r) => r,
            Err(e) => {
                return self.fail(report, started, e.code(), e.to_string(), "not_run");
            }
        };
        report.bridge_version = resp.bridge_version.clone();
        report.availability = resp.availability.as_ref().map(|a| a.state.clone());
        let validated = match validate_ranking(&resp, &bridge_id, &submitted) {
            Ok(v) => v,
            Err(e) => {
                return self.fail(
                    report,
                    started,
                    "invalid_response",
                    e.to_string(),
                    &format!("rejected: {e}"),
                );
            }
        };
        apply(&validated, &mut candidates[..n]);

        // Record the resulting order exactly as the resolver will sort it.
        let mut after: Vec<&Candidate> = candidates.iter().collect();
        after.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.capability_id.cmp(&b.capability_id))
        });
        report.order_after = after.iter().map(|c| c.capability_id.clone()).collect();
        report.status = validated.outcome.as_str().into();
        report.clarification = validated.clarification.clone();
        report.rankings = validated
            .rankings
            .iter()
            .map(|r| ReportedRanking {
                capability_id: r.capability_id.clone(),
                rank: r.rank,
                relevance: r.relevance.as_str().into(),
                reason: clean_reason(&r.reason),
            })
            .collect();
        report.latency_ms = started.elapsed().as_millis() as u64;
        merge_report(&self.0.report, report);
        Ok(())
    }
}

/// Turn validated judgments into bounded score adjustments. Only `score` and
/// explanatory `reasons` change; nothing else on a candidate is touched.
pub fn apply(v: &Validated, candidates: &mut [Candidate]) {
    for c in candidates.iter_mut() {
        if c.score >= DECIDED {
            continue;
        }
        if v.outcome == Outcome::NoMatch {
            if c.score > DEMOTED_CEILING {
                c.score = DEMOTED_CEILING;
            }
            c.reasons
                .push("semantic ranking: no submitted candidate fits this intent".into());
        }
    }
    let top_relevance = v.rankings.first().map(|r| r.relevance);
    for r in &v.rankings {
        let Some(c) = candidates
            .iter_mut()
            .find(|c| c.capability_id == r.capability_id)
        else {
            continue;
        };
        if c.score >= DECIDED {
            continue;
        }
        match r.relevance {
            Relevance::None => c.score = c.score.min(DEMOTED_CEILING),
            rel => {
                let bonus = match rel {
                    Relevance::High => BONUS_HIGH,
                    Relevance::Medium => BONUS_MEDIUM,
                    _ => BONUS_LOW,
                };
                // For a clarification, equally relevant top choices stay tied
                // so the resolver's own ambiguity handling asks the person.
                let step = if v.outcome == Outcome::Clarification && Some(rel) == top_relevance {
                    0.0
                } else {
                    RANK_STEP * (r.rank - 1) as f64
                };
                c.score += bonus - step;
            }
        }
        c.reasons.push(format!(
            "semantic ranking: {} (model rank {}): {}",
            r.relevance.as_str(),
            r.rank,
            clean_reason(&r.reason)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::validate::ValidRanking;
    use cli2_core::CapabilityKind;

    fn cand(id: &str, score: f64) -> Candidate {
        Candidate {
            capability_id: id.into(),
            kind: CapabilityKind::Executable,
            score,
            viable: false,
            reasons: vec![],
            invocation: None,
        }
    }

    fn ranking(id: &str, rank: u32, relevance: Relevance) -> ValidRanking {
        ValidRanking {
            capability_id: id.into(),
            rank,
            relevance,
            reason: "r".into(),
        }
    }

    #[test]
    fn high_relevance_lifts_a_zero_score_candidate_over_the_selection_threshold() {
        let mut cs = vec![cand("a", 0.0), cand("b", 4.0)];
        let v = Validated {
            outcome: Outcome::Ranked,
            rankings: vec![ranking("a", 1, Relevance::High)],
            clarification: None,
        };
        apply(&v, &mut cs);
        assert!(cs[0].score >= MIN_SCORE && cs[0].score > cs[1].score);
        assert!(
            !cs[0].viable && cs[0].invocation.is_none(),
            "viability and invocation are untouched"
        );
    }

    #[test]
    fn no_match_demotes_but_never_touches_an_exact_match() {
        let mut cs = vec![cand("exact", 100.0), cand("weak", 5.0)];
        let v = Validated {
            outcome: Outcome::NoMatch,
            rankings: vec![],
            clarification: None,
        };
        apply(&v, &mut cs);
        assert_eq!(cs[0].score, 100.0);
        assert!(cs[1].score < MIN_SCORE);
    }

    #[test]
    fn clarification_keeps_equal_top_choices_tied() {
        let mut cs = vec![cand("a", 0.0), cand("b", 0.0)];
        let v = Validated {
            outcome: Outcome::Clarification,
            rankings: vec![
                ranking("a", 1, Relevance::High),
                ranking("b", 2, Relevance::High),
            ],
            clarification: Some("which?".into()),
        };
        apply(&v, &mut cs);
        assert_eq!(cs[0].score, cs[1].score);
    }
}

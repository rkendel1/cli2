//! Independent validation of the bridge's response. The runtime trusts nothing
//! the model said: every reference must be one the runtime supplied.

use std::collections::BTreeSet;

use super::bridge::{BridgeResponse, RESPONSE_SCHEMA};

pub const MAX_REASON_CHARS: usize = 500;
pub const MAX_CLARIFICATION_CHARS: usize = 500;
pub const MAX_TERMS: usize = 10;
pub const MAX_TERM_CHARS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    High,
    Medium,
    Low,
    None,
}

impl Relevance {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ranked,
    NoMatch,
    Clarification,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ranked => "ranked",
            Self::NoMatch => "no_match",
            Self::Clarification => "clarification",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidRanking {
    pub capability_id: String,
    pub rank: u32,
    pub relevance: Relevance,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub outcome: Outcome,
    pub rankings: Vec<ValidRanking>,
    pub clarification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ValidationError {
    #[error("unsupported response schema_version {0:?}")]
    Schema(String),
    #[error("response request_id {got:?} does not match {expected:?}")]
    RequestId { expected: String, got: String },
    #[error("the bridge reported {status}{}", .detail.as_ref().map(|d| format!(": {d}")).unwrap_or_default())]
    NotARanking {
        status: String,
        detail: Option<String>,
    },
    #[error("capability {0:?} was not among the submitted candidates")]
    UnknownCapability(String),
    #[error("capability {0:?} appears more than once")]
    Duplicate(String),
    #[error("ranks must be 1..n in order; got {0:?}")]
    BadRanks(Vec<i64>),
    #[error("unknown relevance {0:?}")]
    BadRelevance(String),
    #[error("{0} is longer than its limit")]
    TooLong(&'static str),
    #[error("inconsistent response: {0}")]
    Inconsistent(&'static str),
}

fn check_envelope(resp: &BridgeResponse, request_id: &str) -> Result<(), ValidationError> {
    if resp.schema_version != RESPONSE_SCHEMA {
        return Err(ValidationError::Schema(resp.schema_version.clone()));
    }
    if resp.request_id != request_id {
        return Err(ValidationError::RequestId {
            expected: request_id.into(),
            got: resp.request_id.clone(),
        });
    }
    Ok(())
}

/// Validate a ranking response against the candidate IDs that were submitted.
pub fn validate_ranking(
    resp: &BridgeResponse,
    request_id: &str,
    submitted: &[String],
) -> Result<Validated, ValidationError> {
    check_envelope(resp, request_id)?;
    let outcome = match resp.status.as_str() {
        "ranked" => Outcome::Ranked,
        "no_match" => Outcome::NoMatch,
        "clarification" => Outcome::Clarification,
        other => {
            return Err(ValidationError::NotARanking {
                status: other.into(),
                detail: resp
                    .error
                    .as_ref()
                    .map(|e| format!("{}: {}", e.code, e.message)),
            })
        }
    };
    let known: BTreeSet<&str> = submitted.iter().map(String::as_str).collect();
    let mut seen = BTreeSet::new();
    let mut rankings = Vec::new();
    for r in &resp.rankings {
        if !known.contains(r.capability_id.as_str()) {
            return Err(ValidationError::UnknownCapability(r.capability_id.clone()));
        }
        if !seen.insert(r.capability_id.as_str()) {
            return Err(ValidationError::Duplicate(r.capability_id.clone()));
        }
        let relevance = Relevance::parse(&r.relevance)
            .ok_or_else(|| ValidationError::BadRelevance(r.relevance.clone()))?;
        if r.reason.chars().count() > MAX_REASON_CHARS {
            return Err(ValidationError::TooLong("reason"));
        }
        rankings.push(ValidRanking {
            capability_id: r.capability_id.clone(),
            rank: 0,
            relevance,
            reason: r.reason.clone(),
        });
    }
    let ranks: Vec<i64> = resp.rankings.iter().map(|r| r.rank).collect();
    if ranks.iter().enumerate().any(|(i, r)| *r != i as i64 + 1) {
        return Err(ValidationError::BadRanks(ranks));
    }
    for (i, r) in rankings.iter_mut().enumerate() {
        r.rank = i as u32 + 1;
    }
    if rankings.len() > submitted.len() {
        return Err(ValidationError::Inconsistent(
            "more rankings than candidates",
        ));
    }
    match outcome {
        Outcome::Ranked if rankings.is_empty() => {
            return Err(ValidationError::Inconsistent("ranked without rankings"))
        }
        Outcome::NoMatch if !rankings.is_empty() => {
            return Err(ValidationError::Inconsistent("no_match with rankings"))
        }
        _ => {}
    }
    let clarification = resp.clarification.clone().filter(|c| !c.trim().is_empty());
    if let Some(c) = &clarification {
        if c.chars().count() > MAX_CLARIFICATION_CHARS {
            return Err(ValidationError::TooLong("clarification"));
        }
    }
    if outcome == Outcome::Clarification && clarification.is_none() {
        return Err(ValidationError::Inconsistent(
            "clarification without a question",
        ));
    }
    if outcome != Outcome::Clarification && clarification.is_some() {
        return Err(ValidationError::Inconsistent("unexpected clarification"));
    }
    Ok(Validated {
        outcome,
        rankings,
        clarification,
    })
}

/// Validate and re-sanitize query-expansion terms. Terms are only ever matched
/// against names in the real inventory.
pub fn validate_terms(
    resp: &BridgeResponse,
    request_id: &str,
) -> Result<Vec<String>, ValidationError> {
    check_envelope(resp, request_id)?;
    if resp.status != "expanded" {
        return Err(ValidationError::NotARanking {
            status: resp.status.clone(),
            detail: resp
                .error
                .as_ref()
                .map(|e| format!("{}: {}", e.code, e.message)),
        });
    }
    let terms = resp
        .terms
        .as_ref()
        .ok_or(ValidationError::Inconsistent("expanded without terms"))?;
    let mut out: Vec<String> = Vec::new();
    for t in terms {
        let t = t.trim().to_lowercase();
        let ok = (2..=MAX_TERM_CHARS).contains(&t.chars().count())
            && t.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._+-".contains(c));
        if ok && !out.contains(&t) {
            out.push(t);
        }
        if out.len() == MAX_TERMS {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::bridge::RawRanking;

    fn resp(status: &str, rankings: Vec<(&str, i64, &str)>) -> BridgeResponse {
        BridgeResponse {
            schema_version: RESPONSE_SCHEMA.into(),
            request_id: "r".into(),
            status: status.into(),
            rankings: rankings
                .into_iter()
                .map(|(id, rank, rel)| RawRanking {
                    capability_id: id.into(),
                    rank,
                    relevance: rel.into(),
                    reason: "because".into(),
                })
                .collect(),
            clarification: None,
            terms: None,
            availability: None,
            error: None,
            bridge_version: None,
            model: None,
            latency_ms: None,
        }
    }

    fn ids() -> Vec<String> {
        vec!["a".into(), "b".into()]
    }

    #[test]
    fn accepts_a_consistent_ranking() {
        let v = validate_ranking(
            &resp("ranked", vec![("b", 1, "high"), ("a", 2, "low")]),
            "r",
            &ids(),
        )
        .unwrap();
        assert_eq!(v.rankings[0].capability_id, "b");
    }

    #[test]
    fn rejects_every_documented_defect() {
        let cases: Vec<(BridgeResponse, &str)> = vec![
            (
                resp("ranked", vec![("zzz", 1, "high")]),
                "unknown capability",
            ),
            (
                resp("ranked", vec![("a", 1, "high"), ("a", 2, "low")]),
                "duplicate",
            ),
            (resp("ranked", vec![("a", 2, "high")]), "ranks"),
            (
                resp("ranked", vec![("a", 1, "high"), ("b", 1, "low")]),
                "ranks",
            ),
            (resp("ranked", vec![("a", 1, "certain")]), "relevance"),
            (resp("ranked", vec![]), "inconsistent"),
            (resp("no_match", vec![("a", 1, "high")]), "inconsistent"),
            (resp("clarification", vec![]), "inconsistent"),
            (resp("error", vec![]), "not a ranking"),
        ];
        for (r, why) in cases {
            assert!(validate_ranking(&r, "r", &ids()).is_err(), "{why}");
        }
        let mut wrong_schema = resp("ranked", vec![("a", 1, "high")]);
        wrong_schema.schema_version = "cli2.apple_fm.response.v9".into();
        assert!(validate_ranking(&wrong_schema, "r", &ids()).is_err());
        assert!(
            validate_ranking(&resp("ranked", vec![("a", 1, "high")]), "other", &ids()).is_err()
        );
    }

    #[test]
    fn terms_are_sanitized() {
        let mut r = resp("expanded", vec![]);
        r.terms = Some(vec![
            "Grep".into(),
            "rm -rf /".into(),
            "rg".into(),
            "x".into(),
            "grep".into(),
        ]);
        assert_eq!(validate_terms(&r, "r").unwrap(), vec!["grep", "rg"]);
    }
}

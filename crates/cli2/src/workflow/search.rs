//! Deterministic matching of a request to recorded intents.

use cli2_resolver::terms::{content_stems, equivalents};

/// Minimum similarity for a workflow to be proposed for a request.
pub const MIN_MATCH: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct Similarity {
    pub score: f64,
    /// The request's terms that the recorded intent also covers (directly or by synonym).
    pub shared: Vec<String>,
}

/// Dice coefficient over content stems, counting synonyms as shared.
pub fn similarity(request: &str, recorded: &str) -> Similarity {
    let a = content_stems(request);
    let b = content_stems(recorded);
    if a.is_empty() || b.is_empty() {
        return Similarity {
            score: 0.0,
            shared: vec![],
        };
    }
    let mut shared = Vec::new();
    for s in &a {
        if b.contains(s) || equivalents(s).iter().any(|e| b.contains(e)) {
            shared.push(s.clone());
        }
    }
    let score = 2.0 * shared.len() as f64 / (a.len() + b.len()) as f64;
    Similarity {
        score: (score * 1000.0).round() / 1000.0,
        shared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_similar_and_unrelated() {
        assert_eq!(
            similarity("run the project tests", "run the project tests").score,
            1.0
        );
        assert!(similarity("run the project tests", "execute the tests").score >= MIN_MATCH);
        assert!(similarity("order a pizza", "run the project tests").score < MIN_MATCH);
        assert_eq!(similarity("", "x").score, 0.0);
    }
}

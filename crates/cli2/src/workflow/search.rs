//! Deterministic retrieval of recorded requests that resemble a new one.
//!
//! Retrieval only nominates history that *might* be relevant. It never makes a
//! workflow valid, never lowers an approval requirement and never authorizes
//! anything: validation, fresh planning and authority policy follow, and each
//! can reject what retrieval proposed.
//!
//! `similarity_v1` is the original stem-overlap measure and is kept as the
//! evaluation baseline. `similarity` (v2) compares the *objects* of the two
//! requests (what is being tested, linted, searched for, …) after folding a
//! small developer vocabulary, and refuses to match requests whose intended
//! *actions* differ in a way that matters: a request that deletes, writes,
//! modifies or publishes never matches history that did not, and vice versa.
//! An action mismatch can only lower a score, never raise one.

use std::collections::BTreeSet;

use cli2_resolver::terms::{content_stems, equivalents, is_stop_word, stem, words};

/// Minimum similarity for a workflow to be proposed for a request.
pub const MIN_MATCH: f64 = 0.5;
/// Version of the matcher in use (reported in evaluations).
pub const MATCHER_VERSION: &str = "action_aware_v2";

#[derive(Debug, Clone, PartialEq)]
pub struct Similarity {
    pub score: f64,
    /// Terms (or concepts) the two requests share.
    pub shared: Vec<String>,
    /// Why a textual match was refused (an action mismatch), if it was.
    pub vetoed: Option<String>,
}

// ----------------------------------------------------------------- v2 matcher

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Act {
    // Effect-bearing: changes something.
    Delete,
    Write,
    Modify,
    Publish,
    // Reads or runs.
    Exec,
    Find,
    View,
}

impl Act {
    fn effectful(self) -> bool {
        matches!(self, Act::Delete | Act::Write | Act::Modify | Act::Publish)
    }
    fn label(self) -> &'static str {
        match self {
            Act::Delete => "deletes",
            Act::Write => "creates or writes",
            Act::Modify => "modifies",
            Act::Publish => "publishes or deploys",
            Act::Exec => "runs or checks",
            Act::Find => "searches",
            Act::View => "lists or shows",
        }
    }
}

fn action_of(w: &str) -> Option<Act> {
    Some(match w {
        "delete" | "remove" | "clean" | "wipe" | "erase" | "unpublish" | "uninstall" | "drop"
        | "purge" | "destroy" | "reset" | "rm" | "truncate" => Act::Delete,
        "write" | "create" | "add" | "generate" | "scaffold" | "implement" | "new" | "install" => {
            Act::Write
        }
        "fix" | "rename" | "refactor" | "change" | "edit" | "update" | "modify" | "patch"
        | "rewrite" | "replace" | "migrate" | "skip" | "disable" | "ignore" | "bypass"
        | "upgrade" | "revert" | "move" => Act::Modify,
        "publish" | "release" | "deploy" | "ship" | "push" | "upload" => Act::Publish,
        "run" | "execute" | "launch" | "start" | "kick" | "invoke" | "perform" | "check"
        | "checks" | "verify" | "validate" | "confirm" | "ensure" | "lint" | "compile" => Act::Exec,
        "find" | "search" | "locate" | "grep" | "references" | "reference" | "usages" | "usage"
        | "everywhere" | "where" | "used" | "uses" | "occurrences" => Act::Find,
        "list" | "show" | "display" | "print" | "summarize" | "summary" | "give" | "view"
        | "see" | "which" => Act::View,
        _ => return None,
    })
}

/// Words that carry no object meaning.
fn generic(w: &str) -> bool {
    matches!(
        w,
        "project"
            | "code"
            | "codebase"
            | "repo"
            | "repository"
            | "app"
            | "application"
            | "all"
            | "other"
            | "works"
            | "work"
            | "correctly"
            | "properly"
            | "ok"
            | "okay"
            | "still"
            | "suite"
            | "off"
            | "me"
            | "my"
            | "i"
            | "have"
            | "tree"
            | "current"
            | "please"
            | "again"
            | "now"
            | "everything"
            | "newly"
            | "script"
            | "then"
            | "after"
            | "before"
            | "and"
            | "also"
    ) || is_stop_word(w)
}

/// Fold a word to an object concept, or its stem.
fn concept(w: &str) -> String {
    match w {
        "test" | "tests" | "testing" | "spec" | "specs" | "unittest" | "unittests" => "TEST".into(),
        "lint" | "linter" | "linters" | "linting" | "clippy" | "eslint" => "LINT".into(),
        "format" | "formatting" | "formatter" | "fmt" | "style" | "prettier" | "reformat" => {
            "FORMAT".into()
        }
        "build" | "builds" | "building" | "compile" | "compiles" | "compilation" => "BUILD".into(),
        "diff" | "diffs" | "changes" | "difference" | "differences" | "uncommitted" | "delta" => {
            "DIFF".into()
        }
        "changed" | "modified" | "dirty" | "touched" | "edited" => "CHANGED".into(),
        "summary" | "summarize" | "summarise" | "summarizing" => "SUMMARY".into(),
        "file" | "files" => "FILE".into(),
        "docs" | "documentation" => "DOCS".into(),
        other => stem(other),
    }
}

struct Parsed {
    actions: BTreeSet<Act>,
    objects: BTreeSet<String>,
    normalized: String,
}

fn parse(text: &str) -> Parsed {
    let ws: Vec<String> = words(text).into_iter().map(|w| w.to_lowercase()).collect();
    let has_exec = ws
        .iter()
        .any(|w| action_of(w) == Some(Act::Exec) && w != "compile");
    let mut actions = BTreeSet::new();
    let mut objects = BTreeSet::new();
    for w in &ws {
        // "format the code" applies formatting; "check the formatting" only names it.
        if (w == "format" || w == "reformat") && !has_exec {
            actions.insert(Act::Modify);
            continue;
        }
        if w == "lint" || w == "compile" {
            actions.insert(Act::Exec);
            objects.insert(concept(w));
            continue;
        }
        if let Some(a) = action_of(w) {
            actions.insert(a);
            // "new" and "used" are modifiers more than objects.
            continue;
        }
        if generic(w) {
            continue;
        }
        objects.insert(concept(w));
    }
    Parsed {
        actions,
        objects,
        normalized: ws.join(" "),
    }
}

/// Action-aware similarity of a new request to a recorded one (v2).
pub fn similarity(request: &str, recorded: &str) -> Similarity {
    let (q, h) = (parse(request), parse(recorded));
    if q.normalized == h.normalized && !q.normalized.is_empty() {
        return Similarity {
            score: 1.0,
            shared: q.objects.into_iter().collect(),
            vetoed: None,
        };
    }
    let shared: Vec<String> = q.objects.intersection(&h.objects).cloned().collect();

    // Different intended actions must not match.
    let eff = |p: &Parsed| -> BTreeSet<Act> {
        p.actions
            .iter()
            .copied()
            .filter(|a| a.effectful())
            .collect()
    };
    let (eq, eh) = (eff(&q), eff(&h));
    let mut vetoed = None;
    if eq != eh {
        let differing: Vec<Act> = eq.symmetric_difference(&eh).copied().collect();
        let side = |a: Act| {
            if eq.contains(&a) {
                "the new request"
            } else {
                "the recorded request"
            }
        };
        vetoed = Some(format!(
            "different intended action: {} {}",
            side(differing[0]),
            differing[0].label()
        ));
    } else {
        // Both read-only: only showing vs only running are different asks.
        let read = |p: &Parsed| -> BTreeSet<Act> {
            p.actions
                .iter()
                .copied()
                .filter(|a| !a.effectful())
                .collect()
        };
        let (rq, rh) = (read(&q), read(&h));
        if !rq.is_empty() && !rh.is_empty() && rq.is_disjoint(&rh) {
            vetoed = Some("different intended action: one lists or shows, the other runs".into());
        }
    }
    if let Some(v) = vetoed {
        return Similarity {
            score: 0.0,
            shared,
            vetoed: Some(v),
        };
    }
    if q.objects.is_empty() || h.objects.is_empty() {
        return Similarity {
            score: 0.0,
            shared: vec![],
            vetoed: None,
        };
    }
    let dice = 2.0 * shared.len() as f64 / (q.objects.len() + h.objects.len()) as f64;
    Similarity {
        score: (dice * 1000.0).round() / 1000.0,
        shared,
        vetoed: None,
    }
}

// ----------------------------------------------------------------- v1 baseline

/// Dice coefficient over content stems, counting synonyms as shared. The
/// original matcher, kept unchanged as the evaluation baseline.
pub fn similarity_v1(request: &str, recorded: &str) -> Similarity {
    let a = content_stems(request);
    let b = content_stems(recorded);
    if a.is_empty() || b.is_empty() {
        return Similarity {
            score: 0.0,
            shared: vec![],
            vetoed: None,
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
        vetoed: None,
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
        assert!(similarity("run the project tests", "execute the test suite").score >= MIN_MATCH);
        assert!(similarity("order a pizza", "run the project tests").score < MIN_MATCH);
        assert_eq!(similarity("", "x").score, 0.0);
    }

    #[test]
    fn different_actions_never_match_and_are_explained() {
        for (q, h) in [
            ("delete the project tests", "run the project tests"),
            (
                "run the project tests and then delete the build output",
                "run the project tests",
            ),
            (
                "publish the package after the tests pass",
                "run the project tests",
            ),
            ("unpublish the package", "publish the package"),
            ("format the code", "check the formatting"),
            ("list the project tests", "run the project tests"),
        ] {
            let s = similarity(q, h);
            assert_eq!(s.score, 0.0, "{q} vs {h}");
            assert!(s.vetoed.is_some(), "{q} vs {h}");
        }
        // The same words with the same action still match.
        assert!(similarity("check the formatting", "check the code style").score >= MIN_MATCH);
        assert!(
            similarity("clean the build output", "clean up the build output").score >= MIN_MATCH
        );
    }

    #[test]
    fn a_veto_can_only_lower_a_score() {
        let pairs = [
            ("run the project tests", "run the project tests"),
            ("delete the project tests", "run the project tests"),
            (
                "show me everywhere authenticate is used",
                "find all references to authenticate",
            ),
            ("fix the failing lint checks", "run lint checks"),
        ];
        for (q, h) in pairs {
            let s = similarity(q, h);
            let objects_only = {
                let (a, b) = (parse(q), parse(h));
                let sh = a.objects.intersection(&b.objects).count() as f64;
                if a.objects.is_empty() || b.objects.is_empty() {
                    0.0
                } else {
                    2.0 * sh / (a.objects.len() + b.objects.len()) as f64
                }
            };
            assert!(s.score <= objects_only.max(1.0) + 1e-9);
            if s.vetoed.is_some() {
                assert_eq!(s.score, 0.0);
            }
        }
    }
}

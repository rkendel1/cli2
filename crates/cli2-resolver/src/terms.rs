//! Lexical normalization: tokenization, light stemming, stop words and a small
//! synonym table. Deterministic and documented; no inference involved.

use std::collections::BTreeSet;

const STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "all",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "by",
    "can",
    "could",
    "do",
    "for",
    "from",
    "go",
    "i",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "me",
    "my",
    "now",
    "of",
    "on",
    "our",
    "please",
    "run",
    "execute",
    "some",
    "that",
    "the",
    "their",
    "them",
    "then",
    "this",
    "to",
    "up",
    "us",
    "use",
    "using",
    "via",
    "want",
    "we",
    "with",
    "would",
    "you",
    "your",
    "project",
    "projects",
    "repo",
    "repository",
    "invoke",
    "perform",
    "call",
];

/// Groups of interchangeable terms (compared after stemming).
const SYNONYMS: &[&[&str]] = &[
    &["test", "spec", "unittest"],
    &["build", "compile", "bundle"],
    &["lint", "linter", "style"],
    &["format", "fmt", "prettify", "prettier"],
    &["publish", "release", "ship"],
    &["deploy", "rollout"],
    &["clean", "cleanup", "tidy"],
    &["start", "serve", "launch"],
    &["generate", "create", "write", "produce", "gen"],
    &["search", "grep", "find"],
    &["list", "ls", "show"],
    &["report", "summary", "summarize"],
    &["check", "typecheck", "verify"],
];

pub fn stem(word: &str) -> String {
    let w = word.to_lowercase();
    let n = w.len();
    if n > 5 && w.ends_with("ing") {
        return w[..n - 3].to_string();
    }
    if n > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..n - 3]);
    }
    if n > 4
        && (w.ends_with("sses") || w.ends_with("xes") || w.ends_with("ches") || w.ends_with("shes"))
    {
        return w[..n - 2].to_string();
    }
    if n > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") {
        return w[..n - 1].to_string();
    }
    w
}

pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn is_stop_word(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

/// Content stems of a text (stop words removed).
pub fn content_stems(text: &str) -> BTreeSet<String> {
    words(text)
        .into_iter()
        .filter(|w| !is_stop_word(w))
        .map(|w| stem(&w))
        .collect()
}

/// All stems considered equivalent to `s` (including itself).
pub fn equivalents(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::from([s.to_string()]);
    for group in SYNONYMS {
        let stems: Vec<String> = group.iter().map(|g| stem(g)).collect();
        if stems.iter().any(|g| g == s) {
            out.extend(stems);
        }
    }
    out
}

/// Values the caller embedded in the intent: quoted strings and path-like
/// tokens. Returns `(values, intent text with quoted segments removed)`.
pub fn extract_values(intent: &str) -> (Vec<String>, String) {
    let mut values = Vec::new();
    let mut rest = String::new();
    let mut chars = intent.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' || c == '`' {
            let mut v = String::new();
            let mut closed = false;
            for d in chars.by_ref() {
                if d == c {
                    closed = true;
                    break;
                }
                v.push(d);
            }
            if closed {
                values.push(v);
                rest.push(' ');
                continue;
            }
            rest.push(c);
            rest.push_str(&v);
        } else {
            rest.push(c);
        }
    }
    // Path-like values are inputs, not descriptions of the task: they do not
    // take part in ranking.
    let rest = rest
        .split_whitespace()
        .filter(|w| !is_path_like(w.trim_end_matches([',', ';', ':', ')', '!', '?', '.'])))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = Vec::new();
    // Keep quoted values and path-like tokens in their original order.
    let mut quoted = values.into_iter();
    let mut ordered = Vec::new();
    for piece in intent.split_whitespace() {
        let first = piece.chars().next().unwrap_or(' ');
        if matches!(first, '"' | '\'' | '`') {
            continue;
        }
        let tok = piece.trim_end_matches([',', ';', ':', ')', '!', '?']);
        let tok = tok
            .strip_suffix('.')
            .filter(|t| t.contains('.') || t.contains('/'))
            .unwrap_or(tok);
        if is_path_like(tok) {
            ordered.push((intent.find(piece).unwrap_or(0), tok.to_string()));
        }
    }
    let mut pos = 0;
    for q in quoted.by_ref() {
        let idx = intent[pos..]
            .find(q.as_str())
            .map(|i| i + pos)
            .unwrap_or(pos);
        pos = idx + q.len();
        ordered.push((idx, q));
    }
    ordered.sort_by_key(|(i, _)| *i);
    out.extend(ordered.into_iter().map(|(_, v)| v));
    (out, rest)
}

fn is_path_like(tok: &str) -> bool {
    if tok.len() < 3 || tok.starts_with('-') {
        return false;
    }
    if !tok
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/~+@".contains(c))
    {
        return false;
    }
    if tok.contains('/') {
        return tok.chars().any(|c| c.is_ascii_alphanumeric());
    }
    match tok.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=6).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems() {
        assert_eq!(stem("tests"), "test");
        assert_eq!(stem("testing"), "test");
        assert_eq!(stem("builds"), "build");
        assert_eq!(stem("status"), "status");
    }

    #[test]
    fn values() {
        let (v, rest) = extract_values("write a report to out/report.txt titled \"Q3 numbers\"");
        assert_eq!(v, vec!["out/report.txt", "Q3 numbers"]);
        assert!(!rest.contains("Q3"));
        assert!(!rest.contains("report.txt"));
        let (v, _) = extract_values("run the project tests.");
        assert!(v.is_empty());
        let (v, _) = extract_values("summarize notes.md, please");
        assert_eq!(v, vec!["notes.md"]);
    }
}

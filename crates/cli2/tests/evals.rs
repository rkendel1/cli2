//! Runs `evals/first-slice.jsonl`: intent → expected resolution, in plan-only
//! mode against isolated fixture projects.

mod support;

use serde_json::Value;
use support::*;

#[test]
fn first_slice_evals() {
    let text = std::fs::read_to_string(repo_root().join("evals/first-slice.jsonl")).unwrap();
    let mut failures = Vec::new();
    let mut ran = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let case: Value = serde_json::from_str(line).unwrap();
        let id = case["id"].as_str().unwrap();
        let tools: Vec<&str> = case["requires"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        let missing: Vec<&&str> = tools
            .iter()
            .filter(|t| {
                which(t).is_none() && !(**t == "cargo" && std::env::var_os("CARGO").is_some())
            })
            .collect();
        if !missing.is_empty() {
            eprintln!("SKIPPED eval {id}: missing {missing:?}");
            continue;
        }
        let env = Env::new();
        let proj = env.project(case["fixture"].as_str().unwrap());
        let mut args = vec!["plan", case["intent"].as_str().unwrap()];
        if let Some(extra) = case["args"].as_array() {
            args.extend(extra.iter().filter_map(|a| a.as_str()));
        }
        let (_, out) = env.json(&proj, &args);
        ran += 1;
        let expect = &case["expect"];
        let mut problems = Vec::new();
        if out["status"] != expect["status"] {
            problems.push(format!("status {} != {}", out["status"], expect["status"]));
        }
        if let Some(cap) = expect["capability_id"].as_str() {
            if out["plan"]["steps"][0]["capability_id"].as_str() != Some(cap) {
                problems.push(format!(
                    "capability {} != {cap}",
                    out["plan"]["steps"][0]["capability_id"]
                ));
            }
        }
        if let Some(level) = expect["approval"].as_str() {
            if out["plan"]["approval"]["level"].as_str() != Some(level) {
                problems.push(format!(
                    "approval {} != {level}",
                    out["plan"]["approval"]["level"]
                ));
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{id}: {}\n{}", problems.join("; "), out["message"]));
        }
    }
    eprintln!("ran {ran} evals");
    assert!(
        failures.is_empty(),
        "eval failures:\n{}",
        failures.join("\n")
    );
}

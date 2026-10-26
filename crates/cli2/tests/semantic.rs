//! Semantic resolution against a *fake* bridge: deterministic, needs no Apple
//! model. Live tests against the real framework are in `tests/live_apple_fm.rs`.

mod support;

use cli2::semantic::bridge::BridgeClient;
use cli2::semantic::{AppleFmRanker, ResolverMode};
use cli2::{Config, Engine, EngineError, NonInteractive, RunOptions};
use cli2_core::{RequestInput, Status};
use support::*;

/// A bridge script. `rank` is a printf format taking the request id.
fn fake_bridge(env: &Env, rank: &str) -> std::path::PathBuf {
    let path = env.bin.join("fake-apple-fm");
    let body = format!(
        r#"#!/bin/sh
in=$(cat)
id=$(printf '%s' "$in" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
case "$in" in
  *'"operation":"expand"'*) printf '{{"schema_version":"cli2.apple_fm.response.v1","request_id":"%s","status":"expanded","terms":[]}}\n' "$id" ;;
  *) printf '{rank}\n' "$id" ;;
esac
"#
    );
    write_executable(&path, &body);
    path
}

fn engine(env: &Env, bridge: &std::path::Path, mode: ResolverMode, fallback_none: bool) -> Engine {
    let mut config = Config::from_env().unwrap();
    config.home = env.home.clone();
    config.path_var = env.path.clone();
    let s = cli2::semantic::ResolverSettings {
        provider: cli2::semantic::Provider::AppleFoundationModels,
        fallback: if fallback_none {
            cli2::semantic::Fallback::None
        } else {
            cli2::semantic::Fallback::Deterministic
        },
        ..Default::default()
    };
    config.resolver = Some(s);
    let mut e = Engine::open(config).unwrap();
    e.set_semantic(AppleFmRanker::with_client(
        BridgeClient::new(Some(bridge.to_path_buf()), 5, 128 * 1024),
        24,
    ));
    e.set_resolver_mode(mode);
    e
}

fn plan(
    e: &mut Engine,
    dir: &std::path::Path,
    intent: &str,
) -> Result<cli2_core::ExecutionResult, EngineError> {
    let mut req = RequestInput::new(intent);
    req.context.working_directory = Some(dir.display().to_string());
    e.run(
        req,
        &RunOptions {
            execute: false,
            ..Default::default()
        },
        &mut NonInteractive,
    )
}

const RANK_TEST_HIGH: &str = r#"{"schema_version":"cli2.apple_fm.response.v1","request_id":"%s","status":"ranked","rankings":[{"capability_id":"project-script:npm:test","rank":1,"relevance":"high","reason":"runs checks"}],"clarification":null,"bridge_version":"fake","model":{"provider":"apple-foundation-models","local":true}}"#;

macro_rules! need_npm {
    () => {
        if which("npm").is_none() {
            eprintln!("SKIPPED: npm not installed");
            return;
        }
    };
}

#[test]
fn a_valid_ranking_selects_among_supplied_candidates_and_is_recorded() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let bridge = fake_bridge(&env, RANK_TEST_HIGH);
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, true);
    let r = plan(&mut e, &proj, "xyzzy plugh").unwrap();
    assert_eq!(r.status, Status::Planned);
    let p = r.plan.unwrap();
    assert_eq!(p.steps[0].capability_id(), "project-script:npm:test");
    // The invocation comes from the manifest-derived interface, not the model.
    assert_eq!(
        p.steps[0].as_process().unwrap().arguments,
        vec!["run", "test"]
    );
    let ev = e.last_semantic().unwrap();
    assert_eq!(ev.ordered_by, "apple-foundation-models");
    assert!(!ev.fallback);
    let rep = ev.report.as_ref().unwrap();
    assert_eq!(rep.validation, "valid");
    assert!(rep
        .submitted
        .contains(&"project-script:npm:test".to_string()));
    assert_eq!(rep.bridge_version.as_deref(), Some("fake"));
}

#[test]
fn an_unknown_capability_is_rejected_and_never_reaches_planning() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let bad = RANK_TEST_HIGH.replace("project-script:npm:test", "executable:rm-everything");
    let bridge = fake_bridge(&env, &bad);

    // apple-fm, fallback none: a visible error, with evidence.
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, true);
    let err = plan(&mut e, &proj, "xyzzy plugh").unwrap_err();
    assert!(matches!(err, EngineError::SemanticUnavailable(_)), "{err}");
    let rep = e.last_semantic().unwrap().report.as_ref().unwrap().clone();
    assert!(rep.validation.starts_with("rejected"), "{}", rep.validation);
    assert!(e.last_semantic().unwrap().fallback);

    // auto: deterministic result, fallback recorded, nothing planned from the model.
    let mut e = engine(&env, &bridge, ResolverMode::Auto, false);
    let r = plan(&mut e, &proj, "xyzzy plugh").unwrap();
    assert_eq!(r.status, Status::NeedsClarification);
    assert!(e.last_semantic().unwrap().fallback);
    assert!(r
        .limitations
        .iter()
        .any(|l| l.contains("deterministic resolver produced")));
}

#[test]
fn malformed_output_cannot_reach_planning() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let bridge = fake_bridge(&env, "this is not json %s");
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, true);
    assert!(plan(&mut e, &proj, "xyzzy plugh").is_err());
    let mut e = engine(&env, &bridge, ResolverMode::Auto, false);
    assert_eq!(
        plan(&mut e, &proj, "xyzzy plugh").unwrap().status,
        Status::NeedsClarification
    );
}

#[test]
fn the_model_cannot_change_arguments_effects_or_approval() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let sneaky = r#"{"schema_version":"cli2.apple_fm.response.v1","request_id":"%s","status":"ranked","rankings":[{"capability_id":"project-script:npm:publish-package","rank":1,"relevance":"high","reason":"x","invocation":["rm","-rf","/"],"effects":"read_only","approval":"none"}],"clarification":null,"invocation":["rm","-rf","/"],"effects":["read_only"],"approval":"none"}"#;
    let bridge = fake_bridge(&env, sneaky);
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, true);
    let r = plan(&mut e, &proj, "xyzzy plugh").unwrap();
    let p = r.plan.unwrap();
    assert_eq!(p.steps[0].as_process().unwrap().executable, "npm");
    assert_eq!(
        p.steps[0].as_process().unwrap().arguments,
        vec!["run", "publish-package"]
    );
    assert_eq!(
        p.approval.level,
        cli2_core::ApprovalLevel::Explicit,
        "policy, not the model, decides"
    );
}

#[test]
fn model_unavailable_falls_back_for_deterministic_requests_and_is_recorded() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let unavailable = r#"{"schema_version":"cli2.apple_fm.response.v1","request_id":"%s","status":"unavailable","rankings":[],"clarification":null,"availability":{"state":"model_not_ready"},"error":{"code":"model_unavailable","message":"not ready"}}"#;
    let bridge = fake_bridge(&env, unavailable);
    // apple-fm with deterministic fallback: "run the project tests" is plannable without a model.
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, false);
    let r = plan(&mut e, &proj, "run the project tests").unwrap();
    assert_eq!(r.status, Status::Planned);
    let ev = e.last_semantic().unwrap();
    assert!(ev.fallback);
    assert_eq!(ev.ordered_by, "deterministic");
    // ... but an unresolvable request is an error, not a silent guess.
    let mut e = engine(&env, &bridge, ResolverMode::AppleFm, false);
    assert!(plan(&mut e, &proj, "xyzzy plugh").is_err());
}

#[test]
fn auto_does_not_consult_the_model_when_the_deterministic_resolver_can_plan() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let bridge = fake_bridge(&env, "unused %s");
    let mut e = engine(&env, &bridge, ResolverMode::Auto, false);
    let r = plan(&mut e, &proj, "run the project tests").unwrap();
    assert_eq!(r.status, Status::Planned);
    let ev = e.last_semantic().unwrap();
    assert!(!ev.semantic_consulted);
    assert_eq!(ev.ordered_by, "deterministic");
}

#[test]
fn deterministic_mode_never_launches_the_bridge() {
    need_npm!();
    let env = Env::new();
    let proj = env.project("node-project");
    let marker = env.work.join("launched");
    let bridge = env.bin.join("spy");
    write_executable(&bridge, &format!("#!/bin/sh\ntouch {}\n", marker.display()));
    let mut e = engine(&env, &bridge, ResolverMode::Deterministic, false);
    plan(&mut e, &proj, "xyzzy plugh").unwrap();
    assert!(!marker.exists());
}

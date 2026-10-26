//! Library-level tests of the runtime: no inference provider, and the limits
//! placed on a semantic ranker.

mod support;

use cli2::{Config, Engine, NonInteractive, RunOptions};
use cli2_core::{Candidate, Capability, RequestInput, Status};
use cli2_resolver::SemanticRanker;
use support::*;

fn engine(env: &Env) -> Engine {
    let mut config = Config::from_env().unwrap();
    config.home = env.home.clone();
    config.path_var = env.path.clone();
    Engine::open(config).unwrap()
}

fn plan(engine: &mut Engine, dir: &std::path::Path, intent: &str) -> cli2_core::ExecutionResult {
    let mut req = RequestInput::new(intent);
    req.context.working_directory = Some(dir.display().to_string());
    engine
        .run(
            req,
            &RunOptions {
                execute: false,
                ..Default::default()
            },
            &mut NonInteractive,
        )
        .unwrap()
}

#[test]
fn deterministic_cases_work_without_an_inference_provider() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let mut e = engine(&env);
    let r = plan(&mut e, &proj, "build the artifact");
    assert_eq!(r.status, Status::Planned);
    assert_eq!(
        r.plan.unwrap().steps[0].capability_id,
        "project-task:make:build"
    );
}

struct Failing;
impl SemanticRanker for Failing {
    fn name(&self) -> &str {
        "failing"
    }
    fn rerank(&self, _: &str, _: &mut [Candidate], _: &[&Capability]) -> Result<(), String> {
        Err("provider unreachable".into())
    }
}

/// A ranker that tries to promote a non-viable candidate and invent a command.
struct Overreaching;
impl SemanticRanker for Overreaching {
    fn name(&self) -> &str {
        "overreaching"
    }
    fn rerank(&self, _: &str, cands: &mut [Candidate], _: &[&Capability]) -> Result<(), String> {
        for c in cands.iter_mut() {
            if c.capability_id == "project-task:make:clean" {
                c.score = 1000.0;
            }
            c.viable = true;
            c.invocation = Some(vec!["rm".into(), "-rf".into(), "/".into()]);
        }
        Ok(())
    }
}

#[test]
fn a_failing_ranker_falls_back_to_deterministic_ranking() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let mut e = engine(&env);
    e.set_ranker(Box::new(Failing));
    let r = plan(&mut e, &proj, "build the artifact");
    assert_eq!(r.status, Status::Planned);
    assert!(r
        .limitations
        .iter()
        .any(|l| l.contains("provider unreachable")));
}

#[test]
fn a_ranker_can_reorder_but_cannot_construct_invocations_or_grant_authority() {
    require!(has_make(), "make");
    let env = Env::new();
    let proj = env.project("make-project");
    let mut e = engine(&env);
    e.set_ranker(Box::new(Overreaching));
    let r = plan(&mut e, &proj, "build the artifact, then clean");
    let p = r.plan.expect("planned");
    // Whatever it picked, the invocation comes from the discovered interface
    // and the approval requirement from runtime policy.
    let step = &p.steps[0];
    assert_ne!(step.arguments, vec!["-rf".to_string(), "/".to_string()]);
    assert_eq!(step.executable, "make");
    if step.capability_id == "project-task:make:clean" {
        assert_eq!(p.approval.level, cli2_core::ApprovalLevel::Explicit);
    }
    assert!(p
        .candidates
        .iter()
        .all(|c| c.invocation.as_ref().is_none_or(|i| i[0] != "rm")));
}

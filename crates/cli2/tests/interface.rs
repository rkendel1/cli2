//! Interface discovery lifecycle and the search workflow, end to end.
//! Fixture tools are real executables written by the tests; the real `rg`
//! scenario skips explicitly when `rg` is not installed.

mod support;

use std::time::Duration;

use cli2::{CaptureNote, Config, Engine, NonInteractive, RunOptions};
use cli2_core::{
    ApprovalLevel, Capability, CaptureStatus, EffectCertainty, InterfaceCompleteness, RequestInput,
    Status,
};
use support::*;

const SEARCH_HELP: &str = r#"fsearch 1.0

fsearch recursively searches the current directory for lines matching a pattern.

USAGE:
    fsearch [OPTIONS] PATTERN [PATH ...]

OPTIONS:
    -i, --ignore-case
        Search case insensitively.
    -C, --context <NUM>
        Lines of context.
"#;

const NAMES_HELP: &str = r#"fname - find files by name

Usage: fname [OPTIONS] <PATTERN> [PATH]...

Finds files whose names match PATTERN under the current directory.

Options:
  -x, --exact   Exact names only
"#;

/// A tool that counts how often it is run with `--help`, prints `help` for
/// `--help`, and otherwise searches (via the real grep/find) like a tiny rg/fd.
fn install_tool(env: &Env, name: &str, help: &str, body: &str) -> std::path::PathBuf {
    let counter = env.work.join(format!("{name}.helpcount"));
    let path = env.bin.join(name);
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then echo x >> '{}'; cat <<'EOF'\n{help}EOF\nexit 0; fi\n{body}\n",
        counter.display()
    );
    write_executable(&path, &script);
    path
}

fn help_runs(env: &Env, name: &str) -> usize {
    std::fs::read_to_string(env.work.join(format!("{name}.helpcount")))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn engine(env: &Env) -> Engine {
    let mut config = Config::from_env().unwrap();
    config.home = env.home.clone();
    config.path_var = env.path.clone();
    config.help_timeout = Duration::from_millis(800);
    config.resolver = Some(Default::default());
    Engine::open(config).unwrap()
}

fn run(
    e: &mut Engine,
    dir: &std::path::Path,
    intent: &str,
    execute: bool,
    yes: bool,
    capture: bool,
) -> cli2_core::ExecutionResult {
    let mut req = RequestInput::new(intent);
    req.context.working_directory = Some(dir.display().to_string());
    e.run(
        req,
        &RunOptions {
            execute,
            yes,
            capture_help: capture,
            ..Default::default()
        },
        &mut NonInteractive,
    )
    .unwrap()
}

fn cap(e: &mut Engine, dir: &std::path::Path, id: &str) -> Capability {
    e.get_capability(id, dir).unwrap()
}

#[test]
fn capture_records_options_usage_and_provenance_without_touching_effects() {
    let env = Env::new();
    install_tool(&env, "fsearch", SEARCH_HELP, "exit 0");
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    let (c, note) = e
        .capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    assert_eq!(note, CaptureNote::Captured);
    assert_eq!(c.interface.completeness, InterfaceCompleteness::Inferred);
    let names: Vec<_> = c
        .interface
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p.required))
        .collect();
    assert_eq!(names, vec![("pattern", true), ("path", false)]);
    assert!(c
        .interface
        .options
        .iter()
        .any(|o| o.names == ["-i", "--ignore-case"]));
    let cc = c.interface.capture.as_ref().unwrap();
    assert_eq!(cc.status, CaptureStatus::Parsed);
    assert_eq!(cc.executable_fingerprint, c.fingerprint);
    assert_eq!(cc.method, vec!["--help"]);
    assert!(cc.output_ref.is_some());
    // A valid interface does not make a command safe: effects stay unknown.
    assert_eq!(c.effects.certainty, EffectCertainty::Unknown);
    assert!(c.effects.classes.is_empty());
}

#[test]
fn a_valid_capture_is_reused_and_runs_nothing() {
    let env = Env::new();
    install_tool(&env, "fsearch", SEARCH_HELP, "exit 0");
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    assert_eq!(help_runs(&env, "fsearch"), 1);
    let (_, note) = e
        .capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    assert_eq!(note, CaptureNote::Cached);
    assert_eq!(help_runs(&env, "fsearch"), 1, "no second --help run");
    // An explicit refresh does run it again.
    let (_, note) = e
        .capture_interface("executable:fsearch", &proj, true)
        .unwrap();
    assert_eq!(note, CaptureNote::Captured);
    assert_eq!(help_runs(&env, "fsearch"), 2);
}

#[test]
fn an_older_parser_version_is_reparsed_from_stored_output_not_rerun() {
    let env = Env::new();
    install_tool(&env, "fsearch", SEARCH_HELP, "exit 0");
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    // Simulate an index written by an older parser.
    let mut c = cap(&mut e, &proj, "executable:fsearch");
    c.interface.capture.as_mut().unwrap().parser_version = 1;
    c.interface.parameters.clear();
    let mut idx = cli2_index::Index::open(&env.home.join("index.db")).unwrap();
    idx.put(&c).unwrap();
    drop(idx);
    let mut e = engine(&env);
    let (c, note) = e
        .capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    assert_eq!(note, CaptureNote::Reparsed);
    assert_eq!(c.interface.parameters.len(), 2);
    assert_eq!(
        c.interface.capture.unwrap().parser_version,
        cli2_discovery::PARSER_VERSION
    );
    assert_eq!(help_runs(&env, "fsearch"), 1);
}

#[test]
fn a_changed_executable_loses_its_interface_and_is_not_used_meanwhile() {
    let env = Env::new();
    install_tool(&env, "fsearch", SEARCH_HELP, "echo hi");
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:fsearch", &proj, false)
        .unwrap();

    // The executable is upgraded in place; the index has not been rescanned.
    install_tool(
        &env,
        "fsearch",
        "fsearch 2.0\n\nUsage: fsearch --new-style\n",
        "echo changed",
    );
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication",
        false,
        false,
        false,
    );
    assert_ne!(
        r.status,
        Status::Planned,
        "stale interface must not produce a plan: {r:?}"
    );
    assert!(
        r.limitations.iter().any(|l| l.contains("changed on disk")),
        "{:?}",
        r.limitations
    );
    assert!(
        e.capture_interface("executable:fsearch", &proj, false)
            .is_err(),
        "capturing a changed executable requires a rescan first"
    );

    // After a rescan the old interface is gone, then a new capture reflects the new help.
    let report = e.scan(&proj).unwrap();
    assert!(report
        .environment
        .changed
        .iter()
        .any(|c| c.id == "executable:fsearch"));
    let c = cap(&mut e, &proj, "executable:fsearch");
    assert_eq!(c.interface.completeness, InterfaceCompleteness::Unknown);
    assert!(c.interface.capture.is_none() && c.interface.parameters.is_empty());
    let (c, _) = e
        .capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    assert!(c.interface.parameters.is_empty());
    assert_ne!(c.interface.completeness, InterfaceCompleteness::Inferred);
    assert_eq!(help_runs(&env, "fsearch"), 2);
}

#[test]
fn timeouts_and_garbage_never_produce_a_trusted_interface_and_are_not_retried() {
    let env = Env::new();
    write_executable(
        &env.bin.join("slowhelp"),
        "#!/bin/sh\necho started >> \"$0.runs\"\nsleep 30\n",
    );
    write_executable(&env.bin.join("nohelp"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &env.bin.join("junkhelp"),
        "#!/bin/sh\necho 'Segmentation fault'\nexit 139\n",
    );
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();

    let started = std::time::Instant::now();
    assert!(e
        .capture_interface("executable:slowhelp", &proj, false)
        .is_err());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "bounded by the help timeout"
    );
    let c = cap(&mut e, &proj, "executable:slowhelp");
    assert_eq!(c.interface.completeness, InterfaceCompleteness::Unknown);
    assert_eq!(
        c.interface.capture.as_ref().unwrap().status,
        CaptureStatus::TimedOut
    );
    assert!(c.interface.parameters.is_empty() && c.help.is_none());

    assert!(e
        .capture_interface("executable:nohelp", &proj, false)
        .is_err());
    assert_eq!(
        cap(&mut e, &proj, "executable:nohelp")
            .interface
            .capture
            .unwrap()
            .status,
        CaptureStatus::NoUsableHelp
    );
    assert!(e
        .capture_interface("executable:junkhelp", &proj, false)
        .is_err());
    let j = cap(&mut e, &proj, "executable:junkhelp");
    assert_eq!(j.interface.completeness, InterfaceCompleteness::Unknown);
    assert!(j.interface.parameters.is_empty());

    // A recorded failure is not silently retried ...
    let runs = |n: &str| {
        std::fs::read_to_string(env.bin.join(format!("{n}.runs")))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    let before = runs("slowhelp");
    let err = e
        .capture_interface("executable:slowhelp", &proj, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not retrying"), "{err}");
    assert_eq!(runs("slowhelp"), before);
    // ... and nothing is planned from it.
    let r = run(&mut e, &proj, "slowhelp", false, false, false);
    assert_ne!(r.status, Status::Planned);
}

#[test]
fn unknown_and_partial_interfaces_are_never_guessed() {
    let env = Env::new();
    // Understood only in part: a bare lowercase word could be a subcommand.
    install_tool(&env, "partialtool", "partialtool - searches things for patterns\n\nusage: partialtool subcommand FILE\n  -v, --verbose  Be loud\n", "exit 0");
    install_tool(&env, "blindtool", "", "exit 0");
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    let (c, _) = e
        .capture_interface("executable:partialtool", &proj, false)
        .unwrap();
    assert_eq!(c.interface.completeness, InterfaceCompleteness::Partial);
    let r = run(&mut e, &proj, "partialtool", false, false, false);
    assert_eq!(r.status, Status::NeedsClarification);
    assert!(
        r.message.contains("only part of the interface"),
        "{}",
        r.message
    );
    assert!(r.plan.is_none());
    // Never captured at all: also not plannable.
    let r = run(&mut e, &proj, "blindtool", false, false, false);
    assert_eq!(r.status, Status::NeedsClarification);
    assert!(r.message.contains("not characterized"), "{}", r.message);
}

#[test]
fn source_text_search_is_planned_from_the_verified_interface_and_gated_by_policy() {
    let env = Env::new();
    install_tool(
        &env,
        "fsearch",
        SEARCH_HELP,
        "exec /usr/bin/grep -rn -- \"$1\" .",
    );
    let proj = env.project("node-project");
    std::fs::write(proj.join("src/login.js"), "// authentication entry point\n").unwrap();
    let mut e = engine(&env);
    e.scan(&proj).unwrap();

    // Not yet plannable: the interface is unknown and nothing was captured.
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication",
        false,
        false,
        false,
    );
    assert_ne!(r.status, Status::Planned);
    assert_eq!(
        help_runs(&env, "fsearch"),
        0,
        "capture needs explicit consent"
    );

    // With consent, the interface is learned and the plan uses the actual search term.
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication in fsearch",
        false,
        false,
        true,
    );
    // (the tool is named in the request so lexical retrieval reaches it)
    let plan = r.plan.expect("plannable once its interface is known");
    let step = &plan.steps[0];
    assert_eq!(step.capability_id(), "executable:fsearch");
    assert_eq!(
        step.as_process().unwrap().arguments,
        vec!["authentication"],
        "pattern bound; omitted PATH is documented as the current directory"
    );
    assert!(plan
        .verification
        .checks
        .iter()
        .any(|c| c.task_level && c.source == "interface:search_term:pattern"));
    assert_eq!(plan.effects.certainty, EffectCertainty::Unknown);
    assert_eq!(
        plan.approval.level,
        ApprovalLevel::Confirm,
        "unknown effects still need approval"
    );

    // Without approval nothing runs.
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication in fsearch",
        true,
        false,
        false,
    );
    assert_eq!(r.status, Status::AwaitingApproval);
    assert!(r.steps.is_empty());

    // With approval it runs, and success needs the output to mention the term.
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication in fsearch",
        true,
        true,
        false,
    );
    assert_eq!(r.status, Status::Succeeded, "{r:?}");
    let stdout = e
        .read_evidence(r.steps[0].stdout_ref.as_ref().unwrap())
        .unwrap();
    assert!(String::from_utf8_lossy(&stdout).contains("authentication entry point"));

    // Evidence records the interface provenance.
    let t = e.task(&r.request_id).unwrap();
    let ev = serde_json::to_string(&t).unwrap();
    assert!(ev.contains("interface_provenance"), "{ev}");
}

#[test]
fn a_search_with_no_match_is_not_reported_as_success() {
    let env = Env::new();
    install_tool(
        &env,
        "fsearch",
        SEARCH_HELP,
        "exec /usr/bin/grep -rn -- \"$1\" . ; ",
    );
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:fsearch", &proj, false)
        .unwrap();
    let r = run(
        &mut e,
        &proj,
        "search for zzzqqq with fsearch",
        true,
        true,
        false,
    );
    assert_ne!(r.status, Status::Succeeded, "{r:?}");
}

#[test]
fn filename_search_is_planned_from_a_second_verified_interface() {
    let env = Env::new();
    install_tool(
        &env,
        "fname",
        NAMES_HELP,
        "exec /usr/bin/find . -name \"*$1*\"",
    );
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:fname", &proj, false)
        .unwrap();
    // Without a search subject the pattern is not invented.
    let r = run(&mut e, &proj, "fname", false, false, false);
    assert_ne!(r.status, Status::Planned);
    assert!(r.message.contains("pattern") || r.clarification.is_some());
    let r = run(&mut e, &proj, "find files named package", true, true, false);
    let plan = r.plan.as_ref().expect("planned");
    assert_eq!(plan.steps[0].capability_id(), "executable:fname");
    assert_eq!(
        plan.steps[0].as_process().unwrap().arguments,
        vec!["package"]
    );
    assert_eq!(r.status, Status::Succeeded, "{r:?}");
}

#[test]
fn an_unsupplied_path_is_not_omitted_unless_help_says_what_omission_means() {
    let env = Env::new();
    // Optional path, but the help never says what happens without it.
    install_tool(
        &env,
        "quietsearch",
        "quietsearch - search text in files\n\nusage: quietsearch PATTERN [FILE ...]\n",
        "exit 0",
    );
    let proj = env.project("node-project");
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    e.capture_interface("executable:quietsearch", &proj, false)
        .unwrap();
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication using quietsearch",
        false,
        false,
        false,
    );
    assert_eq!(r.status, Status::NeedsClarification);
    assert!(r.message.contains("file"), "{}", r.message);
    // Supplying the path explicitly makes it plannable.
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication in src/index.js using quietsearch",
        false,
        false,
        false,
    );
    assert_eq!(
        r.plan.unwrap().steps[0].as_process().unwrap().arguments,
        vec!["authentication", "src/index.js"]
    );
}

#[test]
fn real_rg_is_plannable_executable_and_verified_when_installed() {
    if which("rg").is_none() {
        eprintln!("SKIPPED: rg is not installed");
        return;
    }
    let mut env = Env::new();
    std::os::unix::fs::symlink(which("rg").unwrap(), env.bin.join("rg")).unwrap();
    env.path = std::env::join_paths([env.bin.clone()]).unwrap();
    let proj = env.project("node-project");
    std::fs::write(
        proj.join("src/session.js"),
        "const authentication = require('x');\n",
    )
    .unwrap();
    let mut e = engine(&env);
    e.scan(&proj).unwrap();
    let c = e
        .capture_interface("executable:rg", &proj, false)
        .unwrap()
        .0;
    assert_eq!(
        c.interface.completeness,
        InterfaceCompleteness::Inferred,
        "{:?}",
        c.interface.capture
    );
    let r = run(
        &mut e,
        &proj,
        "find all references to authentication",
        true,
        true,
        false,
    );
    let plan = r.plan.as_ref().expect("planned with real rg");
    assert_eq!(plan.steps[0].capability_id(), "executable:rg");
    assert_eq!(
        plan.steps[0].as_process().unwrap().arguments,
        vec!["authentication"]
    );
    assert_eq!(plan.approval.level, ApprovalLevel::Confirm);
    assert_eq!(r.status, Status::Succeeded, "{r:?}");
}

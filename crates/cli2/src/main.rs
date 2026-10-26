use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use cli2::{
    render, ApprovalAnswer, Config, Engine, EngineError, Interaction, NonInteractive, RunOptions,
};
use cli2_core::{
    ApprovalLevel, CapabilityKind, Check, EffectClass, ExecutionResult, NetworkConstraint, Plan,
    RequestInput, Status,
};

mod shell;

const AFTER_HELP: &str = "\
Examples:
  cli2 scan                              discover executables and project tasks
  cli2 \"run the project tests\"           resolve, preview, approve, execute, verify
  cli2 plan \"publish the package\"        resolve and preview only
  cli2 exec PLAN_ID --approve DIGEST     execute a recorded plan with explicit approval
  cli2 task show REQUEST_ID              inspect request, plan, approvals, results, evidence

Exit codes:
  0  succeeded / planned / command completed      5  verification_failed
  1  failed (includes timeouts)                   6  rejected (constraints or policy)
  2  invalid usage, input, or unknown ID          7  cancelled
  3  awaiting_approval                            8  partially_succeeded
  4  needs_clarification                          9  capability index corrupt (run `cli2 scan --rebuild`)
 10  internal error                                 11  semantic resolver unavailable (--resolver apple-fm)

Data directory: $CLI2_HOME, else $XDG_DATA_HOME/cli2, else ~/.local/share/cli2";

#[derive(Parser)]
#[command(
    name = "cli2",
    version,
    about = "CLI 2.0: turn an intent into an authorized, verifiable operation using capabilities discovered in this environment",
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// The intent, in natural language (quote it, or pass several words).
    intent: Vec<String>,
    #[command(flatten)]
    run: RunArgs,
    #[command(flatten)]
    global: GlobalArgs,
}

#[derive(Args, Clone)]
struct GlobalArgs {
    /// Machine-readable JSON output (implies non-interactive).
    #[arg(long, global = true)]
    json: bool,
    /// Working directory to act in.
    #[arg(long, global = true, value_name = "PATH")]
    cwd: Option<PathBuf>,
}

#[derive(Args, Clone, Default)]
struct RunArgs {
    /// Resolve and preview only; do not execute.
    #[arg(long)]
    no_exec: bool,
    /// Approve plans whose policy permits non-interactive confirmation. Never
    /// approves high-impact or forbidden operations.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Explicitly approve the plan with this digest (as shown in the preview).
    #[arg(long, value_name = "DIGEST")]
    approve: Option<String>,
    /// Bound each process to this many seconds.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
    /// Choose a capability among offered alternatives.
    #[arg(long, value_name = "CAPABILITY_ID")]
    pick: Option<String>,
    /// Bind an interface parameter (repeatable).
    #[arg(long = "arg", value_name = "NAME=VALUE")]
    args: Vec<String>,
    /// Also verify that this file exists and was written during execution (repeatable).
    #[arg(long, value_name = "PATH")]
    expect_file: Vec<String>,
    /// Also verify that stdout contains this text (repeatable).
    #[arg(long, value_name = "TEXT")]
    expect_stdout: Vec<String>,
    /// Constraint: the operation must not modify files.
    #[arg(long)]
    no_modify: bool,
    /// Constraint: permit file modification (grants permission for declared file writes).
    #[arg(long, conflicts_with = "no_modify")]
    allow_modify: bool,
    /// Constraint: the operation must not use the network.
    #[arg(long)]
    no_network: bool,
    /// Read a cli2.request.v1 JSON request from a file ('-' for stdin).
    #[arg(long, value_name = "FILE")]
    request: Option<String>,
    /// If the best match is an uncharacterized executable, run its `--help`
    /// (bounded) to learn its interface.
    #[arg(long)]
    capture_help: bool,
    /// Resolver: deterministic | apple-fm | auto (default: from config.json,
    /// else deterministic).
    #[arg(long, value_name = "MODE", value_parser = ["deterministic", "apple-fm", "auto"])]
    resolver: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Discover or reconcile environment and project capabilities.
    Scan {
        /// Move the existing (e.g. corrupt) index aside and rebuild it.
        #[arg(long)]
        rebuild: bool,
    },
    /// Display indexed capabilities.
    Inventory {
        /// Only this kind: executable | project_task.
        #[arg(long)]
        kind: Option<String>,
        /// Only capabilities whose ID or description contains this text.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Resolve an intent and display the plan without executing it.
    Plan {
        intent: Vec<String>,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Execute a recorded plan.
    Exec {
        plan_id: String,
        /// Approve if the plan's policy permits non-interactive confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Explicitly approve the plan with this digest.
        #[arg(long, value_name = "DIGEST")]
        approve: Option<String>,
    },
    /// Show what is known about a capability.
    Describe {
        id: String,
        /// Run `<executable> --help` (bounded) and record what it reveals.
        /// A valid stored capture is reused without running anything.
        #[arg(long)]
        capture_help: bool,
        /// With --capture-help: capture again even if a capture (or a failed
        /// attempt) is already recorded for this executable.
        #[arg(long, requires = "capture_help")]
        refresh_help: bool,
    },
    /// Declare a capability's effects (bound to its current fingerprint).
    Characterize {
        id: String,
        /// Comma-separated: read_only, modifies_files, deletes, network, publishes, deploys.
        #[arg(long, value_delimiter = ',', required = true)]
        effects: Vec<String>,
        #[arg(long)]
        purpose: Option<String>,
    },
    /// Inspect recorded tasks and evidence.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Print a recorded output blob (an `evidence:sha256:...` reference).
    Evidence { reference: String },
    /// Apple Foundation Models: availability, smoke test, evaluation.
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Print the optional shell integration (`c2` helper and completion) to stdout.
    /// Nothing is installed or modified; review the output, then source it.
    ShellInit {
        /// Only zsh is supported.
        shell: ShellName,
    },
    /// Print a standalone shell completion script generated from this CLI's definition.
    Completions {
        /// Only zsh is supported.
        shell: ShellName,
    },
    /// Print the published JSON Schemas.
    Schema {
        /// request | capability | plan | result | approval
        name: Option<String>,
    },
}

#[derive(ValueEnum, Clone, Copy)]
enum ShellName {
    Zsh,
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Report whether Apple Foundation Models is available and ready.
    Status,
    /// Run a harmless, bounded inference smoke test.
    Test,
    /// Run the semantic-resolution evaluation suite on this machine.
    Benchmark {
        /// Evaluation file (default: evals/apple-foundation-models.jsonl in the repository).
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Repository root holding fixtures/ and evals/ (default: found from the binary).
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Inspect a recorded request, plan, execution and evidence.
    Show {
        /// A request ID (req_...) or plan ID (plan_...).
        id: String,
    },
    /// List recent requests.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.global.json;
    match real_main(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            if json {
                let v = serde_json::json!({
                    "schema_version": "cli2.error.v1",
                    "error": {
                        "code": err.code(),
                        "message": err.to_string(),
                        "remedy": err.remedy(),
                    }
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                eprintln!("cli2: error: {err}");
                if let Some(r) = err.remedy() {
                    eprintln!("cli2: {r}");
                }
            }
            ExitCode::from(err.exit_code() as u8)
        }
    }
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!("{}", serde_json::to_string_pretty(v).expect("serializable"));
}

/// The zsh completion script for the real command tree.
fn zsh_completions() -> String {
    let mut buf = Vec::new();
    clap_complete::generate(
        clap_complete::Shell::Zsh,
        &mut Cli::command(),
        "cli2",
        &mut buf,
    );
    String::from_utf8(buf).expect("completion script is UTF-8")
}

fn real_main(cli: Cli) -> Result<i32, EngineError> {
    // Shell integration output is pure text: it must work with no data
    // directory, no index and no configuration.
    match &cli.command {
        Some(Cmd::Completions {
            shell: ShellName::Zsh,
        }) => {
            print!("{}", zsh_completions());
            return Ok(0);
        }
        Some(Cmd::ShellInit {
            shell: ShellName::Zsh,
        }) => {
            print!("{}", shell::zsh_integration(&zsh_completions()));
            return Ok(0);
        }
        _ => {}
    }
    if let Some(dir) = &cli.global.cwd {
        std::env::set_current_dir(dir)
            .map_err(|e| EngineError::Invalid(format!("--cwd {}: {e}", dir.display())))?;
    }
    let json = cli.global.json;
    let interactive = !json && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let cwd = std::env::current_dir()?;
    let config = Config::from_env()?;

    match cli.command {
        // Handled before configuration is loaded (see the top of this function).
        Some(Cmd::ShellInit { .. } | Cmd::Completions { .. }) => Ok(0),
        Some(Cmd::Schema { name }) => {
            let all = cli2_core::schemas();
            match name {
                Some(n) => {
                    let file = format!("{n}.schema.json");
                    let (_, s) = all
                        .into_iter()
                        .find(|(f, _)| *f == file)
                        .ok_or_else(|| EngineError::NotFound(format!("no schema named {n}")))?;
                    print_json(&s);
                }
                None => {
                    let map: BTreeMap<_, _> = all.into_iter().collect();
                    print_json(&map);
                }
            }
            Ok(0)
        }
        Some(Cmd::Scan { rebuild }) => {
            let (mut engine, moved) = if rebuild {
                Engine::open_rebuilding(config)?
            } else {
                (Engine::open(config)?, None)
            };
            let mut report = engine.scan(&cwd)?;
            report.rebuilt_from = moved.map(|p| p.display().to_string());
            if json {
                print_json(&report);
            } else {
                print!("{}", render::scan(&report));
            }
            Ok(0)
        }
        Some(Cmd::Inventory { kind, filter }) => {
            let mut engine = Engine::open(config)?;
            let mut caps = engine.inventory(&cwd)?;
            if let Some(k) = kind {
                let k = match k.as_str() {
                    "executable" => CapabilityKind::Executable,
                    "project_task" | "project-task" | "task" => CapabilityKind::ProjectTask,
                    other => return Err(EngineError::Invalid(format!("unknown kind {other}"))),
                };
                caps.retain(|c| c.kind == k);
            }
            if let Some(f) = filter {
                let f = f.to_lowercase();
                caps.retain(|c| {
                    c.id.to_lowercase().contains(&f)
                        || c.purpose
                            .as_ref()
                            .is_some_and(|p| p.value.to_lowercase().contains(&f))
                });
            }
            if json {
                print_json(&serde_json::json!({
                    "schema_version": "cli2.inventory.v1",
                    "capabilities": caps,
                }));
            } else {
                print!("{}", render::inventory(&caps));
            }
            Ok(0)
        }
        Some(Cmd::Describe {
            id,
            capture_help,
            refresh_help,
        }) => {
            let mut engine = Engine::open(config)?;
            let cap = if capture_help {
                let (cap, note) = engine.capture_interface(&id, &cwd, refresh_help)?;
                if !json {
                    eprintln!("cli2: {}", match note {
                        cli2::CaptureNote::Captured => "ran `--help` and recorded the interface",
                        cli2::CaptureNote::Cached => "a valid capture already exists; nothing was run",
                        cli2::CaptureNote::Reparsed => "re-parsed the stored help output with the current parser; nothing was run",
                    });
                }
                cap
            } else {
                engine.get_capability(&id, &cwd)?
            };
            if json {
                print_json(&cap);
            } else {
                print!("{}", render::capability(&cap));
            }
            Ok(0)
        }
        Some(Cmd::Characterize {
            id,
            effects,
            purpose,
        }) => {
            let mut parsed = Vec::new();
            for e in &effects {
                parsed.push(
                    EffectClass::parse(e)
                        .ok_or_else(|| EngineError::Invalid(format!("unknown effect {e:?}")))?,
                );
            }
            let mut engine = Engine::open(config)?;
            let cap = engine.characterize(&id, &cwd, parsed, purpose)?;
            if json {
                print_json(&cap);
            } else {
                print!("{}", render::capability(&cap));
            }
            Ok(0)
        }
        Some(Cmd::Model { cmd }) => {
            let settings = match &config.resolver {
                Some(s) => s.clone(),
                None => cli2::semantic::ResolverSettings::load(&config.home)
                    .map_err(EngineError::Invalid)?,
            };
            match cmd {
                ModelCmd::Status => {
                    let st = cli2::semantic::status::model_status(&settings);
                    if json {
                        print_json(&st);
                    } else {
                        print!("{}", render::model_status(&st));
                    }
                    Ok(if st.ready { 0 } else { 1 })
                }
                ModelCmd::Test => {
                    let t = cli2::semantic::status::model_test(&settings);
                    if json {
                        print_json(&t);
                    } else {
                        print!("{}", render::model_test(&t));
                    }
                    Ok(if t.passed { 0 } else { 1 })
                }
                ModelCmd::Benchmark { file, repo } => {
                    let root = repo.or_else(find_repo_root).ok_or_else(|| {
                        EngineError::Invalid(
                            "cannot find the repository (fixtures/, evals/); pass --repo".into(),
                        )
                    })?;
                    let file =
                        file.unwrap_or_else(|| root.join("evals/apple-foundation-models.jsonl"));
                    let rep = cli2::semantic::bench::run_benchmark(&root, &file, &settings)?;
                    if json {
                        print_json(&rep);
                    } else {
                        print!("{}", render::benchmark(&rep));
                    }
                    Ok(if rep.model_ready { 0 } else { 1 })
                }
            }
        }
        Some(Cmd::Task { cmd }) => {
            let engine = Engine::open(config)?;
            match cmd {
                TaskCmd::Show { id } => {
                    let t = engine.task(&id)?;
                    if json {
                        print_json(&t);
                    } else {
                        print!("{}", render::task(&t));
                    }
                }
                TaskCmd::List { limit } => {
                    let tasks = engine.tasks(limit)?;
                    if json {
                        print_json(
                            &serde_json::json!({"schema_version": "cli2.task_list.v1", "tasks": tasks}),
                        );
                    } else {
                        for t in tasks {
                            println!(
                                "{}  {}  {:<20} {}",
                                t.request_id,
                                t.received_at,
                                t.status.unwrap_or_else(|| "-".into()),
                                t.intent
                            );
                        }
                    }
                }
            }
            Ok(0)
        }
        Some(Cmd::Evidence { reference }) => {
            let engine = Engine::open(config)?;
            let bytes = engine.read_evidence(&reference)?;
            std::io::stdout().write_all(&bytes)?;
            Ok(0)
        }
        Some(Cmd::Exec {
            plan_id,
            yes,
            approve,
        }) => {
            let mut engine = Engine::open(config)?;
            install_cancel_handler(&engine);
            let opts = RunOptions {
                execute: true,
                yes,
                approve_token: approve,
                timeout_secs: None,
                capture_help: false,
                interactive,
            };
            let result = if json {
                engine.exec_stored(&plan_id, &opts, &mut NonInteractive)?
            } else {
                engine.exec_stored(&plan_id, &opts, &mut Human { interactive })?
            };
            Ok(report(&engine, &result, json))
        }
        Some(Cmd::Plan { intent, run }) => {
            let mut run = run;
            run.no_exec = true;
            do_intent(config, intent, run, json, interactive)
        }
        None => {
            if cli.intent.is_empty() && cli.run.request.is_none() {
                return Err(EngineError::Invalid(
                    "no intent given; try `cli2 --help` or `cli2 \"run the project tests\"`".into(),
                ));
            }
            do_intent(config, cli.intent, cli.run, json, interactive)
        }
    }
}

fn find_repo_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    std::env::current_dir()
        .ok()
        .into_iter()
        .chain(exe.ancestors().skip(1).map(PathBuf::from))
        .flat_map(|d| d.ancestors().map(PathBuf::from).collect::<Vec<_>>())
        .find(|d| {
            d.join("evals/apple-foundation-models.jsonl").is_file() && d.join("fixtures").is_dir()
        })
}

fn parse_mode(run: &RunArgs) -> Option<cli2::semantic::ResolverMode> {
    run.resolver
        .as_deref()
        .and_then(cli2::semantic::ResolverMode::parse)
}

fn build_request(intent: Vec<String>, run: &RunArgs) -> Result<RequestInput, EngineError> {
    let mut req = match &run.request {
        Some(src) => {
            let text = if src == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(src)?
            };
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| EngineError::Invalid(format!("request is not JSON: {e}")))?;
            let req: RequestInput = serde_json::from_value(value).map_err(|e| {
                EngineError::Invalid(format!("request does not match cli2.request.v1: {e}"))
            })?;
            if !intent.is_empty() {
                return Err(EngineError::Invalid(
                    "give the intent either on the command line or in --request, not both".into(),
                ));
            }
            req
        }
        None => RequestInput::new(intent.join(" ")),
    };
    if run.no_modify {
        req.constraints.modify_files = Some(false);
    }
    if run.allow_modify {
        req.constraints.modify_files = Some(true);
    }
    if run.no_network {
        req.constraints.network = Some(NetworkConstraint::Deny);
    }
    if let Some(p) = &run.pick {
        req.selection = Some(p.clone());
    }
    for a in &run.args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| EngineError::Invalid(format!("--arg expects NAME=VALUE, got {a:?}")))?;
        req.bindings.insert(k.to_lowercase(), v.to_string());
    }
    for f in &run.expect_file {
        req.expectations.push(Check::FileExists {
            path: f.clone(),
            modified_after_start: true,
        });
    }
    for t in &run.expect_stdout {
        req.expectations
            .push(Check::StdoutContains { text: t.clone() });
    }
    req.validate()
        .map_err(|e| EngineError::Invalid(e.to_string()))?;
    Ok(req)
}

fn do_intent(
    config: Config,
    intent: Vec<String>,
    run: RunArgs,
    json: bool,
    interactive: bool,
) -> Result<i32, EngineError> {
    let mut req = build_request(intent, &run)?;
    let mut engine = Engine::open(config)?;
    if let Some(mode) = parse_mode(&run) {
        engine.set_resolver_mode(mode);
    }
    install_cancel_handler(&engine);
    let opts = RunOptions {
        execute: !run.no_exec,
        yes: run.yes,
        approve_token: run.approve.clone(),
        timeout_secs: run.timeout,
        capture_help: run.capture_help,
        interactive,
    };
    loop {
        let result = if json {
            engine.run(req.clone(), &opts, &mut NonInteractive)?
        } else {
            engine.run(req.clone(), &opts, &mut Human { interactive })?
        };
        // Interactive disambiguation: let the person pick, then resolve again.
        if interactive && result.status == Status::NeedsClarification {
            let options = result
                .clarification
                .as_ref()
                .map(|c| c.options.clone())
                .unwrap_or_default();
            if options.len() > 1 && req.selection.is_none() {
                print!("{}", render::result(&result));
                if let Some(choice) =
                    prompt(&format!("Choose 1-{} (Enter to cancel): ", options.len()))
                {
                    if let Ok(n) = choice.trim().parse::<usize>() {
                        if (1..=options.len()).contains(&n) {
                            req.selection = Some(options[n - 1].capability_id.clone());
                            continue;
                        }
                    }
                }
                return Ok(result.status.exit_code());
            }
        }
        return Ok(report(&engine, &result, json));
    }
}

fn report(engine: &Engine, result: &ExecutionResult, json: bool) -> i32 {
    if json {
        print_json(result);
    } else {
        for step in &result.steps {
            for (label, r) in [("stdout", &step.stdout_ref), ("stderr", &step.stderr_ref)] {
                if let Some(r) = r {
                    if let Ok(bytes) = engine.read_evidence(r) {
                        print!("{}", render::output_tail(label, &bytes, 40));
                    }
                }
            }
        }
        print!("{}", render::result(result));
    }
    result.status.exit_code()
}

static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

fn install_cancel_handler(engine: &Engine) {
    let token = engine.cancel_token();
    let _ = ctrlc::set_handler(move || {
        if INTERRUPTS.fetch_add(1, Ordering::SeqCst) == 0 {
            eprintln!("\ncli2: cancelling; the running process will be terminated (press Ctrl-C again to exit immediately)");
            token.cancel();
        } else {
            std::process::exit(130);
        }
    });
}

fn prompt(question: &str) -> Option<String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Human output: shows previews on stdout; asks on stderr when interactive.
struct Human {
    interactive: bool,
}

impl Interaction for Human {
    fn preview(&mut self, plan: &Plan) {
        print!("{}", render::preview(plan));
        let _ = std::io::stdout().flush();
    }

    fn ask(&mut self, plan: &Plan) -> ApprovalAnswer {
        if !self.interactive {
            return ApprovalAnswer::Unavailable;
        }
        let answer = match plan.approval.level {
            ApprovalLevel::Explicit => prompt(&format!(
                "This operation needs explicit approval. Type 'yes' to approve plan {} (digest {}): ",
                plan.plan_id,
                plan.short_digest()
            ))
            .map(|a| a.trim() == "yes"),
            ApprovalLevel::Confirm => prompt("Proceed? [y/N] ")
                .map(|a| matches!(a.trim().to_lowercase().as_str(), "y" | "yes")),
            _ => return ApprovalAnswer::Unavailable,
        };
        match answer {
            Some(true) => ApprovalAnswer::Granted,
            Some(false) => ApprovalAnswer::Denied,
            None => ApprovalAnswer::Unavailable,
        }
    }
}

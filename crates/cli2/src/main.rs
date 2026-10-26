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
    /// What to print: `response` (only the captured stdout), `command-and-response`,
    /// or `json` (structured, with the captured streams). Default: the usual
    /// human report. Approval prompts still appear (on stderr); a plan that was
    /// not executed produces no response.
    #[arg(long, value_name = "FORMAT", value_parser = ["response", "command-and-response", "json"])]
    output: Option<String>,
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
        /// What to print: response | command-and-response | json (see `cli2 --help`).
        #[arg(long, value_name = "FORMAT", value_parser = ["response", "command-and-response", "json"])]
        output: Option<String>,
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
    /// List, show and copy captured command results; prune old content.
    Output {
        #[command(subcommand)]
        cmd: OutputCmd,
    },
    /// Serve the local protocol on a Unix socket for embedding applications.
    /// Prints one JSON "ready" line on stdout. Not needed to use the CLI.
    Serve {
        /// Socket path (default: $CLI2_HOME/runtime/cli2.sock).
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
    /// Reject a recorded plan: nothing runs, and the plan can no longer be approved.
    Reject {
        plan_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Evaluate cli2 on realistic development tasks in isolated sandboxes.
    Eval {
        #[command(subcommand)]
        cmd: EvalCmd,
    },
    /// Search what cli2 has done before (evidence-backed, never authority).
    History {
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    /// Inspect a recorded workflow and ask for a fresh plan based on it.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Run a structured batch of requests (see README: `cli2.batch.v1`).
    Batch {
        /// A cli2.batch.v1 JSON file ('-' for stdin).
        #[arg(
            long,
            value_name = "FILE",
            required_unless_present = "show",
            conflicts_with = "show"
        )]
        request_file: Option<String>,
        /// Show a recorded batch (batch_…) instead of running one.
        #[arg(long, value_name = "BATCH_ID")]
        show: Option<String>,
        /// Approve steps whose policy permits non-interactive confirmation (never explicit or forbidden ones).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Bound each process to this many seconds.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
        /// Allow `--help` capture for steps whose best match has an unknown interface.
        #[arg(long)]
        capture_help: bool,
        /// Resolver for every step: deterministic | apple-fm | auto.
        #[arg(long, value_name = "MODE", value_parser = ["deterministic", "apple-fm", "auto"])]
        resolver: Option<String>,
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

#[derive(Subcommand)]
enum EvalCmd {
    /// Run the versioned suites and report measured results (no combined score).
    Run {
        /// Task suite (default: evals/workflows-v1.jsonl in the repository).
        #[arg(long, value_name = "FILE")]
        suite: Option<PathBuf>,
        /// History-matching suite (default: evals/matching-v1.jsonl; `none` to skip).
        #[arg(long, value_name = "FILE")]
        matching: Option<String>,
        /// Repository root holding fixtures/ and evals/ (default: found from the binary).
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,
        /// Keep each case's sandbox, journal, evidence and index here, and write report.json.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Also compare against the real on-device model when it is ready.
        #[arg(long)]
        model: bool,
        /// Run a single case by ID.
        #[arg(long, value_name = "ID")]
        only: Option<String>,
    },
}

#[derive(Subcommand)]
enum HistoryCmd {
    /// Find past workflows resembling a request, with live validity and an explanation.
    Search {
        /// What you want to do (omit to list recent history).
        text: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Only workflows whose task verification passed (the eligible ones).
        #[arg(long)]
        verified_only: bool,
        /// Also list executions that were steps of a recorded batch.
        #[arg(long)]
        include_batch_steps: bool,
        /// `apple-fm`: let the on-device model reorder the matches already found.
        #[arg(long, value_name = "MODE", value_parser = ["deterministic", "apple-fm"])]
        resolver: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Show a workflow: steps, recorded outcome and evidence, and current validity.
    Show { id: String },
    /// Build a FRESH plan for each step from a workflow. Never executes anything.
    Replay {
        id: String,
        /// Accepted for clarity: replay only plans in this release.
        #[arg(long)]
        no_exec: bool,
        /// Supply or override an input for every step (NAME=VALUE, repeatable).
        #[arg(long = "arg", value_name = "NAME=VALUE")]
        args: Vec<String>,
    },
    /// Rebuild the derived workflow index from the journal.
    Reindex,
}

#[derive(Subcommand)]
enum OutputCmd {
    /// List recent captured results (newest first).
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one captured result: command, process status, task status, verification, streams.
    Show {
        /// out_<n>, out_<n>.<step>, exec_<n>, or `latest` (the newest execution that ran a process).
        id: String,
    },
    /// Print one representation of a captured result to stdout (or, with --clipboard, to the clipboard).
    Copy {
        id: String,
        /// response (stdout) | command-and-response | stdout | stderr
        #[arg(long, value_name = "PART", default_value = "response", value_parser = ["response", "command-and-response", "stdout", "stderr"])]
        part: String,
        /// Copy to the system clipboard instead of printing. Opt-in; never automatic.
        #[arg(long)]
        clipboard: bool,
    },
    /// Delete the captured content of old executions (records and IDs stay).
    Prune {
        /// Executions that started more than this many days ago.
        #[arg(long, value_name = "DAYS")]
        older_than_days: u64,
        /// Report what would be removed without removing it.
        #[arg(long)]
        dry_run: bool,
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
        Some(Cmd::Schema { name: Some(n) }) if n.starts_with("protocol.") => {
            let key = &n["protocol.".len()..];
            let (_, schema) = cli2_protocol::schemas()
                .into_iter()
                .find(|(k, _)| *k == key)
                .ok_or_else(|| EngineError::NotFound(format!("no protocol schema named {key}")))?;
            print_json(&schema);
            Ok(0)
        }
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
            output,
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
                ..Default::default()
            };
            let quiet = output.is_some();
            let result = if json {
                engine.exec_stored(&plan_id, &opts, &mut NonInteractive)?
            } else {
                engine.exec_stored(&plan_id, &opts, &mut Human { interactive, quiet })?
            };
            match &output {
                Some(fmt) => Ok(emit_output(&engine, &result, fmt)),
                None => Ok(report(&engine, &result, json)),
            }
        }
        Some(Cmd::History {
            cmd:
                HistoryCmd::Search {
                    text,
                    limit,
                    verified_only,
                    include_batch_steps,
                    resolver,
                },
        }) => {
            let mut engine = Engine::open(config)?;
            let settings = engine.resolver_settings().clone();
            let client = (resolver.as_deref() == Some("apple-fm")).then(|| {
                cli2::semantic::bridge::BridgeClient::new(
                    settings.bridge_path.clone(),
                    settings.timeout_secs,
                    settings.max_response_bytes,
                )
            });
            let res = engine.search_history(
                &text.join(" "),
                &cwd,
                &cli2::workflow::engine_ext::SearchOptions {
                    limit,
                    verified_only,
                    include_batch_steps,
                    freshness_budget: None,
                },
                client.as_ref(),
            )?;
            if json {
                print_json(&res);
            } else {
                print!("{}", render_history(&res));
            }
            Ok(0)
        }
        Some(Cmd::Eval {
            cmd:
                EvalCmd::Run {
                    suite,
                    matching,
                    repo,
                    out,
                    model,
                    only,
                },
        }) => {
            let root = repo.or_else(find_repo_root).ok_or_else(|| {
                EngineError::Invalid(
                    "cannot find the repository (fixtures/, evals/); pass --repo".into(),
                )
            })?;
            let suite = suite.unwrap_or_else(|| root.join("evals/workflows-v1.jsonl"));
            let matching = match matching.as_deref() {
                Some("none") => None,
                Some(p) => Some(PathBuf::from(p)),
                None => Some(root.join("evals/matching-v1.jsonl")).filter(|p| p.is_file()),
            };
            let report = cli2::eval::run_suite(
                &root,
                &suite,
                &cli2::eval::SuiteOptions {
                    only,
                    out_dir: out.clone(),
                    model,
                    matching,
                },
            )?;
            if let Some(dir) = &out {
                std::fs::create_dir_all(dir)?;
                std::fs::write(
                    dir.join("report.json"),
                    serde_json::to_string_pretty(&report).expect("serializable") + "\n",
                )?;
            }
            if json {
                print_json(&report);
            } else {
                print!("{}", render::eval_report(&report));
            }
            Ok(0)
        }
        Some(Cmd::Serve { socket }) => {
            let socket = socket.unwrap_or_else(|| config.home.join("runtime/cli2.sock"));
            let runtime = std::sync::Arc::new(cli2::runtime::Runtime::new(config));
            let server = cli2::server::Server::bind(runtime.clone(), &socket)
                .map_err(|e| EngineError::Invalid(e.to_string()))?;
            let reconciled = runtime
                .reconcile()
                .map_err(|e| EngineError::Invalid(e.to_string()))?;
            // Interrupted executions are recorded as such first; then unfinished
            // development tasks continue (they never re-run an interrupted execution).
            let resumed_tasks = server
                .controller()
                .recover()
                .map_err(|e| EngineError::Invalid(e.to_string()))?;
            let stop = server.stop_flag();
            let _ = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst));
            println!(
                "{}",
                serde_json::json!({
                    "event": "ready",
                    "socket": socket,
                    "instance_id": runtime.instance_id(),
                    "protocol_versions": cli2_protocol::SUPPORTED_VERSIONS,
                    "reconciled_executions": reconciled,
                    "resumed_tasks": resumed_tasks,
                })
            );
            let _ = std::io::stdout().flush();
            eprintln!(
                "cli2: serving protocol {:?} on {} (Ctrl-C to stop)",
                cli2_protocol::SUPPORTED_VERSIONS,
                socket.display()
            );
            let controller = server.controller();
            server.serve();
            controller.shutdown(std::time::Duration::from_secs(5));
            runtime.shutdown(std::time::Duration::from_secs(5));
            Ok(0)
        }
        Some(Cmd::Reject { plan_id, reason }) => {
            let mut engine = Engine::open(config)?;
            let result = engine.reject_plan(&plan_id, reason)?;
            Ok(report(&engine, &result, json))
        }
        Some(Cmd::Workflow { cmd }) => {
            let mut engine = Engine::open(config)?;
            match cmd {
                WorkflowCmd::Show { id } => {
                    let wf = engine.workflow_get(&id)?;
                    let v = engine.validate_workflows(&[&wf], &cwd)?.remove(0);
                    if json {
                        print_json(
                            &serde_json::json!({"schema_version": "cli2.workflow_view.v1", "workflow": wf, "validity": v}),
                        );
                    } else {
                        print!("{}", render_workflow(&wf, &v));
                    }
                    Ok(0)
                }
                WorkflowCmd::Replay {
                    id,
                    no_exec: _,
                    args,
                } => {
                    let mut overrides = BTreeMap::new();
                    for a in &args {
                        let (k, v) = a.split_once('=').ok_or_else(|| {
                            EngineError::Invalid(format!("--arg expects NAME=VALUE, got {a:?}"))
                        })?;
                        overrides.insert(k.to_lowercase(), v.to_string());
                    }
                    let replay = engine.replay_workflow(&id, &cwd, &overrides)?;
                    let code = replay
                        .steps
                        .iter()
                        .map(|s| s.result.status)
                        .find(|s| *s != Status::Planned)
                        .map(|s| s.exit_code())
                        .unwrap_or(0);
                    if json {
                        print_json(&replay);
                    } else {
                        print!("{}", render_replay(&replay));
                    }
                    Ok(code)
                }
                WorkflowCmd::Reindex => {
                    let (rep, fp) = engine.workflow_reindex()?;
                    if json {
                        print_json(&serde_json::json!({"index": rep, "fingerprint": fp}));
                    } else {
                        println!(
                            "Rebuilt the derived workflow index from the journal: {} workflow(s), fingerprint {}.{}",
                            rep.workflows,
                            &fp[..12],
                            rep.recovered_from.map(|p| format!(" The unusable previous file was moved to {p}.")).unwrap_or_default()
                        );
                    }
                    Ok(0)
                }
            }
        }
        Some(Cmd::Output { cmd }) => {
            let mut engine = Engine::open(config)?;
            output_command(&mut engine, cmd, json)
        }
        Some(Cmd::Batch {
            request_file,
            show,
            yes,
            timeout,
            capture_help,
            resolver,
        }) => {
            let mut engine = Engine::open(config)?;
            if let Some(id) = show {
                let v = engine.batch_record(&id)?;
                if json {
                    print_json(&v);
                } else {
                    print!("{}", render_batch_value(&v));
                }
                return Ok(0);
            }
            if let Some(mode) = resolver
                .as_deref()
                .and_then(cli2::semantic::ResolverMode::parse)
            {
                engine.set_resolver_mode(mode);
            }
            install_cancel_handler(&engine);
            let src = request_file.expect("clap requires --request-file or --show");
            let text = if src == "-" {
                let mut t = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)?;
                t
            } else {
                std::fs::read_to_string(&src)?
            };
            let file = cli2::batch::BatchFile::parse(&text)?;
            let opts = RunOptions {
                execute: true,
                yes,
                approve_token: None,
                timeout_secs: timeout,
                capture_help,
                interactive,
                ..Default::default()
            };
            let result = if json {
                cli2::batch::run_batch(&mut engine, &file, &opts, &mut NonInteractive)?
            } else {
                // Per-step previews and prompts go to stderr; the report is stdout.
                cli2::batch::run_batch(
                    &mut engine,
                    &file,
                    &opts,
                    &mut Human {
                        interactive,
                        quiet: true,
                    },
                )?
            };
            if json {
                print_json(&result);
            } else {
                print!(
                    "{}",
                    render_batch_value(&serde_json::to_value(&result).expect("serializable"))
                );
            }
            Ok(result.status.exit_code())
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
        ..Default::default()
    };
    let quiet = run.output.is_some();
    loop {
        let result = if json {
            engine.run(req.clone(), &opts, &mut NonInteractive)?
        } else {
            engine.run(req.clone(), &opts, &mut Human { interactive, quiet })?
        };
        // Interactive disambiguation: let the person pick, then resolve again.
        if interactive && result.status == Status::NeedsClarification {
            let options = result
                .clarification
                .as_ref()
                .map(|c| c.options.clone())
                .unwrap_or_default();
            if options.len() > 1 && req.selection.is_none() {
                if quiet {
                    eprint!("{}", render::result(&result));
                } else {
                    print!("{}", render::result(&result));
                }
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
        return Ok(match &run.output {
            Some(fmt) => emit_output(&engine, &result, fmt),
            None => report(&engine, &result, json),
        });
    }
}

/// Print a selected `--output` format. stdout carries only the selected
/// representation; everything else (failure notes, why nothing ran) goes to
/// stderr, and the exit code is the usual one for the request's status.
fn emit_output(engine: &Engine, result: &ExecutionResult, format: &str) -> i32 {
    use cli2::output::{started_steps, OutputId};
    let seq = engine.result_seq(&result.request_id).ok().flatten();
    let steps: Vec<_> = started_steps(result).collect();
    let id_of = |step: usize| seq.map(|seq| OutputId { seq, step }.format());
    match format {
        "json" => {
            let outputs: Vec<serde_json::Value> = steps
                .iter()
                .filter_map(|s| id_of(s.index))
                .filter_map(|id| engine.output_record(&id, true).ok())
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect();
            print_json(&serde_json::json!({
                "schema_version": "cli2.output_result.v1",
                "request_id": result.request_id,
                "plan_id": result.plan_id,
                "status": result.status,
                "executed": !steps.is_empty(),
                "message": result.message,
                "process_status": result.process_status,
                "verification": result.verification,
                "approval": result.approval,
                "clarification": result.clarification,
                "rejection": result.rejection,
                "limitations": result.limitations,
                "evidence_ref": result.evidence_ref,
                "outputs": outputs,
            }));
        }
        _ => {
            if steps.is_empty() {
                // Nothing ran: there is no response, and a plan is not a result.
                eprint!("{}", render::result(result));
            } else {
                for s in &steps {
                    let Some(id) = id_of(s.index) else { continue };
                    let part = if format == "response" {
                        cli2::output::Part::Response
                    } else {
                        cli2::output::Part::CommandAndResponse
                    };
                    match engine.output_part(&id, part) {
                        Ok(bytes) => {
                            let _ = std::io::stdout().write_all(&bytes);
                        }
                        Err(e) => eprintln!("cli2: {e}"),
                    }
                    if format == "response" {
                        if let Ok(err) = engine.output_part(&id, cli2::output::Part::Stderr) {
                            let _ = std::io::stderr().write_all(&err);
                        }
                    }
                }
                if result.status != Status::Succeeded {
                    // A failure is never hidden by choosing a quiet format.
                    eprintln!("cli2: {}", result.message);
                    for st in &steps {
                        if st.stdout_truncated || st.stderr_truncated {
                            eprintln!("cli2: output was truncated when captured; it is incomplete");
                        }
                    }
                } else if steps
                    .iter()
                    .any(|st| st.stdout_truncated || st.stderr_truncated)
                {
                    eprintln!("cli2: output was truncated when captured; it is incomplete");
                }
            }
        }
    }
    result.status.exit_code()
}

fn output_command(engine: &mut Engine, cmd: OutputCmd, json: bool) -> Result<i32, EngineError> {
    use cli2::output::Part;
    match cmd {
        OutputCmd::List { limit } => {
            let list = engine.outputs(limit)?;
            if json {
                print_json(
                    &serde_json::json!({"schema_version": cli2::output::OUTPUT_LIST_SCHEMA, "outputs": list}),
                );
            } else if list.is_empty() {
                println!("No captured results yet.");
            } else {
                for o in list {
                    let exit = o
                        .exit_code
                        .map(|c| format!("exit {c}"))
                        .unwrap_or_else(|| format!("{:?}", o.termination));
                    println!(
                        "{:<10} {}  {:<9} {:<19} {:<10} {}{}",
                        o.output_id,
                        o.started_at,
                        exit,
                        o.task_status,
                        format!("{:?}", o.verification).to_lowercase(),
                        o.command,
                        if o.truncated { "  [truncated]" } else { "" }
                    );
                }
            }
            Ok(0)
        }
        OutputCmd::Show { id } => {
            let rec = engine.output_record(&id, true)?;
            if json {
                print_json(&rec);
            } else {
                print!("{}", render_output_record(&rec));
            }
            Ok(0)
        }
        OutputCmd::Copy {
            id,
            part,
            clipboard,
        } => {
            let part = Part::parse(&part).expect("validated by clap");
            let bytes = engine.output_part(&id, part)?;
            if clipboard {
                cli2::clipboard::copy(&bytes).map_err(|e| EngineError::Invalid(e.to_string()))?;
                eprintln!("cli2: copied {} byte(s) to the clipboard", bytes.len());
            } else {
                std::io::stdout().write_all(&bytes)?;
            }
            Ok(0)
        }
        OutputCmd::Prune {
            older_than_days,
            dry_run,
        } => {
            let cutoff = cli2_core::rfc3339_days_ago(older_than_days);
            let r = engine.prune_outputs(&cutoff, dry_run)?;
            if json {
                print_json(&r);
            } else {
                println!(
                    "{} {} execution(s) older than {older_than_days} day(s): {} content blob(s), {} byte(s){}; {} kept because something newer shares them.",
                    if dry_run { "Would remove content of" } else { "Removed content of" },
                    r.executions, r.blobs_removed, r.bytes_freed,
                    if dry_run { " (dry run, nothing deleted)" } else { "" },
                    r.kept_shared
                );
            }
            Ok(0)
        }
    }
}

fn render_history(r: &cli2::workflow::engine_ext::SearchResult) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if r.hits.is_empty() {
        let _ = writeln!(
            s,
            "No matching history{}.",
            if r.query.is_empty() {
                String::new()
            } else {
                format!(" for {:?}", r.query)
            }
        );
        return s;
    }
    let _ = writeln!(
        s,
        "History matching {:?} (ordered by {}; history is evidence, not authority):",
        r.query, r.ordered_by
    );
    if let Some(n) = &r.model_note {
        let _ = writeln!(s, "  {n}");
    }
    if let Some(p) = &r.index.recovered_from {
        let _ = writeln!(
            s,
            "  (the derived index was unusable and was rebuilt from the journal; old file: {p})"
        );
    }
    for h in &r.hits {
        let w = &h.workflow;
        let _ = writeln!(
            s,
            "\n{}  [{:?}]  {:?}  {}  {:?}",
            w.id, h.validity.state, w.outcome, w.recorded_at, w.intent
        );
        for l in &h.explanation {
            let _ = writeln!(s, "    {l}");
        }
        let _ = writeln!(
            s,
            "    inspect: cli2 workflow show {}    fresh plan: cli2 workflow replay {}",
            w.id, w.id
        );
    }
    s
}

fn render_workflow(w: &cli2::workflow::Workflow, v: &cli2::workflow::validate::Validity) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}  ({:?}{})",
        w.id,
        w.kind,
        w.part_of_batch
            .as_ref()
            .map(|b| format!(", a step of {b}"))
            .unwrap_or_default()
    );
    let _ = writeln!(s, "  request       {:?}", w.intent);
    let _ = writeln!(s, "  recorded      {}", w.recorded_at);
    let _ = writeln!(s, "  outcome       {:?}", w.outcome);
    let _ = writeln!(
        s,
        "  eligibility   {}",
        if w.eligibility.eligible {
            "eligible (verified)".to_string()
        } else {
            format!("not eligible: {}", w.eligibility.reasons.join("; "))
        }
    );
    let _ = writeln!(s, "  validity now  {:?}  (in {})", v.state, v.checked_in);
    for r in &v.reasons {
        let _ = writeln!(s, "                ✘ {r}");
    }
    for r in &v.unconfirmed {
        let _ = writeln!(s, "                ? {r}");
    }
    for r in &v.confirmed {
        let _ = writeln!(s, "                ✓ {r}");
    }
    if let (Some(b), Some(f)) = (&w.batch_id, &w.on_failure) {
        let _ = writeln!(s, "  batch         {b}, on_failure {f}");
    }
    for st in &w.steps {
        let _ = writeln!(s, "  step {:<8} {:?}", st.step_id, st.outcome);
        if !st.intent.is_empty() {
            let _ = writeln!(s, "    request     {:?}", st.intent);
        }
        if let Some(c) = &st.capability_id {
            let _ = writeln!(s, "    capability  {c}");
        }
        if !st.recorded_argv.is_empty() {
            let _ = writeln!(
                s,
                "    ran as      {}   (for comparison; never replayed)",
                st.recorded_argv
                    .iter()
                    .map(|a| cli2_core::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if !st.depends_on.is_empty() {
            let _ = writeln!(s, "    depends on  {}", st.depends_on.join(", "));
        }
        if let Some(e) = &st.execution_id {
            let _ = writeln!(
                s,
                "    evidence    {e}  outputs {}  verification {:?}",
                st.output_ids.join(","),
                st.verification
            );
        }
        if let Some(a) = &st.recorded_approval {
            let _ = writeln!(
                s,
                "    approval    was {:?} / granted {} via {:?} (historical; not reused)",
                a.required, a.granted, a.method
            );
        }
    }
    let _ = writeln!(
        s,
        "  next          cli2 workflow replay {}   (builds a fresh plan; nothing executes)",
        w.id
    );
    s
}

fn render_replay(r: &cli2::workflow::engine_ext::Replay) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Fresh plan(s) from workflow {} — validity {:?}",
        r.workflow_id, r.validity.state
    );
    for x in r.validity.reasons.iter().chain(&r.validity.unconfirmed) {
        let _ = writeln!(s, "  ! {x}");
    }
    for st in &r.steps {
        let _ = writeln!(s, "\nstep {:?}: {:?}", st.step_id, st.intent);
        let _ = writeln!(
            s,
            "  recorded    {}   (historical; not reused)",
            st.recorded_argv.join(" ")
        );
        match (&st.result.plan, &st.fresh_argv) {
            (Some(p), Some(a)) => {
                let _ = writeln!(
                    s,
                    "  fresh plan  {}   [{}]",
                    a.iter()
                        .map(|x| cli2_core::shell_quote(x))
                        .collect::<Vec<_>>()
                        .join(" "),
                    if st.argv_differs == Some(true) {
                        "differs from the recorded command"
                    } else {
                        "same as the recorded command"
                    }
                );
                let _ = writeln!(
                    s,
                    "  approval    {:?} required now{}",
                    p.approval.level,
                    st.recorded_approval
                        .as_ref()
                        .map(|a| format!(
                            " (then: {:?}, granted {} — not carried over)",
                            a.required, a.granted
                        ))
                        .unwrap_or_default()
                );
                let _ = writeln!(
                    s,
                    "  plan        {}  digest {}   run it: cli2 exec {}",
                    p.plan_id,
                    p.short_digest(),
                    p.plan_id
                );
            }
            _ => {
                let _ = writeln!(s, "  blocked     {}", st.result.message);
            }
        }
    }
    let _ = writeln!(s, "\n{}", r.note);
    s
}

fn render_output_record(r: &cli2::output::OutputRecord) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}  ({} · request {}{})",
        r.output_id,
        r.execution_id,
        r.request_id,
        r.plan_id
            .as_ref()
            .map(|p| format!(" · plan {p}"))
            .unwrap_or_default()
    );
    let _ = writeln!(s, "  command       {}", r.command.display);
    let _ = writeln!(s, "  directory     {}", r.working_directory);
    let _ = writeln!(
        s,
        "  ran           {} for {} ms",
        r.started_at, r.duration_ms
    );
    let _ = writeln!(
        s,
        "  process       {}{}",
        cli2::output::termination_word(r.process.termination, r.process.signal),
        r.process
            .exit_code
            .map(|c| format!(", exit {c}"))
            .unwrap_or_default()
    );
    let _ = writeln!(s, "  task          {}: {}", r.task.status, r.task.message);
    let _ = writeln!(s, "  verification  {:?}", r.verification.status);
    for c in &r.verification.checks {
        let _ = writeln!(s, "                - {:?}: {}", c.status, c.detail);
    }
    for (name, st) in [("stdout", &r.stdout), ("stderr", &r.stderr)] {
        let _ = writeln!(
            s,
            "  {name:<13} {} byte(s){}{}",
            st.bytes,
            if st.truncated {
                format!(", TRUNCATED (kept {})", st.captured_bytes)
            } else {
                String::new()
            },
            if st.available {
                ""
            } else {
                ", content no longer retained"
            }
        );
    }
    for n in &r.notes {
        let _ = writeln!(s, "  note          {n}");
    }
    let _ = writeln!(s, "  evidence      {}", r.evidence_ref);
    let _ = writeln!(
        s,
        "  copy          cli2 output copy {} --part response|command-and-response|stdout|stderr",
        r.output_id
    );
    s
}

fn render_batch_value(v: &serde_json::Value) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Batch {}: {} ({})",
        v["batch_id"].as_str().unwrap_or("?"),
        v["status"].as_str().unwrap_or("?"),
        v["summary"].as_str().unwrap_or("")
    );
    for st in v["steps"].as_array().into_iter().flatten() {
        let status = st["status"]
            .as_str()
            .unwrap_or_else(|| st["state"].as_str().unwrap_or("?"));
        let mark = if status == "succeeded" { "✔" } else { "✘" };
        let outs: Vec<&str> = st["output_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str())
            .collect();
        let _ = writeln!(
            s,
            "  {mark} {:<14} {:<19} {}  {}",
            st["id"].as_str().unwrap_or("?"),
            status,
            outs.join(","),
            st["message"].as_str().unwrap_or("")
        );
        if let Some(d) = st["approval_digest"].as_str() {
            let _ = writeln!(
                s,
                "      approve with: cli2 exec {} --approve {}",
                st["plan_id"].as_str().unwrap_or("PLAN_ID"),
                d.trim_start_matches("sha256:")
                    .chars()
                    .take(12)
                    .collect::<String>()
            );
        }
    }
    let _ = writeln!(
        s,
        "  inspect a result: cli2 output show <ID>   copy: cli2 output copy <ID> --part response"
    );
    s
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
    /// Keep stdout clean for a selected `--output` format: previews are not
    /// printed, except that an approval prompt is always preceded by the
    /// preview, on stderr, so approval is never given blind.
    quiet: bool,
}

impl Interaction for Human {
    fn preview(&mut self, plan: &Plan) {
        if self.quiet {
            return;
        }
        print!("{}", render::preview(plan));
        let _ = std::io::stdout().flush();
    }

    fn ask(&mut self, plan: &Plan) -> ApprovalAnswer {
        if !self.interactive {
            return ApprovalAnswer::Unavailable;
        }
        if self.quiet {
            eprint!("{}", render::preview(plan));
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

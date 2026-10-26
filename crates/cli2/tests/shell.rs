//! Shell integration: generated output (portable) and zsh runtime behaviour
//! (skipped explicitly, never silently passed, when zsh is unavailable).

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use support::*;

fn cli2(args: &[&str]) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(args)
        // A pristine environment: no data directory exists, nothing may be created.
        .env("CLI2_HOME", tmp.path().join("nonexistent"))
        .env("HOME", tmp.path())
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("nonexistent").exists(),
        "generating output must not create the data directory"
    );
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        0,
        "must not write files"
    );
    String::from_utf8(out.stdout).unwrap()
}

fn code_lines(script: &str) -> Vec<&str> {
    script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect()
}

#[test]
fn shell_init_emits_a_versioned_non_empty_integration_deterministically() {
    let a = cli2(&["shell-init", "zsh"]);
    assert!(
        a.starts_with("# cli2 zsh integration, format version 1"),
        "{}",
        &a[..80]
    );
    assert!(a.contains("\nc2() {"));
    assert!(a.contains("compdef _cli2 cli2 c2"));
    assert!(a.lines().count() > 100);
    assert_eq!(
        a,
        cli2(&["shell-init", "zsh"]),
        "repeated generation is identical"
    );
    assert_eq!(cli2(&["completions", "zsh"]), cli2(&["completions", "zsh"]));
}

#[test]
fn integration_defines_no_global_hooks_aliases_bindings_or_evaluation() {
    let script = cli2(&["shell-init", "zsh"]);
    let code = code_lines(&script).join("\n");
    for forbidden in [
        "command_not_found_handler",
        "alias ",
        "bindkey",
        "setopt",
        "unsetopt",
        "eval ",
        "eval(",
        "chpwd",
        "zle ",
        "TRAPEXIT",
        "preexec",
        "compinit",
    ] {
        assert!(
            !code.contains(forbidden),
            "integration must not contain {forbidden:?}"
        );
    }
    // c2 itself: no directory changes and no flags of its own (the completion
    // text legitimately mentions cli2's flags).
    let start = script.find("\nc2() {").unwrap();
    let c2 = &script[start..script[start..].find("\n}\n").unwrap() + start];
    for forbidden in ["--yes", " -y", "--approve", "cd ", "pushd", "--cwd"] {
        assert!(
            !c2.contains(forbidden),
            "c2 must not contain {forbidden:?}:\n{c2}"
        );
    }
    // The only function that takes user input is c2, and it forwards "$@" untouched.
    assert!(code.contains("command cli2 \"$@\""));
}

#[test]
fn completions_come_from_the_real_command_tree() {
    let script = cli2(&["completions", "zsh"]);
    assert!(script.starts_with("#compdef cli2"));
    // Every subcommand cli2 reports in --help appears in the completion, so the
    // script cannot drift from the clap definition.
    let help = Command::new(BIN).arg("--help").output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    let mut commands = Vec::new();
    let mut in_commands = false;
    for l in help.lines() {
        if l.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            if !l.starts_with("  ") {
                break;
            }
            if let Some(name) = l.split_whitespace().next() {
                commands.push(name.to_string());
            }
        }
    }
    assert!(commands.len() >= 10, "{commands:?}");
    for c in commands.iter().filter(|c| *c != "help") {
        assert!(
            script.contains(&format!("{c}:")) || script.contains(&format!("'{c}")),
            "completion lacks `{c}`"
        );
    }
    // Nested subcommands, flags and constrained values.
    for needle in [
        "status",
        "benchmark",
        "show",
        "list",
        "--resolver=",
        "(deterministic apple-fm auto)",
        "--capture-help",
        "--no-exec",
        "--refresh-help",
        "--cwd=",
        "shell-init",
        "completions",
    ] {
        assert!(script.contains(needle), "completion lacks {needle:?}");
    }
}

#[test]
fn completion_output_needs_no_model_scan_or_data_directory() {
    // Generated with HOME and CLI2_HOME pointing nowhere and PATH empty: pure text.
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["completions", "zsh"])
        .env_clear()
        .env("CLI2_HOME", tmp.path().join("nope"))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!tmp.path().join("nope").exists());
}

#[test]
fn existing_exit_codes_and_commands_are_unchanged() {
    let env = Env::new();
    let proj = env.project("node-project");
    // Unknown intent still exits 4, invalid usage 2, as documented.
    let (code, _) = env.json(&proj, &["order a pizza with extra cheese"]);
    assert_eq!(code, 4);
    let r = env.run(&proj, &["task", "show", "req_missing"]);
    assert_eq!(r.code, 2);
    // `shell-init` with an unsupported shell is a usage error.
    let r = env.run(&proj, &["shell-init", "fish"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
}

// ------------------------------------------------------------ zsh runtime

fn zsh() -> Option<PathBuf> {
    which("zsh")
}

macro_rules! need_zsh {
    () => {
        match zsh() {
            Some(z) => z,
            None => {
                eprintln!("SKIPPED (zsh runtime test, not a pass): zsh is not installed");
                return;
            }
        }
    };
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    bin: PathBuf,
    script: PathBuf,
    argv_log: PathBuf,
}

/// A temp dir holding the generated integration and a recording stand-in for
/// `cli2` (and for git), so forwarding can be observed exactly.
fn sandbox() -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let argv_log = dir.join("argv.log");
    // One argument per record, NUL-terminated, so nothing is lost to word splitting.
    write_executable(
        &bin.join("cli2"),
        &format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{}'\nprintf 'cwd=%s\\n' \"$(pwd)\" > '{}.cwd'\nexit ${{FAKE_EXIT:-0}}\n",
            argv_log.display(),
            argv_log.display()
        ),
    );
    write_executable(&bin.join("git"), "#!/bin/sh\necho \"real git: $*\"\n");
    let script = dir.join("cli2.zsh");
    std::fs::write(&script, cli2(&["shell-init", "zsh"])).unwrap();
    Sandbox {
        _tmp: tmp,
        dir,
        bin,
        script,
        argv_log,
    }
}

impl Sandbox {
    fn zsh(&self, z: &Path, body: &str, with_cli2: bool) -> (i32, String, String) {
        let mut path = vec!["/usr/bin".to_string(), "/bin".to_string()];
        if with_cli2 {
            path.insert(0, self.bin.display().to_string());
        } else {
            // Keep git, drop the cli2 stand-in.
            let only_git = self.dir.join("only-git");
            std::fs::create_dir_all(&only_git).unwrap();
            let _ = std::os::unix::fs::symlink(self.bin.join("git"), only_git.join("git"));
            path.insert(0, only_git.display().to_string());
        }
        let out = Command::new(z)
            .args(["-f", "-c", body])
            .env_clear()
            .env("PATH", path.join(":"))
            .env("HOME", &self.dir)
            .current_dir(&self.dir)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into(),
            String::from_utf8_lossy(&out.stderr).into(),
        )
    }

    fn argv(&self) -> Vec<String> {
        let data = std::fs::read(&self.argv_log).unwrap();
        // Records are NUL-terminated; keep empty arguments.
        let mut recs: Vec<String> = data
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into())
            .collect();
        recs.pop();
        recs
    }
}

#[test]
fn generated_scripts_are_valid_zsh() {
    let z = need_zsh!();
    let sb = sandbox();
    let completions = sb.dir.join("_cli2");
    std::fs::write(&completions, cli2(&["completions", "zsh"])).unwrap();
    for f in [&sb.script, &completions] {
        let out = Command::new(&z).arg("-n").arg(f).output().unwrap();
        assert!(
            out.status.success(),
            "{}: {}",
            f.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn c2_forwards_quoted_multi_word_requests_and_flags_without_mutation() {
    let z = need_zsh!();
    let sb = sandbox();
    let src = format!("source '{}'", sb.script.display());
    let (code, _, err) = sb.zsh(
        &z,
        &format!(
            "{src}; c2 'find all references to authentication' --resolver auto --capture-help"
        ),
        true,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sb.argv(),
        [
            "find all references to authentication",
            "--resolver",
            "auto",
            "--capture-help"
        ]
    );

    // Awkward arguments survive intact: spaces, quotes, globs, leading dashes, empty strings.
    let (code, _, err) = sb.zsh(
        &z,
        &format!(
            "{src}; c2 plan 'say \"hi\" * ? [a] $HOME ~ `x` ; | &' '' -- '--odd value' 'it'\\''s'"
        ),
        true,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sb.argv(),
        [
            "plan",
            "say \"hi\" * ? [a] $HOME ~ `x` ; | &",
            "",
            "--",
            "--odd value",
            "it's"
        ]
    );

    // Subcommands pass straight through.
    sb.zsh(&z, &format!("{src}; c2 scan --rebuild"), true);
    assert_eq!(sb.argv(), ["scan", "--rebuild"]);
}

#[test]
fn c2_never_adds_yes_or_approval_and_forwards_the_exit_status() {
    let z = need_zsh!();
    let sb = sandbox();
    let src = format!("source '{}'", sb.script.display());
    sb.zsh(&z, &format!("{src}; c2 'publish the package'"), true);
    let argv = sb.argv();
    assert_eq!(argv, ["publish the package"]);
    assert!(!argv
        .iter()
        .any(|a| a == "--yes" || a == "-y" || a == "--approve"));
    // cli2's exit codes (3 = awaiting approval) reach the caller unchanged.
    let out = Command::new(&z)
        .args([
            "-f",
            "-c",
            &format!("{src}; c2 'publish the package'; print -r -- \"status=$?\""),
        ])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", sb.bin.display()))
        .env("FAKE_EXIT", "3")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("status=3"));
}

#[test]
fn c2_runs_in_the_current_directory_and_does_not_change_it() {
    let z = need_zsh!();
    let sb = sandbox();
    let proj = sb.dir.join("some project");
    std::fs::create_dir_all(&proj).unwrap();
    let src = format!("source '{}'", sb.script.display());
    let (code, out, err) = sb.zsh(
        &z,
        &format!(
            "cd '{}'; {src}; c2 scan; print -r -- \"after=$PWD\"",
            proj.display()
        ),
        true,
    );
    assert_eq!(code, 0, "{err}");
    let cwd = std::fs::read_to_string(format!("{}.cwd", sb.argv_log.display())).unwrap();
    assert!(cwd.trim().ends_with("some project"), "{cwd}");
    assert!(out.contains("some project"), "{out}");
    // No arguments: usage on stderr, status 2, and cli2 is not run.
    let _ = std::fs::remove_file(&sb.argv_log);
    let (code, _, err) = sb.zsh(&z, &format!("{src}; c2"), true);
    assert_eq!(code, 2);
    assert!(err.contains("usage"), "{err}");
    assert!(!sb.argv_log.exists());
}

#[test]
fn ordinary_commands_and_shell_state_are_untouched() {
    let z = need_zsh!();
    let sb = sandbox();
    let src = format!("source '{}'", sb.script.display());
    // `git status` still runs the real command, and nothing was routed to cli2.
    let (code, out, err) = sb.zsh(&z, &format!("{src}; git status"), true);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "real git: status");
    assert!(
        !sb.argv_log.exists(),
        "cli2 must not be invoked for ordinary commands"
    );
    // An unknown command is still "command not found" (no handler installed).
    let (code, _, err) = sb.zsh(
        &z,
        &format!("{src}; definitely-not-a-command-xyz arg"),
        true,
    );
    assert_eq!(code, 127);
    assert!(err.contains("not found"), "{err}");
    assert!(!sb.argv_log.exists());
    // Aliases, key bindings, options, command_not_found_handler: identical before and after.
    let probe = "print -r -- \"alias:$(alias)\"; print -r -- \"opts:$(setopt)\"; print -r -- \"bind:$(bindkey 2>/dev/null)\"; print -r -- \"cnf:$+functions[command_not_found_handler]\"; print -r -- \"pwd:$PWD\"; print -r -- \"fns:${(kj:,:)functions}\"";
    let (_, before, _) = sb.zsh(&z, probe, true);
    let (_, after, _) = sb.zsh(&z, &format!("{src}; {probe}"), true);
    let strip_fns = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("fns:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip_fns(&before), strip_fns(&after));
    assert!(after.contains("cnf:0"));
    // The only functions added are c2, cli2's completion functions, and the one-shot registrar.
    let fns = |s: &str| -> std::collections::BTreeSet<String> {
        s.lines().find(|l| l.starts_with("fns:")).unwrap()[4..]
            .split(',')
            .map(String::from)
            .collect()
    };
    let added: Vec<String> = fns(&after).difference(&fns(&before)).cloned().collect();
    for f in &added {
        assert!(
            f == "c2"
                || f.starts_with("_cli2")
                || f == "_cli2_register_completion"
                || f.starts_with("add-zsh-hook")
                || f.starts_with("is-at-least")
                || f.starts_with("_cli2"),
            "unexpected function {f}"
        );
    }
    assert!(added.iter().any(|f| f == "c2"));
}

#[test]
fn missing_cli2_gives_a_useful_diagnostic_and_never_breaks_startup() {
    let z = need_zsh!();
    let sb = sandbox();
    // Sourcing without cli2 on PATH succeeds ...
    let (code, out, err) = sb.zsh(
        &z,
        &format!("source '{}'; print -r -- started", sb.script.display()),
        false,
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("started"));
    // ... interactive startup too ...
    let o = Command::new(&z)
        .args([
            "-f",
            "-i",
            "-c",
            &format!(
                "source '{}'; print -r -- interactive-ok",
                sb.script.display()
            ),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &sb.dir)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("interactive-ok"));
    // ... and c2 explains what is wrong and returns 127 instead of aborting the shell.
    let (code, out, err) = sb.zsh(
        &z,
        &format!(
            "source '{}'; c2 'run the tests'; print -r -- \"status=$?\"; print -r -- still-alive",
            sb.script.display()
        ),
        false,
    );
    assert_eq!(code, 0);
    assert!(
        out.contains("status=127") && out.contains("still-alive"),
        "{out}"
    );
    assert!(
        err.contains("cli2 executable was not found") && err.contains("cargo install"),
        "{err}"
    );
}

#[test]
fn a_missing_integration_file_is_reported_but_the_documented_guard_keeps_startup_going() {
    let z = need_zsh!();
    let sb = sandbox();
    let missing = sb.dir.join("shell/cli2.zsh");
    let guarded = format!(
        "f='{}'; [[ -r $f ]] && source $f || print -u2 \"cli2: integration not found at $f (generate it with: cli2 shell-init zsh)\"; print -r -- startup-continues",
        missing.display()
    );
    let (code, out, err) = sb.zsh(&z, &guarded, true);
    assert_eq!(code, 0);
    assert!(out.contains("startup-continues"));
    assert!(err.contains("cli2 shell-init zsh"), "{err}");
    // Even the plain `source` line from the documentation does not stop startup.
    let (_, out, err) = sb.zsh(
        &z,
        &format!(
            "source '{}'; print -r -- startup-continues",
            missing.display()
        ),
        true,
    );
    assert!(out.contains("startup-continues"));
    assert!(!err.is_empty());
}

#[test]
fn completion_registers_whether_sourced_before_or_after_compinit_and_is_idempotent() {
    let z = need_zsh!();
    let sb = sandbox();
    let cd = sb.dir.join("zcompdump");
    let src = format!("source '{}'", sb.script.display());
    // After compinit: registered immediately, for both names.
    let (code, out, err) = sb.zsh(&z, &format!("autoload -Uz compinit; compinit -u -d '{}'; {src}; {src}; print -r -- \"${{_comps[cli2]}} ${{_comps[c2]}}\"", cd.display()), true);
    assert_eq!(code, 0, "{err}");
    assert!(out.trim().ends_with("_cli2 _cli2"), "{out}");
    // Before compinit: a one-shot registrar waits for the completion system.
    let (code, out, err) = sb.zsh(&z, &format!("{src}; print -r -- \"pending=$+functions[_cli2_register_completion]\"; autoload -Uz compinit; compinit -u -d '{}'; _cli2_register_completion; print -r -- \"${{_comps[cli2]}} ${{_comps[c2]}} pending=$+functions[_cli2_register_completion]\"", cd.display()), true);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("pending=1") && out.contains("_cli2 _cli2 pending=0"),
        "{out}"
    );
}

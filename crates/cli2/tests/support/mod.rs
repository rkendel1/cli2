//! Shared harness for acceptance tests: isolated data directory, controlled
//! PATH, fixture projects copied into temporary directories, and the real
//! `cli2` binary executing real subprocesses.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_cli2");

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub fn fixture(name: &str) -> PathBuf {
    repo_root().join("fixtures").join(name)
}

/// Locate an executable on the test process's PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

pub fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() == "target" || entry.file_name() == "node_modules" {
                continue;
            }
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

pub fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Host executables the fixture projects and tests rely on, and nothing else.
pub const HOST_TOOLS: &[&str] = &[
    "sh", "env", "node", "npm", "npx", "make", "cargo", "rustc", "rustup", "mkdir", "rm", "cp",
    "mv", "sleep", "cat", "touch", "ls", "dirname", "basename", "uname", "true", "false", "sudo",
    "printf", "sed", "tr", "head", "tail", "wc", "sort", "test", "echo",
    // C toolchain for linking Rust fixtures.
    "cc", "clang", "ld", "ar", "xcrun", "dsymutil", "strip", "lipo", "gcc",
];

pub struct Env {
    _tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub work: PathBuf,
    pub path: OsString,
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

impl Env {
    /// A fresh environment. `with_tools` lists executables (node, make, cargo,
    /// ...) whose directories are added to PATH after the private bin dir.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let bin = root.join("bin");
        let work = root.join("work");
        for d in [&home, &bin, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Hermetic PATH: only the private bin directory, populated with
        // symlinks to the specific tools the fixtures need. The tests must not
        // depend on (or be disturbed by) whatever else the host has installed:
        // e.g. macOS ships `/usr/bin/write`, which is a perfectly good match
        // for "write a summary report" and has nothing to do with `mkreport`.
        for tool in HOST_TOOLS {
            if let Some(p) = which(tool) {
                let link = bin.join(tool);
                if !link.exists() {
                    let _ = std::os::unix::fs::symlink(&p, &link);
                }
            }
        }
        if let Some(c) = std::env::var_os("CARGO") {
            let c = PathBuf::from(c);
            let link = bin.join("cargo");
            if c.is_file() && !link.exists() {
                let _ = std::os::unix::fs::symlink(&c, &link);
            }
        }
        let dirs = vec![bin.clone()];
        let path = std::env::join_paths(dirs).unwrap();
        Env {
            _tmp: tmp,
            home,
            bin,
            work,
            path,
        }
    }

    /// Copy a fixture project into the work directory and return its path.
    pub fn project(&self, name: &str) -> PathBuf {
        let dst = self.work.join(name);
        copy_dir(&fixture(name), &dst);
        dst
    }

    pub fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(dir)
            .env("CLI2_HOME", &self.home)
            .env("PATH", &self.path)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null());
        cmd
    }

    pub fn run(&self, dir: &Path, args: &[&str]) -> Run {
        let out: Output = self.command(dir, args).output().expect("cli2 runs");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run with `--json` and parse stdout.
    pub fn json(&self, dir: &Path, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let r = self.run(dir, &all);
        let v = r.json();
        (r.code, v)
    }

    pub fn inventory_ids(&self, dir: &Path) -> Vec<String> {
        let (code, v) = self.json(dir, &["inventory"]);
        assert_eq!(code, 0, "{v}");
        v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect()
    }
}

pub fn has_node() -> bool {
    which("node").is_some() && which("npm").is_some()
}

pub fn has_make() -> bool {
    which("make").is_some()
}

/// Skip a test (with a visible message) when a required tool is missing.
#[macro_export]
macro_rules! require {
    ($cond:expr, $what:expr) => {
        if !$cond {
            eprintln!("SKIPPED: {} is not available", $what);
            return;
        }
    };
}

pub fn schema(name: &str) -> Value {
    let p = repo_root()
        .join("schemas/v1")
        .join(format!("{name}.schema.json"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Validate an instance against a published schema, panicking with details.
pub fn assert_valid(name: &str, instance: &Value) {
    let schema = schema(name);
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{name} schema violations: {errors:#?}\ninstance: {instance:#}"
    );
}

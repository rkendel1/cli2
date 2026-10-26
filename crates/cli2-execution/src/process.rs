//! Bounded subprocess execution.
//!
//! Processes are started from an explicit executable path and argument vector
//! (no shell), with stdin closed, in their own process group so that timeouts
//! and cancellation terminate the whole tree. This is **not** a sandbox: the
//! child runs with the caller's user, environment and filesystem access.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use cli2_core::Termination;

/// Default cap on captured bytes per stream.
pub const DEFAULT_MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Grace period between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
}

impl ProcessSpec {
    pub fn new(
        program: impl Into<PathBuf>,
        args: Vec<String>,
        cwd: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: cwd.into(),
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT,
            kill_grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub termination: Termination,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub pid: Option<u32>,
    pub started: SystemTime,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_total: u64,
    pub stderr_total: u64,
    pub error: Option<String>,
}

impl ProcessOutcome {
    pub fn stdout_truncated(&self) -> bool {
        self.stdout_total > self.stdout.len() as u64
    }
    pub fn stderr_truncated(&self) -> bool {
        self.stderr_total > self.stderr.len() as u64
    }
}

/// A cancellation flag shared with signal handlers.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut r: R,
    cap: usize,
) -> thread::JoinHandle<(Vec<u8>, u64)> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut total: u64 = 0;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total += n as u64;
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        (kept, total)
    })
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // SAFETY: kill(2) with a negative pid signals the process group we created.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

/// Run a process to completion, timeout, or cancellation.
pub fn run(spec: &ProcessSpec, cancel: &CancelToken) -> ProcessOutcome {
    let started = SystemTime::now();
    let started_at = cli2_core::now_rfc3339();
    let clock = Instant::now();

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProcessOutcome {
                termination: Termination::SpawnFailed,
                exit_code: None,
                signal: None,
                pid: None,
                started,
                started_at,
                finished_at: cli2_core::now_rfc3339(),
                duration_ms: clock.elapsed().as_millis() as u64,
                stdout: vec![],
                stderr: vec![],
                stdout_total: 0,
                stderr_total: 0,
                error: Some(format!("failed to start {}: {e}", spec.program.display())),
            }
        }
    };
    let pid = child.id();
    let out_reader = spawn_reader(
        child.stdout.take().expect("piped stdout"),
        spec.max_output_bytes,
    );
    let err_reader = spawn_reader(
        child.stderr.take().expect("piped stderr"),
        spec.max_output_bytes,
    );

    let mut forced: Option<Termination> = None;
    let mut term_sent_at: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if forced.is_none() {
            if cancel.is_cancelled() {
                forced = Some(Termination::Cancelled);
            } else if clock.elapsed() >= spec.timeout {
                forced = Some(Termination::TimedOut);
            }
            if forced.is_some() {
                #[cfg(unix)]
                signal_group(pid, libc::SIGTERM);
                #[cfg(not(unix))]
                let _ = child.kill();
                term_sent_at = Some(Instant::now());
            }
        } else if let Some(at) = term_sent_at {
            if at.elapsed() >= spec.kill_grace {
                #[cfg(unix)]
                signal_group(pid, libc::SIGKILL);
                let _ = child.kill();
                term_sent_at = None;
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Make sure nothing in the group outlives a forced termination.
    #[cfg(unix)]
    if forced.is_some() {
        signal_group(pid, libc::SIGKILL);
    }

    let (stdout, stdout_total) = out_reader.join().unwrap_or_default();
    let (stderr, stderr_total) = err_reader.join().unwrap_or_default();
    let duration_ms = clock.elapsed().as_millis() as u64;
    let finished_at = cli2_core::now_rfc3339();

    let (termination, exit_code, signal, error) = match status {
        Ok(status) => {
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            let termination = match forced {
                Some(t) => t,
                None if status.code().is_some() => Termination::Exited,
                None => Termination::Signaled,
            };
            (termination, status.code(), signal, None)
        }
        Err(e) => (
            forced.unwrap_or(Termination::Signaled),
            None,
            None,
            Some(format!("failed waiting for process: {e}")),
        ),
    };

    ProcessOutcome {
        termination,
        exit_code,
        signal,
        pid: Some(pid),
        started,
        started_at,
        finished_at,
        duration_ms,
        stdout,
        stderr,
        stdout_total,
        stderr_total,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> ProcessOutcome {
        let spec = ProcessSpec::new("/bin/sh", vec!["-c".into(), script.into()], "/", timeout);
        run(&spec, &CancelToken::new())
    }

    #[test]
    fn captures_success_and_failure() {
        let o = sh("echo out; echo err >&2; exit 0", Duration::from_secs(10));
        assert_eq!(o.termination, Termination::Exited);
        assert_eq!(o.exit_code, Some(0));
        assert_eq!(o.stdout, b"out\n");
        assert_eq!(o.stderr, b"err\n");
        let o = sh("exit 3", Duration::from_secs(10));
        assert_eq!(o.exit_code, Some(3));
    }

    #[test]
    fn timeout_kills_process_group() {
        let o = sh(
            "sleep 30 & sleep 30; echo never",
            Duration::from_millis(300),
        );
        assert_eq!(o.termination, Termination::TimedOut);
        assert!(o.duration_ms < 5000, "took {}ms", o.duration_ms);
        assert!(o.stdout.is_empty());
    }

    #[test]
    fn cancellation() {
        let token = CancelToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            t2.cancel();
        });
        let spec = ProcessSpec::new(
            "/bin/sh",
            vec!["-c".into(), "sleep 30".into()],
            "/",
            Duration::from_secs(60),
        );
        let o = run(&spec, &token);
        assert_eq!(o.termination, Termination::Cancelled);
    }

    #[test]
    fn output_is_bounded() {
        let mut spec = ProcessSpec::new(
            "/bin/sh",
            vec!["-c".into(), "head -c 100000 /dev/zero".into()],
            "/",
            Duration::from_secs(10),
        );
        spec.max_output_bytes = 1000;
        let o = run(&spec, &CancelToken::new());
        assert_eq!(o.stdout.len(), 1000);
        assert_eq!(o.stdout_total, 100000);
        assert!(o.stdout_truncated());
    }

    #[test]
    fn spawn_failure_is_reported() {
        let spec = ProcessSpec::new("/nonexistent/prog", vec![], "/", Duration::from_secs(1));
        let o = run(&spec, &CancelToken::new());
        assert_eq!(o.termination, Termination::SpawnFailed);
        assert!(o.error.is_some());
    }
}

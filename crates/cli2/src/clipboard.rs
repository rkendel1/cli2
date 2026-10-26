//! Opt-in clipboard copy (`cli2 output copy … --clipboard`).
//!
//! Uses the platform's own clipboard utility, found on `PATH`:
//! macOS `pbcopy`; Linux `wl-copy`, `xclip` or `xsel`; Windows `clip`.
//! There is no fallback to any other destination: if the utility is missing
//! or fails, that is an error and nothing else is done with the content.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard copy is not supported on this platform; use `cli2 output copy … > file` or pipe the output")]
    Unsupported,
    #[error("no clipboard utility found on PATH (looked for: {}); install one or pipe the output instead", .0.join(", "))]
    NoUtility(Vec<String>),
    #[error("the clipboard utility `{tool}` failed: {detail}")]
    Failed { tool: String, detail: String },
}

fn candidates() -> &'static [(&'static str, &'static [&'static str])] {
    if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "linux") {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[]
    }
}

fn find(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

pub fn copy(bytes: &[u8]) -> Result<(), ClipboardError> {
    let cands = candidates();
    if cands.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    let Some((name, args, path)) = cands.iter().find_map(|(n, a)| find(n).map(|p| (*n, *a, p)))
    else {
        return Err(ClipboardError::NoUtility(
            cands.iter().map(|(n, _)| n.to_string()).collect(),
        ));
    };
    let fail = |detail: String| ClipboardError::Failed {
        tool: name.into(),
        detail,
    };
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fail(e.to_string()))?;
    let mut stdin = child.stdin.take().expect("piped");
    let data = bytes.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().map_err(|e| fail(e.to_string()))? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail("timed out after 10 s".into()));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let write_result = writer.join();
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        return Err(fail(format!(
            "exited with {status}{}",
            if err.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", err.trim())
            }
        )));
    }
    match write_result {
        Ok(Ok(())) => Ok(()),
        _ => Err(fail("could not write the content to the utility".into())),
    }
}

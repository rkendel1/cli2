//! Applying a file-edit plan step to the filesystem.
//!
//! What this module guarantees (Linux and macOS), and what it does not:
//!
//! * **Containment.** The target is reached by walking from a directory
//!   descriptor for the project root, one component at a time, with
//!   `openat(O_NOFOLLOW)`. A symbolic link in any component is refused at the
//!   moment of use, so it cannot be swapped in between a check and the open.
//!   (The root directory itself is opened by its canonical path with
//!   `O_NOFOLLOW`; components *above* the root are outside this guarantee.)
//! * **Only regular files**, owned by the current user, with a single link.
//! * **No lost update.** The new content is written to a temporary file in the
//!   target's directory and committed with an atomic *exchange* of the two
//!   names (`renameat2(RENAME_EXCHANGE)` / `renameatx_np(RENAME_SWAP)`), not a
//!   plain rename. The file that was displaced is then read through the
//!   descriptor opened before the exchange and must still hash to the expected
//!   value. If anything changed it between the first check and the exchange,
//!   the displaced version is put back (and the outcome says so). A creation is
//!   committed with `link(2)`, which fails if the name now exists.
//! * **Fail closed.** If the filesystem cannot exchange names atomically, or the
//!   platform is unsupported, nothing is written.
//!
//! Not guaranteed: a process that already holds an open descriptor to the old
//! file and writes through it *after* the exchange has committed is not seen
//! (inherent to replace-by-rename), and extended attributes, ACLs and ownership
//! of the old file are not carried over. Concurrent writers that replace the
//! file by renaming over it during the commit are detected and undone on a best
//! effort basis; this is detect-and-restore, not mutual exclusion.

use cli2_core::{
    apply_hunks, content_hash, FileEditOutcome, FileEditStatus, FileEditStep, MAX_FILE_BYTES,
};
use std::path::Path;

/// A file as read for planning or verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// Hooks that run at fixed points of the commit. They exist so tests can
/// interleave a concurrent writer deterministically; production passes none.
#[derive(Default)]
pub struct Hooks<'a> {
    /// After the expected content has been confirmed and the temporary file is
    /// written, immediately before the commit.
    pub before_commit: Option<&'a dyn Fn()>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{0}")]
    Refused(String),
    #[error("{0} does not exist")]
    Missing(String),
}

/// Read a file below `root` with the same containment rules as an edit.
pub fn read_file(root: &Path, rel: &str) -> Result<FileSnapshot, ReadError> {
    imp::read_file(root, rel)
}

/// Apply a plan's file-edit step. Never panics on filesystem conditions; every
/// outcome, including refusal, is described by the returned record.
pub fn apply(step: &FileEditStep) -> FileEditOutcome {
    apply_with(step, &Hooks::default())
}

pub fn apply_with(step: &FileEditStep, hooks: &Hooks<'_>) -> FileEditOutcome {
    imp::apply(step, hooks)
}

fn outcome(
    step: &FileEditStep,
    status: FileEditStatus,
    message: impl Into<String>,
) -> FileEditOutcome {
    FileEditOutcome {
        status,
        path: step.path.clone(),
        root: step.root.clone(),
        expected_sha256: step.expected_sha256.clone(),
        before_sha256: None,
        planned_sha256: step.result_sha256.clone(),
        after_sha256: None,
        bytes_before: None,
        bytes_after: None,
        message: message.into(),
    }
}

/// The content the step's hunks produce from `before`, if it is what the plan
/// promised.
fn planned_content(step: &FileEditStep, before: &[u8]) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(before).map_err(|_| "the file is not valid UTF-8".to_string())?;
    let new = apply_hunks(text, &step.hunks)?;
    if new.len() > MAX_FILE_BYTES {
        return Err("the result is too large".into());
    }
    let bytes = new.into_bytes();
    if content_hash(&bytes) != step.result_sha256 {
        return Err(
            "applying the hunks does not produce the content hash recorded in the plan".into(),
        );
    }
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn cstr(s: &str) -> io::Result<CString> {
        CString::new(s).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }

    fn openat(
        dir: RawFd,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<OwnedFd> {
        let c = cstr(name)?;
        // SAFETY: `c` is a valid NUL-terminated string and `dir` a descriptor we hold.
        let fd = unsafe {
            libc::openat(
                dir,
                c.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: openat returned a new descriptor that nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn open_root(root: &Path) -> Result<OwnedFd, String> {
        let c = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| "the project root contains a NUL byte".to_string())?;
        // SAFETY: valid C string.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(format!(
                "cannot open the project root {}: {}",
                root.display(),
                io::Error::last_os_error()
            ));
        }
        // SAFETY: new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn describe(e: &io::Error, what: &str) -> String {
        match e.raw_os_error() {
            Some(libc::ELOOP) => format!("{what} is or passes through a symbolic link"),
            Some(libc::ENOTDIR) => format!(
                "{what} passes through something that is not a directory (or is a symbolic link)"
            ),
            Some(libc::ENOENT) => format!("{what} does not exist"),
            _ => format!("{what}: {e}"),
        }
    }

    /// Walk to the directory containing the target.
    fn parent_dir(root: &Path, rel: &str) -> Result<(OwnedFd, String), String> {
        let comps: Vec<&str> = rel.split('/').collect();
        let (name, dirs) = comps.split_last().ok_or("empty path")?;
        let mut cur = open_root(root)?;
        for d in dirs {
            cur = openat(
                cur.as_raw_fd(),
                d,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| describe(&e, &format!("directory `{d}` of {rel}")))?;
        }
        Ok((cur, (*name).to_string()))
    }

    enum Opened {
        Missing,
        File(File),
    }

    /// Open the target for reading without following links; non-regular files,
    /// foreign owners and multiply-linked files are refused.
    fn open_target(parent: RawFd, name: &str, rel: &str) -> Result<Opened, String> {
        let fd = match openat(
            parent,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Opened::Missing),
            Err(e) => return Err(describe(&e, rel)),
        };
        let f = File::from(fd);
        let m = f.metadata().map_err(|e| format!("{rel}: {e}"))?;
        if !m.file_type().is_file() {
            return Err(format!("{rel} is not a regular file"));
        }
        if m.nlink() != 1 {
            return Err(format!(
                "{rel} has {} hard links; replacing it would silently detach them",
                m.nlink()
            ));
        }
        // SAFETY: geteuid has no failure modes.
        if m.uid() != unsafe { libc::geteuid() } {
            return Err(format!("{rel} is not owned by the current user"));
        }
        if m.len() as usize > MAX_FILE_BYTES {
            return Err(format!("{rel} is larger than {MAX_FILE_BYTES} bytes"));
        }
        Ok(Opened::File(f))
    }

    fn read_all(f: &File) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut off = 0u64;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let n = f.read_at(&mut chunk, off)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            off += n as u64;
            if buf.len() > MAX_FILE_BYTES {
                return Err(io::Error::other("file grew beyond the size limit"));
            }
        }
        Ok(buf)
    }

    pub fn read_file(root: &Path, rel: &str) -> Result<FileSnapshot, ReadError> {
        let (parent, name) = parent_dir(root, rel).map_err(ReadError::Refused)?;
        match open_target(parent.as_raw_fd(), &name, rel).map_err(ReadError::Refused)? {
            Opened::Missing => Err(ReadError::Missing(rel.to_string())),
            Opened::File(f) => {
                let bytes = read_all(&f).map_err(|e| ReadError::Refused(format!("{rel}: {e}")))?;
                let sha256 = content_hash(&bytes);
                Ok(FileSnapshot { bytes, sha256 })
            }
        }
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_name() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(
            ".cli2-edit-{}-{}-{}.tmp",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn stat_at(dir: RawFd, name: &str) -> io::Result<(u64, u64)> {
        let c = cstr(name)?;
        // SAFETY: zeroed stat is a valid out-parameter for fstatat.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointers; AT_SYMLINK_NOFOLLOW never follows a final link.
        let r = unsafe { libc::fstatat(dir, c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((st.st_dev as u64, st.st_ino as u64))
        }
    }

    fn unlink_at(dir: RawFd, name: &str) {
        if let Ok(c) = cstr(name) {
            // SAFETY: valid C string and descriptor.
            unsafe { libc::unlinkat(dir, c.as_ptr(), 0) };
        }
    }

    fn rename_at(dir: RawFd, from: &str, to: &str) -> io::Result<()> {
        let (a, b) = (cstr(from)?, cstr(to)?);
        // SAFETY: valid C strings and descriptor.
        let r = unsafe { libc::renameat(dir, a.as_ptr(), dir, b.as_ptr()) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn link_at(dir: RawFd, from: &str, to: &str) -> io::Result<()> {
        let (a, b) = (cstr(from)?, cstr(to)?);
        // SAFETY: valid C strings and descriptor.
        let r = unsafe { libc::linkat(dir, a.as_ptr(), dir, b.as_ptr(), 0) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Atomically swap two names in one directory.
    fn exchange(dir: RawFd, a: &str, b: &str) -> io::Result<()> {
        let (ca, cb): (CString, CString) = (cstr(a)?, cstr(b)?);
        exchange_c(dir, &ca, &cb)
    }

    #[cfg(target_os = "linux")]
    fn exchange_c(dir: RawFd, a: &CStr, b: &CStr) -> io::Result<()> {
        const RENAME_EXCHANGE: libc::c_uint = 2;
        // SAFETY: valid C strings and descriptor; renameat2 has no other inputs.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                dir,
                a.as_ptr(),
                dir,
                b.as_ptr(),
                RENAME_EXCHANGE,
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    fn exchange_c(dir: RawFd, a: &CStr, b: &CStr) -> io::Result<()> {
        extern "C" {
            fn renameatx_np(
                fromfd: libc::c_int,
                from: *const libc::c_char,
                tofd: libc::c_int,
                to: *const libc::c_char,
                flags: libc::c_uint,
            ) -> libc::c_int;
        }
        const RENAME_SWAP: libc::c_uint = 0x2;
        // SAFETY: valid C strings and descriptor.
        let r = unsafe { renameatx_np(dir, a.as_ptr(), dir, b.as_ptr(), RENAME_SWAP) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn fsync_dir(dir: RawFd) {
        // SAFETY: fsync on a descriptor we hold; a failure is not actionable here.
        unsafe { libc::fsync(dir) };
    }

    pub fn apply(step: &FileEditStep, hooks: &Hooks<'_>) -> FileEditOutcome {
        let fail = |status, msg: String| outcome(step, status, msg);
        let refuse = |msg: String| outcome(step, FileEditStatus::Refused, msg);
        let root = Path::new(&step.root);
        let rel = step.path.as_str();

        let (parent, name) = match parent_dir(root, rel) {
            Ok(p) => p,
            Err(e) => return refuse(e),
        };
        let dir = parent.as_raw_fd();
        let opened = match open_target(dir, &name, rel) {
            Ok(o) => o,
            Err(e) => return refuse(e),
        };

        // What is there now, and is it what the patch was prepared against?
        let (old_file, before, old_mode) = match (&step.expected_sha256, opened) {
            (None, Opened::Missing) => (None, Vec::new(), 0o644),
            (None, Opened::File(_)) => {
                return fail(
                    FileEditStatus::Stale,
                    format!("{rel} already exists; the plan creates it"),
                )
            }
            (Some(_), Opened::Missing) => {
                return fail(
                    FileEditStatus::Stale,
                    format!(
                        "{rel} no longer exists; the patch was prepared against an existing file"
                    ),
                )
            }
            (Some(_), Opened::File(f)) => {
                let bytes = match read_all(&f) {
                    Ok(b) => b,
                    Err(e) => return refuse(format!("{rel}: {e}")),
                };
                let mode = f.metadata().map(|m| m.mode() & 0o7777).unwrap_or(0o644);
                (Some(f), bytes, mode)
            }
        };
        let before_hash = old_file.as_ref().map(|_| content_hash(&before));
        let mut o = outcome(step, FileEditStatus::Refused, String::new());
        o.before_sha256 = before_hash.clone();
        o.bytes_before = old_file.as_ref().map(|_| before.len() as u64);
        if before_hash != step.expected_sha256 {
            o.status = FileEditStatus::Stale;
            o.message = format!(
                "{rel} changed since the patch was prepared (expected {}, found {}); nothing was written",
                step.expected_sha256.as_deref().unwrap_or("a new file"),
                before_hash.as_deref().unwrap_or("none")
            );
            return o;
        }
        let new = match planned_content(step, &before) {
            Ok(n) => n,
            Err(e) => {
                o.message = e;
                return o;
            }
        };

        // Stage the new content next to the target.
        let tmp = temp_name();
        let tmp_fd = match openat(
            dir,
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                o.message = describe(&e, "creating the temporary file");
                return o;
            }
        };
        let mut tmp_file = File::from(tmp_fd);
        let staged = tmp_file
            .write_all(&new)
            .and_then(|_| tmp_file.sync_all())
            .and_then(|_| {
                // SAFETY: fchmod on a descriptor we own.
                let r = unsafe { libc::fchmod(tmp_file.as_raw_fd(), old_mode as libc::mode_t) };
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        if let Err(e) = staged {
            unlink_at(dir, &tmp);
            o.message = format!("writing the temporary file: {e}");
            return o;
        }
        let new_ident = match tmp_file.metadata() {
            Ok(m) => (m.dev(), m.ino()),
            Err(e) => {
                unlink_at(dir, &tmp);
                o.message = format!("{e}");
                return o;
            }
        };

        if let Some(h) = hooks.before_commit {
            h();
        }

        // Commit.
        match &old_file {
            None => {
                // Creation: link fails if the name now exists.
                let linked = link_at(dir, &tmp, &name);
                unlink_at(dir, &tmp);
                if let Err(e) = linked {
                    o.status = if e.raw_os_error() == Some(libc::EEXIST) {
                        FileEditStatus::Conflict
                    } else {
                        FileEditStatus::Refused
                    };
                    o.message = if e.raw_os_error() == Some(libc::EEXIST) {
                        format!("{rel} was created concurrently; nothing was overwritten")
                    } else {
                        format!("creating {rel}: {e}")
                    };
                    return o;
                }
            }
            Some(old) => {
                let old_ident = match old.metadata() {
                    Ok(m) => (m.dev(), m.ino()),
                    Err(e) => {
                        unlink_at(dir, &tmp);
                        o.message = format!("{e}");
                        return o;
                    }
                };
                if let Err(e) = exchange(dir, &tmp, &name) {
                    unlink_at(dir, &tmp);
                    o.message = match e.raw_os_error() {
                        Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => format!(
                            "this filesystem cannot exchange names atomically ({e}); refusing to edit {rel} without that guarantee"
                        ),
                        _ => describe(&e, &format!("committing {rel}")),
                    };
                    return o;
                }
                // `tmp` now names whatever used to be at `name`. It must be the
                // inode we read, still holding the content we hashed.
                let displaced_ok = stat_at(dir, &tmp)
                    .map(|(d, i)| (d, i) == old_ident)
                    .unwrap_or(false)
                    && read_all(old).map(|b| content_hash(&b)).ok() == step.expected_sha256;
                if !displaced_ok {
                    return undo(dir, &tmp, &name, new_ident, step, o);
                }
                unlink_at(dir, &tmp);
            }
        }
        fsync_dir(dir);

        // Read back what is on disk now.
        match read_file(root, rel) {
            Ok(snap) => {
                o.after_sha256 = Some(snap.sha256.clone());
                o.bytes_after = Some(snap.bytes.len() as u64);
                if snap.sha256 == step.result_sha256 {
                    o.status = FileEditStatus::Applied;
                    o.message = format!("{rel} was edited and reads back as planned");
                } else {
                    o.status = FileEditStatus::Mismatch;
                    o.message = format!(
                        "{rel} was edited but reads back with a different content hash than planned ({}); it was probably modified again right after the commit",
                        snap.sha256
                    );
                }
            }
            Err(e) => {
                o.status = FileEditStatus::Mismatch;
                o.message = format!("{rel} was edited but could not be read back: {e}");
            }
        }
        o
    }

    /// The exchange displaced something other than the file we validated.
    /// Put back what we displaced without destroying anything that arrived
    /// concurrently.
    fn undo(
        dir: RawFd,
        tmp: &str,
        name: &str,
        ours: (u64, u64),
        step: &FileEditStep,
        mut o: FileEditOutcome,
    ) -> FileEditOutcome {
        o.status = FileEditStatus::Conflict;
        let rel = &step.path;
        // Is our new file still the one at `name`?
        let ours_at_name = stat_at(dir, name)
            .map(|(d, i)| (d, i) == ours)
            .unwrap_or(false);
        if ours_at_name && exchange(dir, tmp, name).is_ok() {
            // `tmp` should again be our file. If something else slipped in
            // between the check and this exchange, swap it back.
            let tmp_is_ours = stat_at(dir, tmp)
                .map(|(d, i)| (d, i) == ours)
                .unwrap_or(false);
            if tmp_is_ours {
                unlink_at(dir, tmp);
                fsync_dir(dir);
                o.message = format!(
                    "{rel} was changed by someone else while the edit was being committed; the other version was restored and nothing was lost"
                );
                return o;
            }
            if exchange(dir, tmp, name).is_ok() {
                // What is at `name` again is what arrived concurrently; `tmp`
                // holds the version this edit displaced. Keep it, visibly.
                let kept = format!("{name}.cli2-displaced");
                let at = if rename_at(dir, tmp, &kept).is_ok() {
                    kept
                } else {
                    tmp.to_string()
                };
                o.message = format!(
                    "{rel} was changed concurrently more than once while the edit was being committed; the edit was withdrawn and what arrived concurrently is in place; the version this edit displaced is preserved as {at}"
                );
                return o;
            }
        }
        // We cannot safely put it back: leave everything that exists where it
        // is, and say where the displaced version is.
        let kept = format!("{name}.cli2-displaced");
        let preserved = rename_at(dir, tmp, &kept).is_ok();
        o.message = format!(
            "{rel} was replaced concurrently while the edit was being committed and could not be restored automatically; {}",
            if preserved {
                format!("the version that was displaced is preserved as {kept}")
            } else {
                format!("the version that was displaced is at {tmp}")
            }
        );
        o
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;

    pub fn read_file(_: &Path, _: &str) -> Result<FileSnapshot, ReadError> {
        Err(ReadError::Refused(
            "file operations are not supported on this platform".into(),
        ))
    }

    pub fn apply(step: &FileEditStep, _: &Hooks<'_>) -> FileEditOutcome {
        outcome(
            step,
            FileEditStatus::Refused,
            "file edits are not supported on this platform: the atomic exchange needed to make them safe is not available",
        )
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use cli2_core::{FileEditKind, Hunk, FILE_EDIT_CAPABILITY_ID};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn step(
        root: &Path,
        path: &str,
        before: Option<&str>,
        old: &str,
        new: &str,
        start: usize,
    ) -> FileEditStep {
        let hunks = vec![Hunk {
            start_line: start,
            old_text: old.into(),
            new_text: new.into(),
        }];
        let base = before.unwrap_or("");
        let result = apply_hunks(base, &hunks).unwrap();
        FileEditStep {
            kind: FileEditKind::FileEdit,
            index: 0,
            capability_id: FILE_EDIT_CAPABILITY_ID.into(),
            root: root.canonicalize().unwrap().display().to_string(),
            path: path.into(),
            expected_sha256: before.map(|b| content_hash(b.as_bytes())),
            hunks,
            result_sha256: content_hash(result.as_bytes()),
            result_bytes: result.len() as u64,
        }
    }

    fn no_temp_files(dir: &Path) {
        for e in fs::read_dir(dir).unwrap() {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!n.contains(".cli2-edit-"), "leftover temporary file {n}");
        }
    }

    #[test]
    fn applies_a_patch_to_the_version_it_was_prepared_against() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("src")).unwrap();
        let f = d.path().join("src/a.txt");
        fs::write(&f, "one\ntwo\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        let s = step(
            d.path(),
            "src/a.txt",
            Some("one\ntwo\n"),
            "two\n",
            "TWO\n",
            2,
        );
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Applied, "{}", o.message);
        assert_eq!(fs::read_to_string(&f).unwrap(), "one\nTWO\n");
        assert_eq!(o.after_sha256.as_deref(), Some(s.result_sha256.as_str()));
        assert_eq!(o.before_sha256, s.expected_sha256);
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o755,
            "the mode is preserved"
        );
        no_temp_files(&d.path().join("src"));
    }

    #[test]
    fn a_stale_patch_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        fs::write(&f, "one\ntwo\n").unwrap();
        let s = step(d.path(), "a.txt", Some("one\ntwo\n"), "two\n", "TWO\n", 2);
        fs::write(&f, "one\ntwo\nthree\n").unwrap(); // someone edited it since
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Stale);
        assert_eq!(fs::read_to_string(&f).unwrap(), "one\ntwo\nthree\n");
        no_temp_files(d.path());
    }

    #[test]
    fn a_change_between_the_check_and_the_commit_is_not_overwritten_in_place() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        fs::write(&f, "one\ntwo\n").unwrap();
        let s = step(d.path(), "a.txt", Some("one\ntwo\n"), "two\n", "TWO\n", 2);
        let fp = f.clone();
        let race = move || fs::write(&fp, "one\ntwo\nconcurrent\n").unwrap();
        let o = apply_with(
            &s,
            &Hooks {
                before_commit: Some(&race),
            },
        );
        assert_eq!(o.status, FileEditStatus::Conflict, "{}", o.message);
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "one\ntwo\nconcurrent\n",
            "the concurrent edit survives"
        );
        no_temp_files(d.path());
    }

    #[test]
    fn a_file_replaced_by_rename_during_the_commit_is_restored() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        fs::write(&f, "one\ntwo\n").unwrap();
        let s = step(d.path(), "a.txt", Some("one\ntwo\n"), "two\n", "TWO\n", 2);
        let (fp, other) = (f.clone(), d.path().join("other"));
        let race = move || {
            fs::write(&other, "someone else's rewrite\n").unwrap();
            fs::rename(&other, &fp).unwrap();
        };
        let o = apply_with(
            &s,
            &Hooks {
                before_commit: Some(&race),
            },
        );
        assert_eq!(o.status, FileEditStatus::Conflict, "{}", o.message);
        assert_eq!(fs::read_to_string(&f).unwrap(), "someone else's rewrite\n");
        no_temp_files(d.path());
    }

    #[test]
    fn a_file_created_concurrently_is_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let s = step(d.path(), "new.txt", None, "", "hello\n", 1);
        let fp = d.path().join("new.txt");
        let race = move || fs::write(&fp, "theirs\n").unwrap();
        let o = apply_with(
            &s,
            &Hooks {
                before_commit: Some(&race),
            },
        );
        assert_eq!(o.status, FileEditStatus::Conflict, "{}", o.message);
        assert_eq!(
            fs::read_to_string(d.path().join("new.txt")).unwrap(),
            "theirs\n"
        );
        no_temp_files(d.path());
    }

    #[test]
    fn creates_a_new_file_that_did_not_exist() {
        let d = tempfile::tempdir().unwrap();
        let s = step(d.path(), "new.txt", None, "", "hello\n", 1);
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Applied, "{}", o.message);
        assert_eq!(
            fs::read_to_string(d.path().join("new.txt")).unwrap(),
            "hello\n"
        );
        no_temp_files(d.path());
        // Applying the same creation again must not clobber it.
        let again = apply(&s);
        assert_eq!(again.status, FileEditStatus::Stale);
    }

    #[test]
    fn symlinks_hard_links_and_special_files_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "one\n").unwrap();

        // Symlinked file.
        symlink(outside.path().join("secret"), d.path().join("link")).unwrap();
        let s = step(d.path(), "link", Some("one\n"), "one\n", "two\n", 1);
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Refused, "{}", o.message);
        assert!(o.message.contains("symbolic link"), "{}", o.message);
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "one\n"
        );

        // Symlinked directory component.
        symlink(outside.path(), d.path().join("dir")).unwrap();
        let s = step(d.path(), "dir/secret", Some("one\n"), "one\n", "two\n", 1);
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Refused, "{}", o.message);
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "one\n"
        );

        // Hard link.
        fs::write(d.path().join("a"), "one\n").unwrap();
        fs::hard_link(d.path().join("a"), d.path().join("b")).unwrap();
        let s = step(d.path(), "a", Some("one\n"), "one\n", "two\n", 1);
        assert_eq!(apply(&s).status, FileEditStatus::Refused);
        assert_eq!(fs::read_to_string(d.path().join("b")).unwrap(), "one\n");

        // Directory and FIFO.
        fs::create_dir(d.path().join("sub")).unwrap();
        let s = step(d.path(), "sub", Some("one\n"), "one\n", "two\n", 1);
        assert_eq!(apply(&s).status, FileEditStatus::Refused);
        let fifo = std::ffi::CString::new(d.path().join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let s = step(d.path(), "fifo", Some("one\n"), "one\n", "two\n", 1);
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Refused, "{}", o.message);
        assert!(o.message.contains("not a regular file"), "{}", o.message);

        // A missing parent directory is not created.
        let s = step(d.path(), "no/such/file", None, "", "x\n", 1);
        assert_eq!(apply(&s).status, FileEditStatus::Refused);
        assert!(!d.path().join("no").exists());
        no_temp_files(d.path());
    }

    #[test]
    fn a_plan_whose_hunks_do_not_produce_its_recorded_hash_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        fs::write(&f, "one\n").unwrap();
        let mut s = step(d.path(), "a.txt", Some("one\n"), "one\n", "two\n", 1);
        s.hunks[0].new_text = "something else\n".into();
        let o = apply(&s);
        assert_eq!(o.status, FileEditStatus::Refused, "{}", o.message);
        assert_eq!(fs::read_to_string(&f).unwrap(), "one\n");
    }

    #[test]
    fn reading_follows_the_same_containment_rules() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "hello\n").unwrap();
        let snap = read_file(d.path(), "a.txt").unwrap();
        assert_eq!(snap.bytes, b"hello\n");
        assert_eq!(snap.sha256, content_hash(b"hello\n"));
        assert!(matches!(
            read_file(d.path(), "nope"),
            Err(ReadError::Missing(_))
        ));
        symlink(d.path().join("a.txt"), d.path().join("l")).unwrap();
        assert!(matches!(
            read_file(d.path(), "l"),
            Err(ReadError::Refused(_))
        ));
        fs::write(d.path().join("big"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(matches!(
            read_file(d.path(), "big"),
            Err(ReadError::Refused(_))
        ));
    }
}

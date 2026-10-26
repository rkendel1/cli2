//! File modification as a first-class plan step.
//!
//! A [`FileEditStep`] carries everything that determines the change: the
//! project root, the normalized target path, the content hash the patch was
//! prepared against, the exact hunks, and the hash of the content they produce.
//! All of it is part of the plan digest, so an approval covers the exact patch
//! that will be applied, not a description of it.
//!
//! The functions here are pure. Applying a plan step to the filesystem lives in
//! `cli2-execution`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FILE_EDIT_CAPABILITY_ID: &str = "builtin:file_edit";

/// Largest file a plan may read or rewrite.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Largest total patch (old plus new text) in one step.
pub const MAX_PATCH_BYTES: usize = 256 * 1024;
pub const MAX_HUNKS: usize = 64;
const MAX_PATH_BYTES: usize = 1024;

/// Marks a step as a file edit in JSON (process steps carry no `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileEditKind {
    FileEdit,
}

/// Replace `old_text` (which must match the file exactly, starting at the
/// 1-based line `start_line`) with `new_text`. Both are whole lines, including
/// their line endings. An empty `old_text` inserts before `start_line`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hunk {
    pub start_line: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEditStep {
    pub kind: FileEditKind,
    pub index: usize,
    /// Always [`FILE_EDIT_CAPABILITY_ID`].
    pub capability_id: String,
    /// Absolute project root. The edit can never leave it.
    pub root: String,
    /// Normalized path relative to `root`, `/`-separated.
    pub path: String,
    /// SHA-256 (`sha256:<hex>`) of the file the patch was prepared against.
    /// `None` means the file must not exist and the edit creates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub hunks: Vec<Hunk>,
    /// SHA-256 of the content the hunks produce from the expected content.
    pub result_sha256: String,
    pub result_bytes: u64,
}

impl FileEditStep {
    /// Reviewable form of the exact change.
    pub fn unified_diff(&self) -> String {
        let mut s = format!(
            "--- {}{}\n+++ {}\n",
            self.path,
            if self.expected_sha256.is_none() {
                " (new file)"
            } else {
                ""
            },
            self.path
        );
        let mut shift: isize = 0;
        for h in &self.hunks {
            let old_n = count_lines(&h.old_text);
            let new_n = count_lines(&h.new_text);
            let new_start = (h.start_line as isize + shift).max(0) as usize;
            s.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.start_line, old_n, new_start, new_n
            ));
            for l in h.old_text.split_inclusive('\n') {
                s.push('-');
                s.push_str(l);
                if !l.ends_with('\n') {
                    s.push_str("\n\\ No newline at end of file\n");
                }
            }
            for l in h.new_text.split_inclusive('\n') {
                s.push('+');
                s.push_str(l);
                if !l.ends_with('\n') {
                    s.push_str("\n\\ No newline at end of file\n");
                }
            }
            shift += new_n as isize - old_n as isize;
        }
        s
    }
}

fn count_lines(s: &str) -> usize {
    s.split_inclusive('\n').count()
}

pub fn content_hash(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Normalize a caller-supplied relative path. Absolute paths, `..`, empty or
/// over-long paths, NULs and backslashes are refused; `.` components and
/// repeated separators are dropped. This is a syntactic rule only: whether the
/// path crosses a symlink is decided when it is opened.
pub fn normalize_rel_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("path is empty".into());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path is longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.contains('\0') {
        return Err("path contains a NUL byte".into());
    }
    if path.contains('\\') {
        return Err("path contains a backslash".into());
    }
    if path.starts_with('/') {
        return Err("path must be relative to the project root".into());
    }
    let mut parts = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err("path must not contain `..`".into()),
            c if c.len() > 255 => return Err("a path component is longer than 255 bytes".into()),
            c => parts.push(c),
        }
    }
    if parts.is_empty() {
        return Err("path names no file".into());
    }
    Ok(parts.join("/"))
}

/// Validate hunk structure independent of any file.
pub fn validate_hunks(hunks: &[Hunk]) -> Result<(), String> {
    if hunks.is_empty() {
        return Err("the patch has no hunks".into());
    }
    if hunks.len() > MAX_HUNKS {
        return Err(format!("the patch has more than {MAX_HUNKS} hunks"));
    }
    let total: usize = hunks
        .iter()
        .map(|h| h.old_text.len() + h.new_text.len())
        .sum();
    if total > MAX_PATCH_BYTES {
        return Err(format!("the patch is larger than {MAX_PATCH_BYTES} bytes"));
    }
    for (i, h) in hunks.iter().enumerate() {
        if h.start_line == 0 {
            return Err(format!("hunk {i}: lines are numbered from 1"));
        }
        if h.old_text == h.new_text {
            return Err(format!("hunk {i} changes nothing"));
        }
        if h.old_text.contains('\0') || h.new_text.contains('\0') {
            return Err(format!(
                "hunk {i} contains a NUL byte; binary edits are not supported"
            ));
        }
    }
    Ok(())
}

/// Apply `hunks` to `content`, requiring every hunk's `old_text` to match the
/// content exactly where it claims to be. Hunks must be ordered and must not
/// overlap. Nothing is guessed: a hunk that does not match is an error.
pub fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String, String> {
    validate_hunks(hunks)?;
    if content.contains('\0') {
        return Err("the file contains NUL bytes; binary files cannot be edited".into());
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::with_capacity(content.len());
    let mut next = 0usize; // index of the first line not yet copied
    for (i, h) in hunks.iter().enumerate() {
        let at = h.start_line - 1;
        let n = count_lines(&h.old_text);
        if at < next {
            return Err(format!("hunk {i} overlaps or precedes the previous hunk"));
        }
        if at + n > lines.len() || (n == 0 && at > lines.len()) {
            return Err(format!(
                "hunk {i} starts at line {} but the file has {} lines",
                h.start_line,
                lines.len()
            ));
        }
        let have: String = lines[at..at + n].concat();
        if have != h.old_text {
            return Err(format!(
                "hunk {i} does not match the file at line {}",
                h.start_line
            ));
        }
        for l in &lines[next..at] {
            out.push_str(l);
        }
        out.push_str(&h.new_text);
        next = at + n;
    }
    for l in &lines[next..] {
        out.push_str(l);
    }
    if out.len() > MAX_FILE_BYTES {
        return Err(format!("the result would exceed {MAX_FILE_BYTES} bytes"));
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileEditStatus {
    /// The planned content was committed and read back as planned.
    Applied,
    /// The target no longer had the content the patch was prepared against.
    /// Nothing was changed.
    Stale,
    /// Refused or failed without changing the target (unsafe path, not a
    /// regular file, platform cannot make the commit safe, I/O error, ...).
    Refused,
    /// A change landed concurrently while the edit was being committed. The
    /// previous version was restored where that was safe; see `message`.
    Conflict,
    /// The edit was committed but reading it back did not give the planned
    /// content. The target needs attention.
    Mismatch,
}

/// What actually happened to the file. Recorded as evidence for the step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEditOutcome {
    pub status: FileEditStatus,
    pub path: String,
    pub root: String,
    /// Hash the patch was prepared against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    /// Hash of the file as found when the edit was attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<String>,
    /// Hash the plan said the result would have.
    pub planned_sha256: String,
    /// Hash read back from disk after the commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_after: Option<u64>,
    pub message: String,
}

impl FileEditOutcome {
    pub fn applied(&self) -> bool {
        self.status == FileEditStatus::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(start: usize, old: &str, new: &str) -> Hunk {
        Hunk {
            start_line: start,
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn paths_are_normalized_and_unsafe_ones_refused() {
        assert_eq!(normalize_rel_path("./a//b/./c.rs").unwrap(), "a/b/c.rs");
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a\\b",
            "a\0b",
            ".",
            "./",
        ] {
            assert!(normalize_rel_path(bad).is_err(), "{bad:?}");
        }
        assert!(normalize_rel_path(&"x/".repeat(600)).is_err());
    }

    #[test]
    fn hunks_apply_only_where_they_match() {
        let c = "one\ntwo\nthree\n";
        assert_eq!(
            apply_hunks(c, &[h(2, "two\n", "TWO\nand more\n")]).unwrap(),
            "one\nTWO\nand more\nthree\n"
        );
        assert_eq!(
            apply_hunks(c, &[h(1, "", "zero\n"), h(3, "three\n", "")]).unwrap(),
            "zero\none\ntwo\n"
        );
        assert_eq!(
            apply_hunks(c, &[h(4, "", "four\n")]).unwrap(),
            "one\ntwo\nthree\nfour\n"
        );
        assert!(apply_hunks(c, &[h(2, "tw\n", "x\n")]).is_err());
        assert!(apply_hunks(c, &[h(3, "three\nfour\n", "x\n")]).is_err());
        assert!(apply_hunks(c, &[h(5, "", "x\n")]).is_err());
        assert!(apply_hunks(c, &[h(2, "two\n", "x\n"), h(1, "one\n", "y\n")]).is_err());
        assert!(apply_hunks(c, &[h(1, "one\n", "one\n")]).is_err());
        assert!(apply_hunks(c, &[]).is_err());
        assert!(apply_hunks("a\0b", &[h(1, "a\0b", "c")]).is_err());
    }

    #[test]
    fn an_unterminated_last_line_is_matched_exactly() {
        assert_eq!(
            apply_hunks("a\nb", &[h(2, "b", "b\nc")]).unwrap(),
            "a\nb\nc"
        );
        assert!(apply_hunks("a\nb", &[h(2, "b\n", "x\n")]).is_err());
    }

    #[test]
    fn the_diff_shows_the_exact_change() {
        let step = FileEditStep {
            kind: FileEditKind::FileEdit,
            index: 0,
            capability_id: FILE_EDIT_CAPABILITY_ID.into(),
            root: "/p".into(),
            path: "src/a.txt".into(),
            expected_sha256: Some(content_hash(b"x")),
            hunks: vec![h(2, "two\n", "TWO\nmore\n")],
            result_sha256: content_hash(b"y"),
            result_bytes: 1,
        };
        let d = step.unified_diff();
        assert!(d.contains("@@ -2,1 +2,2 @@\n-two\n+TWO\n+more\n"), "{d}");
    }
}

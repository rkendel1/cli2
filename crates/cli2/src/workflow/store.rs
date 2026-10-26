//! The derived workflow index: `$CLI2_HOME/workflows.db`.
//!
//! Disposable by design. It holds nothing the journal does not: every row is
//! a pure function of the journal's contents ([`super::derive`]). It is
//! rebuilt when missing, damaged, built by another derivation version, or
//! behind the journal. Nothing here is ever consulted as a source of truth.

use std::path::{Path, PathBuf};

use cli2_evidence::Journal;
use rusqlite::{params, Connection, OptionalExtension};

use super::{derive, Workflow, BUILD_VERSION};
use crate::engine::EngineError;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS workflows (
    ord INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, recorded_at TEXT NOT NULL, json TEXT NOT NULL);
"#;

pub struct WorkflowIndex {
    conn: Connection,
    path: PathBuf,
}

/// What happened while making the index current.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct IndexReport {
    pub rebuilt: bool,
    /// The previous file was unusable and was moved here.
    pub recovered_from: Option<String>,
    pub workflows: usize,
}

fn err(e: rusqlite::Error) -> EngineError {
    EngineError::Invalid(format!("workflow index: {e}"))
}

fn open_checked(path: &Path) -> Option<Connection> {
    let conn = Connection::open(path).ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
    let ok: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .ok()?;
    if ok != "ok" {
        return None;
    }
    conn.execute_batch(SCHEMA).ok()?;
    // The tables must really be ours (a different SQLite file would pass quick_check).
    conn.query_row("SELECT COUNT(*) FROM workflows", [], |r| r.get::<_, i64>(0))
        .ok()?;
    Some(conn)
}

impl WorkflowIndex {
    /// Open the index; an unusable file is moved aside (never read as truth).
    pub fn open(path: &Path) -> Result<(Self, Option<String>), EngineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut recovered = None;
        let conn = match open_checked(path) {
            Some(c) => c,
            None => {
                if path.exists() {
                    let aside = path.with_extension(format!("db.unusable-{}", std::process::id()));
                    std::fs::rename(path, &aside)?;
                    recovered = Some(aside.display().to_string());
                }
                // A stale WAL/journal from the old file must not follow it.
                for ext in ["db-wal", "db-shm", "db-journal"] {
                    let _ = std::fs::remove_file(path.with_extension(ext));
                }
                let c = Connection::open(path).map_err(err)?;
                c.execute_batch(SCHEMA).map_err(err)?;
                c
            }
        };
        Ok((
            Self {
                conn,
                path: path.to_path_buf(),
            },
            recovered,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    fn is_current(&self, journal: &Journal) -> bool {
        let Ok((r, e)) = journal.watermark() else {
            return false;
        };
        self.meta("build_version").as_deref() == Some(&BUILD_VERSION.to_string())
            && self.meta("results_watermark").as_deref() == Some(&r.to_string())
            && self.meta("events_watermark").as_deref() == Some(&e.to_string())
    }

    /// Make the index agree with the journal; rebuild only if it does not.
    pub fn ensure_current(&mut self, journal: &Journal) -> Result<bool, EngineError> {
        if self.is_current(journal) {
            return Ok(false);
        }
        self.rebuild(journal)?;
        Ok(true)
    }

    /// Replace the contents with a fresh derivation from the journal.
    pub fn rebuild(&mut self, journal: &Journal) -> Result<usize, EngineError> {
        let (r, e) = journal.watermark()?;
        let workflows = derive(journal)?;
        let tx = self.conn.transaction().map_err(err)?;
        tx.execute("DELETE FROM workflows", []).map_err(err)?;
        tx.execute("DELETE FROM meta", []).map_err(err)?;
        for (i, w) in workflows.iter().enumerate() {
            tx.execute(
                "INSERT INTO workflows(ord, id, recorded_at, json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    i as i64,
                    w.id,
                    w.recorded_at,
                    serde_json::to_string(w).expect("serializable")
                ],
            )
            .map_err(err)?;
        }
        for (k, v) in [
            ("build_version", BUILD_VERSION.to_string()),
            ("results_watermark", r.to_string()),
            ("events_watermark", e.to_string()),
        ] {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                params![k, v],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(workflows.len())
    }

    /// All workflows in index order. A row that does not parse means the file
    /// is damaged; the caller recovers by rebuilding.
    pub fn list(&self) -> Result<Vec<Workflow>, EngineError> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM workflows ORDER BY ord")
            .map_err(err)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(err)?;
            out.push(serde_json::from_str(&json).map_err(|e| {
                EngineError::Invalid(format!("workflow index row is damaged: {e}"))
            })?);
        }
        Ok(out)
    }

    /// Hash of the derived contents (not of timestamps of indexing), for
    /// checking that two rebuilds are identical.
    pub fn fingerprint(&self) -> Result<String, EngineError> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let mut stmt = self
            .conn
            .prepare("SELECT id, json FROM workflows ORDER BY ord")
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?;
        for row in rows {
            let (id, json) = row.map_err(err)?;
            h.update(id.as_bytes());
            h.update(b"\n");
            h.update(json.as_bytes());
            h.update(b"\n");
        }
        Ok(hex::encode(h.finalize()))
    }
}

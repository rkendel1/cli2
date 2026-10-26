//! The derived workflow index: `$CLI2_HOME/workflows.db`.
//!
//! Disposable by design. It holds nothing the journal does not: every row is
//! a pure function of the journal's contents ([`super::derive`]). It is
//! rebuilt when missing, damaged, built by another derivation version, or
//! behind the journal. Nothing here is ever consulted as a source of truth.

use std::path::{Path, PathBuf};

use cli2_evidence::Journal;
use rusqlite::{params, Connection, OptionalExtension};

use super::{derive_range, Workflow, BUILD_VERSION};
use crate::engine::EngineError;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS workflows (
    id TEXT PRIMARY KEY, recorded_at TEXT NOT NULL, request_id TEXT NOT NULL, json TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS workflows_request ON workflows(request_id);
"#;

pub struct WorkflowIndex {
    conn: Connection,
    path: PathBuf,
}

/// Where an index stands relative to the journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexStatus {
    pub initialized: bool,
    pub results_watermark: i64,
    pub batch_watermark: i64,
    pub journal_results: i64,
    pub journal_batches: i64,
    /// Executions recorded after what the index has seen.
    pub results_behind: i64,
    pub batches_behind: bool,
}

/// What `refresh` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Refresh {
    Current,
    Appended { workflows: usize },
    Rebuilt { workflows: usize },
}

/// Whether what is being shown is established to be current.
///
/// The freshness contract: explicit history commands always bring the index
/// up to date (`current`, `updated_incrementally` or `rebuilt`). An ordinary
/// request may only do bounded work; if the index is further behind than that
/// budget, or cannot be refreshed, the state is `behind` / `unavailable` and
/// no historical match is offered as current.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Freshness {
    Current,
    UpdatedIncrementally {
        workflows: usize,
    },
    Rebuilt {
        workflows: usize,
    },
    Behind {
        executions_behind: i64,
        budget: usize,
    },
    Unavailable {
        reason: String,
    },
}

impl Freshness {
    pub fn is_current(&self) -> bool {
        matches!(
            self,
            Self::Current | Self::UpdatedIncrementally { .. } | Self::Rebuilt { .. }
        )
    }
}

/// What happened while making the index current.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexReport {
    pub freshness: Freshness,
    pub rebuilt: bool,
    /// The previous file was unusable and was moved here.
    pub recovered_from: Option<String>,
    pub workflows: usize,
}

impl Default for IndexReport {
    fn default() -> Self {
        Self {
            freshness: Freshness::Current,
            rebuilt: false,
            recovered_from: None,
            workflows: 0,
        }
    }
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

    /// Where this index stands relative to the journal. Cheap: a few indexed
    /// lookups, no scan of history.
    pub fn status(&self, journal: &Journal) -> Result<IndexStatus, EngineError> {
        let built_by_this_version =
            self.meta("build_version").as_deref() == Some(&BUILD_VERSION.to_string());
        let r_wm: i64 = self
            .meta("results_watermark")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let b_wm: i64 = self
            .meta("batch_watermark")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let (r, b, behind) = journal.history_position(r_wm)?;
        let initialized = built_by_this_version && self.meta("results_watermark").is_some();
        Ok(IndexStatus {
            initialized,
            results_watermark: r_wm,
            batch_watermark: b_wm,
            journal_results: r,
            journal_batches: b,
            results_behind: if initialized { behind } else { r.max(behind) },
            batches_behind: initialized && b > b_wm || !initialized && b > 0,
        })
    }

    /// Bring the index up to date. Appends only what is new when it can (cost
    /// proportional to the new records); rebuilds from scratch when the file is
    /// new, was built by another derivation version, or `force_rebuild`.
    pub fn refresh(
        &mut self,
        journal: &Journal,
        force_rebuild: bool,
    ) -> Result<Refresh, EngineError> {
        let st = self.status(journal)?;
        if !force_rebuild && st.initialized && st.results_behind == 0 && !st.batches_behind {
            return Ok(Refresh::Current);
        }
        if force_rebuild || !st.initialized {
            let n = self.rebuild(journal)?;
            return Ok(Refresh::Rebuilt { workflows: n });
        }
        let added = self.append(journal, &st)?;
        Ok(Refresh::Appended { workflows: added })
    }

    fn append(&mut self, journal: &Journal, st: &IndexStatus) -> Result<usize, EngineError> {
        let (r, b, _) = journal.history_position(0)?;
        let derived = derive_range(journal, st.results_watermark, st.batch_watermark)?;
        let tx = self.conn.transaction().map_err(err)?;
        for w in &derived.workflows {
            tx.execute(
                "INSERT OR REPLACE INTO workflows(id, recorded_at, request_id, json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    w.id,
                    w.recorded_at,
                    w.steps.first().map(|s| s.request_id.as_str()).unwrap_or(""),
                    serde_json::to_string(w).expect("serializable")
                ],
            )
            .map_err(err)?;
        }
        // Executions indexed earlier that a newly recorded batch says were its steps.
        for (request_id, batch_wf) in &derived.members {
            let rows: Vec<(String, String)> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id, json FROM workflows WHERE request_id = ?1 AND id LIKE 'wf_e%'",
                    )
                    .map_err(err)?;
                let it = stmt
                    .query_map(params![request_id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(err)?;
                it.collect::<Result<_, _>>().map_err(err)?
            };
            for (id, json) in rows {
                let mut w: Workflow = serde_json::from_str(&json).map_err(|e| {
                    EngineError::Invalid(format!("workflow index row is damaged: {e}"))
                })?;
                if w.part_of_batch.as_deref() != Some(batch_wf) {
                    w.part_of_batch = Some(batch_wf.clone());
                    tx.execute(
                        "UPDATE workflows SET json = ?1 WHERE id = ?2",
                        params![serde_json::to_string(&w).expect("serializable"), id],
                    )
                    .map_err(err)?;
                }
            }
        }
        for (k, v) in [
            ("results_watermark", r.to_string()),
            ("batch_watermark", b.to_string()),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
                params![k, v],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(derived.workflows.len())
    }

    /// Replace the contents with a fresh derivation from the journal.
    pub fn rebuild(&mut self, journal: &Journal) -> Result<usize, EngineError> {
        let (r, b, _) = journal.history_position(0)?;
        let derived = derive_range(journal, 0, 0)?;
        let tx = self.conn.transaction().map_err(err)?;
        tx.execute("DELETE FROM workflows", []).map_err(err)?;
        tx.execute("DELETE FROM meta", []).map_err(err)?;
        for w in &derived.workflows {
            tx.execute(
                "INSERT INTO workflows(id, recorded_at, request_id, json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    w.id,
                    w.recorded_at,
                    w.steps.first().map(|s| s.request_id.as_str()).unwrap_or(""),
                    serde_json::to_string(w).expect("serializable")
                ],
            )
            .map_err(err)?;
        }
        for (k, v) in [
            ("build_version", BUILD_VERSION.to_string()),
            ("results_watermark", r.to_string()),
            ("batch_watermark", b.to_string()),
        ] {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)",
                params![k, v],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(derived.workflows.len())
    }

    /// All workflows in index order. A row that does not parse means the file
    /// is damaged; the caller recovers by rebuilding.
    pub fn list(&self) -> Result<Vec<Workflow>, EngineError> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM workflows ORDER BY recorded_at, id")
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
            .prepare("SELECT id, json FROM workflows ORDER BY recorded_at, id")
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

//! Append-only evidence journal with a hash chain and content-addressed blobs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cli2_core::{sha256_hex, ApprovalRecord, ExecutionResult, Plan, RecordedRequest};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const JOURNAL_SCHEMA_VERSION: &str = "cli2.journal.v1";
pub const TASK_SCHEMA_VERSION: &str = "cli2.task.v1";

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("evidence journal error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("evidence journal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("evidence journal is damaged: {0}")]
    Damaged(String),
    #[error("no recorded task, request or plan with ID {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct Event {
    pub seq: i64,
    pub request_id: String,
    pub recorded_at: String,
    pub kind: String,
    pub data: serde_json::Value,
    pub prev_hash: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub schema_version: String,
    pub request: RecordedRequest,
    pub plans: Vec<Plan>,
    pub approvals: Vec<ApprovalRecord>,
    pub results: Vec<ExecutionResult>,
    pub events: Vec<Event>,
    /// Whether the journal hash chain verified when this record was read.
    pub journal_integrity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub successes: u32,
    pub failures: u32,
    /// Fingerprints for which the capability has been observed executing.
    pub fingerprints: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub request_id: String,
    pub received_at: String,
    pub intent: String,
    pub status: Option<String>,
}

pub struct Journal {
    conn: Connection,
    blobs: PathBuf,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS requests (
    request_id TEXT PRIMARY KEY, received_at TEXT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS plans (
    plan_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, digest TEXT NOT NULL,
    created_at TEXT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS approvals (
    approval_id TEXT PRIMARY KEY, plan_id TEXT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS results (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL, plan_id TEXT,
    status TEXT NOT NULL, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS observations (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, capability_id TEXT NOT NULL,
    capability_fingerprint TEXT NOT NULL, request_id TEXT NOT NULL, plan_id TEXT NOT NULL,
    succeeded INTEGER NOT NULL, recorded_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL, recorded_at TEXT NOT NULL,
    kind TEXT NOT NULL, json TEXT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_request ON events(request_id);
CREATE INDEX IF NOT EXISTS plans_request ON plans(request_id);
"#;

const GENESIS: &str = "genesis";

impl Journal {
    pub fn open(dir: &Path) -> Result<Self, JournalError> {
        std::fs::create_dir_all(dir)?;
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs)?;
        let conn = Connection::open(dir.join("journal.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![JOURNAL_SCHEMA_VERSION],
        )?;
        Ok(Self { conn, blobs })
    }

    /// Store output bytes; returns `evidence:sha256:<hex>`.
    pub fn store_blob(&self, bytes: &[u8]) -> Result<String, JournalError> {
        let hash = sha256_hex(bytes);
        let path = self.blobs.join(&hash);
        if !path.exists() {
            let tmp = self.blobs.join(format!(".{hash}.tmp"));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(format!("evidence:sha256:{hash}"))
    }

    /// Every recorded result, oldest first (the order is the journal's own).
    pub fn all_results(&self) -> Result<Vec<(i64, ExecutionResult)>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, json FROM results ORDER BY seq ASC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            out.push((seq, from_json(&json)?));
        }
        Ok(out)
    }

    /// Results recorded after `seq`, oldest first.
    pub fn results_since(&self, seq: i64) -> Result<Vec<(i64, ExecutionResult)>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, json FROM results WHERE seq > ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![seq], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (s, json) = row?;
            out.push((s, from_json(&json)?));
        }
        Ok(out)
    }

    /// Batch records (`batch_recorded` events) after `seq`, oldest first, as
    /// `(seq, batch_id, data)`. Batch IDs start with `batch_`, so the
    /// existing `request_id` index bounds the scan.
    pub fn batch_events_since(
        &self,
        seq: i64,
    ) -> Result<Vec<(i64, String, serde_json::Value)>, JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, request_id, json FROM events
             WHERE request_id >= 'batch_' AND request_id < 'batch`' AND kind = 'batch_recorded' AND seq > ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![seq], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (s, req, json) = row?;
            out.push((
                s,
                req,
                serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
            ));
        }
        Ok(out)
    }

    /// `(highest result seq, highest batch-record seq, results after `since`)`:
    /// how much history a derived index has not seen. Cheap: no table scans.
    pub fn history_position(&self, since_results: i64) -> Result<(i64, i64, i64), JournalError> {
        let r: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM results", [], |r| {
                r.get(0)
            })?;
        let b: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE request_id >= 'batch_' AND request_id < 'batch`' AND kind = 'batch_recorded'",
            [],
            |r| r.get(0),
        )?;
        let behind: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM results WHERE seq > ?1",
            params![since_results],
            |r| r.get(0),
        )?;
        Ok((r, b, behind))
    }

    /// Events of one kind, oldest first, as `(seq, request_id, data)`.
    pub fn events_of_kind(
        &self,
        kind: &str,
    ) -> Result<Vec<(i64, String, serde_json::Value)>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, request_id, json FROM events WHERE kind = ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![kind], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, req, json) = row?;
            out.push((
                seq,
                req,
                serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
            ));
        }
        Ok(out)
    }

    /// `(highest result seq, highest event seq)`: how much history exists.
    /// A derived index records this to know whether it is current.
    pub fn watermark(&self) -> Result<(i64, i64), JournalError> {
        let r: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM results", [], |r| {
                r.get(0)
            })?;
        let e: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
        Ok((r, e))
    }

    /// The data of the newest event of `kind` recorded under `request_id`.
    pub fn latest_event_data(
        &self,
        request_id: &str,
        kind: &str,
    ) -> Result<Option<serde_json::Value>, JournalError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM events WHERE request_id = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT 1",
                params![request_id, kind],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// Whether a blob is still present (it may have been pruned).
    pub fn blob_exists(&self, reference: &str) -> bool {
        reference
            .strip_prefix("evidence:sha256:")
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .is_some_and(|h| self.blobs.join(h).is_file())
    }

    /// Remove a blob's content. The journal's records and hash chain are
    /// untouched; the reference simply stops resolving.
    pub fn delete_blob(&self, reference: &str) -> Result<bool, JournalError> {
        let Some(h) = reference
            .strip_prefix("evidence:sha256:")
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        else {
            return Ok(false);
        };
        match std::fs::remove_file(self.blobs.join(h)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Recorded results, newest first, with their stable sequence numbers
    /// (the execution IDs: `exec_<seq>`; sequence numbers are never reused).
    pub fn result_rows(&self, limit: usize) -> Result<Vec<(i64, ExecutionResult)>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, json FROM results ORDER BY seq DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            out.push((seq, from_json(&json)?));
        }
        Ok(out)
    }

    pub fn result_row(&self, seq: i64) -> Result<ExecutionResult, JournalError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM results WHERE seq = ?1",
                params![seq],
                |r| r.get(0),
            )
            .optional()?;
        from_json(&json.ok_or_else(|| JournalError::NotFound(format!("exec_{seq}")))?)
    }

    /// The newest result recorded for a request.
    pub fn latest_result_seq(&self, request_id: &str) -> Result<Option<i64>, JournalError> {
        Ok(self
            .conn
            .query_row(
                "SELECT seq FROM results WHERE request_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![request_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Whether any event other than `result_recorded` events, and other than
    /// events belonging to `except_requests`, mentions the blob (for example a
    /// captured `--help` that happens to produce the same bytes).
    pub fn blob_mentioned_outside(
        &self,
        reference: &str,
        except_requests: &[String],
    ) -> Result<bool, JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id FROM events WHERE kind <> 'result_recorded' AND instr(json, ?1) > 0",
        )?;
        let owners = stmt
            .query_map(params![reference], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(owners.iter().any(|o| !except_requests.contains(o)))
    }

    /// Results (by seq) whose stored JSON mentions `reference`, other than `except`.
    pub fn results_mentioning(
        &self,
        reference: &str,
        except: &[i64],
    ) -> Result<bool, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM results WHERE instr(json, ?1) > 0")?;
        let seqs = stmt
            .query_map(params![reference], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(seqs.iter().any(|s| !except.contains(s)))
    }

    pub fn read_blob(&self, reference: &str) -> Result<Vec<u8>, JournalError> {
        let hash = reference
            .strip_prefix("evidence:sha256:")
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| JournalError::NotFound(reference.to_string()))?;
        let bytes = std::fs::read(self.blobs.join(hash))
            .map_err(|_| JournalError::NotFound(reference.to_string()))?;
        if sha256_hex(&bytes) != hash {
            return Err(JournalError::Damaged(format!(
                "blob {hash} does not match its hash"
            )));
        }
        Ok(bytes)
    }

    pub fn record_request(&self, req: &RecordedRequest) -> Result<(), JournalError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO requests(request_id, received_at, json) VALUES (?1, ?2, ?3)",
            params![req.request_id, req.received_at, to_json(req)],
        )?;
        self.event(&req.request_id, "request_received", req)?;
        Ok(())
    }

    pub fn record_plan(&self, plan: &Plan) -> Result<(), JournalError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO plans(plan_id, request_id, digest, created_at, json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![plan.plan_id, plan.request_id, plan.digest, plan.created_at, to_json(plan)],
        )?;
        self.event(&plan.request_id, "plan_created", plan)?;
        Ok(())
    }

    pub fn record_approval(
        &self,
        request_id: &str,
        a: &ApprovalRecord,
    ) -> Result<(), JournalError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO approvals(approval_id, plan_id, json) VALUES (?1, ?2, ?3)",
            params![a.approval_id, a.plan_id, to_json(a)],
        )?;
        self.event(request_id, "approval_decision", a)?;
        Ok(())
    }

    pub fn record_result(
        &self,
        result: &ExecutionResult,
        plan: Option<&Plan>,
    ) -> Result<(), JournalError> {
        // The embedded plan is already recorded separately.
        let mut stored = result.clone();
        stored.plan = None;
        self.conn.execute(
            "INSERT INTO results(request_id, plan_id, status, json) VALUES (?1, ?2, ?3, ?4)",
            params![
                stored.request_id,
                stored.plan_id,
                stored.status.as_str(),
                to_json(&stored)
            ],
        )?;
        if let Some(plan) = plan {
            let now = cli2_core::now_rfc3339();
            for step in &stored.steps {
                if step.termination == cli2_core::Termination::NotStarted {
                    continue;
                }
                let fp = plan
                    .steps
                    .get(step.index)
                    .map(|s| s.capability_fingerprint.clone())
                    .unwrap_or_default();
                self.conn.execute(
                    "INSERT INTO observations(capability_id, capability_fingerprint, request_id, plan_id, succeeded, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![step.capability_id, fp, stored.request_id, plan.plan_id, step.process_succeeded() as i64, now],
                )?;
            }
        }
        self.event(&stored.request_id, "result_recorded", &stored)?;
        Ok(())
    }

    /// Append an event to the hash chain.
    pub fn event<T: Serialize>(
        &self,
        request_id: &str,
        kind: &str,
        data: &T,
    ) -> Result<(), JournalError> {
        let prev: String = self
            .conn
            .query_row(
                "SELECT hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| GENESIS.to_string());
        let now = cli2_core::now_rfc3339();
        let json = to_json(data);
        let hash = event_hash(&prev, request_id, &now, kind, &json);
        self.conn.execute(
            "INSERT INTO events(request_id, recorded_at, kind, json, prev_hash, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![request_id, now, kind, json, prev, hash],
        )?;
        Ok(())
    }

    /// Verify the whole event hash chain.
    pub fn verify_chain(&self) -> Result<(), JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, request_id, recorded_at, kind, json, prev_hash, hash FROM events ORDER BY seq",
        )?;
        let mut rows = stmt.query([])?;
        let mut prev = GENESIS.to_string();
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let (rid, at, kind, json, p, h): (String, String, String, String, String, String) = (
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            );
            if p != prev || event_hash(&p, &rid, &at, &kind, &json) != h {
                return Err(JournalError::Damaged(format!(
                    "event {seq} breaks the hash chain"
                )));
            }
            prev = h;
        }
        Ok(())
    }

    pub fn plan(&self, plan_id: &str) -> Result<Plan, JournalError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM plans WHERE plan_id = ?1",
                params![plan_id],
                |r| r.get(0),
            )
            .optional()?;
        let json = json.ok_or_else(|| JournalError::NotFound(plan_id.to_string()))?;
        from_json(&json)
    }

    pub fn request(&self, request_id: &str) -> Result<RecordedRequest, JournalError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM requests WHERE request_id = ?1",
                params![request_id],
                |r| r.get(0),
            )
            .optional()?;
        from_json(&json.ok_or_else(|| JournalError::NotFound(request_id.to_string()))?)
    }

    /// Look up everything recorded for a request ID or plan ID.
    pub fn task(&self, id: &str) -> Result<TaskRecord, JournalError> {
        let request_id = if id.starts_with("plan_") {
            self.conn
                .query_row(
                    "SELECT request_id FROM plans WHERE plan_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| JournalError::NotFound(id.to_string()))?
        } else {
            id.to_string()
        };
        let request = self.request(&request_id)?;
        let plans: Vec<Plan> = self.json_rows(
            "SELECT json FROM plans WHERE request_id = ?1 ORDER BY created_at",
            &request_id,
        )?;
        let approvals: Vec<ApprovalRecord> = self.json_rows(
            "SELECT a.json FROM approvals a JOIN plans p ON a.plan_id = p.plan_id WHERE p.request_id = ?1",
            &request_id,
        )?;
        let results: Vec<ExecutionResult> = self.json_rows(
            "SELECT json FROM results WHERE request_id = ?1 ORDER BY seq",
            &request_id,
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT seq, request_id, recorded_at, kind, json, prev_hash, hash FROM events WHERE request_id = ?1 ORDER BY seq",
        )?;
        let events = stmt
            .query_map(params![request_id], |r| {
                let json: String = r.get(4)?;
                Ok(Event {
                    seq: r.get(0)?,
                    request_id: r.get(1)?,
                    recorded_at: r.get(2)?,
                    kind: r.get(3)?,
                    data: serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json)),
                    prev_hash: r.get(5)?,
                    hash: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let journal_integrity = match self.verify_chain() {
            Ok(()) => "verified".to_string(),
            Err(e) => format!("failed: {e}"),
        };
        Ok(TaskRecord {
            schema_version: TASK_SCHEMA_VERSION.into(),
            request,
            plans,
            approvals,
            results,
            events,
            journal_integrity,
        })
    }

    pub fn recent_tasks(&self, limit: usize) -> Result<Vec<TaskSummary>, JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.request_id, r.received_at, r.json,
                    (SELECT status FROM results x WHERE x.request_id = r.request_id ORDER BY seq DESC LIMIT 1)
             FROM requests r ORDER BY r.received_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let json: String = r.get(2)?;
            let intent = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v.get("intent").and_then(|i| i.as_str()).map(str::to_string))
                .unwrap_or_default();
            Ok(TaskSummary {
                request_id: r.get(0)?,
                received_at: r.get(1)?,
                intent,
                status: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Execution history per capability: used to improve ranking and to mark
    /// capabilities as observed — never as proof of current behavior.
    pub fn observations(&self) -> Result<BTreeMap<String, Observation>, JournalError> {
        let mut stmt = self
            .conn
            .prepare("SELECT capability_id, capability_fingerprint, succeeded FROM observations")?;
        let mut rows = stmt.query([])?;
        let mut out: BTreeMap<String, Observation> = BTreeMap::new();
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let fp: String = r.get(1)?;
            let ok: i64 = r.get(2)?;
            let o = out.entry(id).or_default();
            if ok != 0 {
                o.successes += 1;
            } else {
                o.failures += 1;
            }
            if !o.fingerprints.contains(&fp) {
                o.fingerprints.push(fp);
            }
        }
        Ok(out)
    }

    fn json_rows<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        arg: &str,
    ) -> Result<Vec<T>, JournalError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![arg], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(from_json(&json?)?);
        }
        Ok(out)
    }

    #[doc(hidden)]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn event_hash(prev: &str, request_id: &str, at: &str, kind: &str, json: &str) -> String {
    sha256_hex(format!("{prev}\n{request_id}\n{at}\n{kind}\n{json}").as_bytes())
}

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("journal record serializes")
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, JournalError> {
    serde_json::from_str(s).map_err(|e| JournalError::Damaged(format!("unreadable record: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_and_chain() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path()).unwrap();
        let r = j.store_blob(b"hello").unwrap();
        assert_eq!(j.read_blob(&r).unwrap(), b"hello");
        j.event("req_1", "a", &serde_json::json!({"x": 1})).unwrap();
        j.event("req_1", "b", &serde_json::json!({"x": 2})).unwrap();
        j.verify_chain().unwrap();
        j.conn
            .execute("UPDATE events SET json = '{\"x\":3}' WHERE kind = 'a'", [])
            .unwrap();
        assert!(j.verify_chain().is_err());
    }
}

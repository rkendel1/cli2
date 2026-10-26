//! Persistent, provenance-aware capability index backed by SQLite.
//!
//! The index is a rebuildable cache of discovery facts. It is integrity-checked
//! on open; corrupt state is reported, never silently trusted, and can be
//! rebuilt with [`Index::rebuild`]. Execution evidence lives elsewhere (see
//! `cli2-evidence`) so rebuilding the index never destroys evidence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cli2_core::{sha256_hex, Capability};
use rusqlite::{params, Connection, OptionalExtension};

pub const INDEX_SCHEMA_VERSION: &str = "cli2.index.v1";
pub const ENV_SCOPE: &str = "env";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("capability index at {path} is corrupt: {detail}")]
    Corrupt { path: String, detail: String },
    #[error("capability index error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("capability index I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ChangedCapability {
    pub id: String,
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    /// Enrichments discarded because they were bound to the old fingerprint.
    pub invalidated: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReconcileReport {
    pub scope: String,
    pub added: Vec<String>,
    pub changed: Vec<ChangedCapability>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

impl ReconcileReport {
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScopeInfo {
    pub scope: String,
    pub fingerprint: String,
    pub scanned_at: String,
    pub capabilities: usize,
}

pub struct Index {
    conn: Connection,
    path: PathBuf,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS scopes (
    scope       TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    scanned_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS capabilities (
    scope       TEXT NOT NULL,
    id          TEXT NOT NULL,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    json        TEXT NOT NULL,
    json_sha256 TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (scope, id)
);
CREATE INDEX IF NOT EXISTS capabilities_name ON capabilities(name);
"#;

impl Index {
    /// Open (or create) the index and verify its integrity.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existed = path.exists() && std::fs::metadata(path)?.len() > 0;
        let corrupt = |detail: String| IndexError::Corrupt {
            path: path.display().to_string(),
            detail,
        };
        let conn = Connection::open(path).map_err(|e| corrupt(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let index = Index {
            conn,
            path: path.to_path_buf(),
        };
        if existed {
            index.verify().map_err(|e| match e {
                IndexError::Sqlite(err) => corrupt(err.to_string()),
                other => other,
            })?;
        } else {
            index.conn.execute_batch(SCHEMA)?;
            index.conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
                params![INDEX_SCHEMA_VERSION],
            )?;
        }
        Ok(index)
    }

    /// Move a (corrupt) index aside and create a fresh one. Returns the new
    /// index and the path the old file was moved to, if any.
    pub fn rebuild(path: &Path) -> Result<(Self, Option<PathBuf>), IndexError> {
        let mut moved = None;
        if path.exists() {
            let stamp = cli2_core::now_rfc3339().replace([':', '.'], "-");
            let aside = path.with_extension(format!("db.corrupt-{stamp}"));
            std::fs::rename(path, &aside)?;
            moved = Some(aside);
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if side.exists() {
                std::fs::remove_file(side)?;
            }
        }
        Ok((Self::open(path)?, moved))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn verify(&self) -> Result<(), IndexError> {
        let corrupt = |detail: String| IndexError::Corrupt {
            path: self.path.display().to_string(),
            detail,
        };
        let check: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(corrupt(format!("sqlite quick_check: {check}")));
        }
        let version: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| corrupt(format!("missing metadata: {e}")))?;
        match version.as_deref() {
            Some(INDEX_SCHEMA_VERSION) => {}
            Some(other) => return Err(corrupt(format!("unsupported index version {other}"))),
            None => return Err(corrupt("missing schema_version".into())),
        }
        // Every record must hash to its stored checksum and deserialize.
        let mut stmt = self
            .conn
            .prepare("SELECT scope, id, fingerprint, json, json_sha256 FROM capabilities")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (scope, id, fp, json, sha) = row?;
            if sha256_hex(json.as_bytes()) != sha {
                return Err(corrupt(format!(
                    "checksum mismatch for {id} in scope {scope}"
                )));
            }
            let cap: Capability = serde_json::from_str(&json)
                .map_err(|e| corrupt(format!("unreadable record {id}: {e}")))?;
            if cap.id != id || cap.fingerprint != fp {
                return Err(corrupt(format!("record {id} does not match its row")));
            }
        }
        Ok(())
    }

    pub fn scope_info(&self, scope: &str) -> Result<Option<ScopeInfo>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.scope, s.fingerprint, s.scanned_at,
                        (SELECT COUNT(*) FROM capabilities c WHERE c.scope = s.scope)
                 FROM scopes s WHERE s.scope = ?1",
                params![scope],
                |r| {
                    Ok(ScopeInfo {
                        scope: r.get(0)?,
                        fingerprint: r.get(1)?,
                        scanned_at: r.get(2)?,
                        capabilities: r.get::<_, i64>(3)? as usize,
                    })
                },
            )
            .optional()?)
    }

    pub fn scopes(&self) -> Result<Vec<ScopeInfo>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.scope, s.fingerprint, s.scanned_at,
                    (SELECT COUNT(*) FROM capabilities c WHERE c.scope = s.scope)
             FROM scopes s ORDER BY s.scope",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ScopeInfo {
                scope: r.get(0)?,
                fingerprint: r.get(1)?,
                scanned_at: r.get(2)?,
                capabilities: r.get::<_, i64>(3)? as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Reconcile one scope with a fresh discovery result.
    ///
    /// * New IDs are added.
    /// * IDs whose fingerprint is unchanged keep their stored enrichments
    ///   (help capture, user characterization); location details are refreshed.
    /// * IDs whose fingerprint changed are replaced by the fresh record and any
    ///   enrichment bound to the old fingerprint is discarded and reported.
    /// * IDs no longer discovered are removed.
    pub fn reconcile_scope(
        &mut self,
        scope: &str,
        scope_fingerprint: &str,
        fresh: Vec<Capability>,
        now: &str,
    ) -> Result<ReconcileReport, IndexError> {
        let existing: BTreeMap<String, Capability> = self
            .list_scope(scope)?
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let mut report = ReconcileReport {
            scope: scope.to_string(),
            ..Default::default()
        };
        let tx = self.conn.transaction()?;
        let mut fresh_ids = std::collections::BTreeSet::new();
        for mut cap in fresh {
            fresh_ids.insert(cap.id.clone());
            match existing.get(&cap.id) {
                None => report.added.push(cap.id.clone()),
                Some(old) if old.fingerprint == cap.fingerprint => {
                    let mut kept = old.clone();
                    kept.location = cap.location.clone();
                    kept.source = cap.source.clone();
                    if kept == *old {
                        report.unchanged += 1;
                        continue;
                    }
                    report.unchanged += 1;
                    cap = kept;
                }
                Some(old) => {
                    let mut invalidated = Vec::new();
                    if old.characterization.is_some() {
                        invalidated.push("user characterization".to_string());
                    }
                    if old.help.is_some() {
                        invalidated.push("captured help and inferred interface".to_string());
                    }
                    if old.location.path != cap.location.path {
                        invalidated.push(format!("location (was {})", old.location.path));
                    }
                    invalidated.push("task metadata".to_string());
                    report.changed.push(ChangedCapability {
                        id: cap.id.clone(),
                        old_fingerprint: old.fingerprint.clone(),
                        new_fingerprint: cap.fingerprint.clone(),
                        invalidated,
                    });
                }
            }
            upsert(&tx, scope, &cap, now)?;
        }
        for id in existing.keys() {
            if !fresh_ids.contains(id) {
                tx.execute(
                    "DELETE FROM capabilities WHERE scope = ?1 AND id = ?2",
                    params![scope, id],
                )?;
                report.removed.push(id.clone());
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO scopes(scope, fingerprint, scanned_at) VALUES (?1, ?2, ?3)",
            params![scope, scope_fingerprint, now],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// Store an updated record (e.g. after help capture or characterization).
    pub fn put(&mut self, cap: &Capability) -> Result<(), IndexError> {
        let now = cli2_core::now_rfc3339();
        let tx = self.conn.transaction()?;
        upsert(&tx, &cap.scope(), cap, &now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_scope(&self, scope: &str) -> Result<Vec<Capability>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM capabilities WHERE scope = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![scope], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(self.decode(&json?)?);
        }
        Ok(out)
    }

    /// Capabilities visible from a working context: the environment plus the
    /// given project scope, if any.
    pub fn visible(&self, project_scope: Option<&str>) -> Result<Vec<Capability>, IndexError> {
        let mut caps = self.list_scope(ENV_SCOPE)?;
        if let Some(scope) = project_scope {
            caps.extend(self.list_scope(scope)?);
        }
        Ok(caps)
    }

    /// Look up a capability by logical ID, preferring the project scope.
    pub fn get(
        &self,
        id: &str,
        project_scope: Option<&str>,
    ) -> Result<Option<Capability>, IndexError> {
        for scope in project_scope.into_iter().chain([ENV_SCOPE]) {
            let json: Option<String> = self
                .conn
                .query_row(
                    "SELECT json FROM capabilities WHERE scope = ?1 AND id = ?2",
                    params![scope, id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(json) = json {
                return Ok(Some(self.decode(&json)?));
            }
        }
        Ok(None)
    }

    fn decode(&self, json: &str) -> Result<Capability, IndexError> {
        serde_json::from_str(json).map_err(|e| IndexError::Corrupt {
            path: self.path.display().to_string(),
            detail: format!("unreadable capability record: {e}"),
        })
    }
}

fn upsert(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    cap: &Capability,
    now: &str,
) -> Result<(), IndexError> {
    let json = serde_json::to_string(cap).expect("capability serializes");
    let kind = serde_json::to_value(cap.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    tx.execute(
        "INSERT OR REPLACE INTO capabilities(scope, id, kind, name, fingerprint, json, json_sha256, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![scope, cap.id, kind, cap.name, cap.fingerprint, json, sha256_hex(json.as_bytes()), now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cli2_core::*;

    fn cap(id: &str, fp: &str) -> Capability {
        Capability {
            schema_version: CAPABILITY_SCHEMA_VERSION.into(),
            id: id.into(),
            kind: CapabilityKind::Executable,
            name: id.trim_start_matches("executable:").into(),
            location: Location {
                path: format!("/bin/{id}"),
                project_root: None,
                shadows: vec![],
            },
            source: DiscoverySource {
                kind: DiscoverySourceKind::Path,
                discovered_at: "t".into(),
            },
            fingerprint: fp.into(),
            enrichment: EnrichmentState::Discovered,
            purpose: None,
            interface: Interface {
                executable: "x".into(),
                arguments: vec![],
                parameters: vec![],
                known_flags: vec![],
                completeness: InterfaceCompleteness::Unknown,
                source: "path".into(),
            },
            effects: Effects::unknown("n"),
            postconditions: vec![],
            help: None,
            characterization: None,
        }
    }

    #[test]
    fn reconcile_add_change_remove_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut idx = Index::open(&path).unwrap();
            let r = idx
                .reconcile_scope(
                    ENV_SCOPE,
                    "s1",
                    vec![cap("executable:a", "1"), cap("executable:b", "1")],
                    "t1",
                )
                .unwrap();
            assert_eq!(r.added.len(), 2);
            let mut a = idx.get("executable:a", None).unwrap().unwrap();
            a.characterization = Some(Characterization {
                fingerprint: "1".into(),
                declared_at: "t".into(),
                effects: Some(vec![EffectClass::ReadOnly]),
                purpose: None,
            });
            idx.put(&a).unwrap();
        }
        let mut idx = Index::open(&path).unwrap();
        // unchanged fingerprint keeps enrichment
        let r = idx
            .reconcile_scope(
                ENV_SCOPE,
                "s2",
                vec![cap("executable:a", "1"), cap("executable:c", "1")],
                "t2",
            )
            .unwrap();
        assert_eq!(r.added, vec!["executable:c"]);
        assert_eq!(r.removed, vec!["executable:b"]);
        assert!(idx
            .get("executable:a", None)
            .unwrap()
            .unwrap()
            .characterization
            .is_some());
        // changed fingerprint invalidates it
        let r = idx
            .reconcile_scope(
                ENV_SCOPE,
                "s3",
                vec![cap("executable:a", "2"), cap("executable:c", "1")],
                "t3",
            )
            .unwrap();
        assert_eq!(r.changed.len(), 1);
        assert!(r.changed[0]
            .invalidated
            .iter()
            .any(|s| s.contains("characterization")));
        assert!(idx
            .get("executable:a", None)
            .unwrap()
            .unwrap()
            .characterization
            .is_none());
        assert_eq!(
            idx.scope_info(ENV_SCOPE).unwrap().unwrap().fingerprint,
            "s3"
        );
    }

    #[test]
    fn corruption_is_detected_and_rebuild_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut idx = Index::open(&path).unwrap();
            idx.reconcile_scope(ENV_SCOPE, "s", vec![cap("executable:a", "1")], "t")
                .unwrap();
            // Tamper with a record behind the index's back.
            idx.conn
                .execute(
                    "UPDATE capabilities SET json = replace(json, '/bin/', '/evil/')",
                    [],
                )
                .unwrap();
        }
        assert!(matches!(
            Index::open(&path),
            Err(IndexError::Corrupt { .. })
        ));

        std::fs::write(
            &path,
            b"this is not a database at all, just garbage bytes......",
        )
        .unwrap();
        assert!(matches!(
            Index::open(&path),
            Err(IndexError::Corrupt { .. })
        ));

        let (idx, moved) = Index::rebuild(&path).unwrap();
        assert!(moved.unwrap().exists());
        assert!(idx.list_scope(ENV_SCOPE).unwrap().is_empty());
    }
}

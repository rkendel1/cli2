//! `PATH` executable discovery.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use cli2_core::{
    Capability, CapabilityKind, DiscoverySource, DiscoverySourceKind, EnrichmentState, Interface,
    InterfaceCompleteness, Location, CAPABILITY_SCHEMA_VERSION,
};

use crate::effects::heuristic_effects_for_executable;

#[derive(Debug, Clone, Default)]
pub struct PathScan {
    pub capabilities: Vec<Capability>,
    /// The `PATH` directories that were read, in order.
    pub directories: Vec<String>,
    /// Non-fatal problems (unreadable directories, ...).
    pub warnings: Vec<String>,
}

/// Fingerprint an executable from file metadata (size, mtime, inode, mode).
///
/// Contents are not hashed: hashing every binary on `PATH` would make scans
/// slow. Any replacement, rebuild or edit of the file changes the metadata.
pub fn executable_fingerprint(path: &Path) -> std::io::Result<String> {
    let meta = fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "file:v1:size={}:mtime={}:ino={}:mode={:o}",
            meta.len(),
            mtime,
            meta.ino(),
            meta.mode()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("file:v1:size={}:mtime={}", meta.len(), mtime))
    }
}

pub fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

/// Discover executables on the given `PATH` value. The first match for a name
/// wins (as in a shell lookup); later matches are recorded as shadowed.
pub fn scan_path(path_var: &OsStr, now: &str) -> PathScan {
    let mut scan = PathScan::default();
    let mut found: BTreeMap<String, Capability> = BTreeMap::new();

    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        // Relative PATH entries depend on the caller's cwd; they cannot be
        // represented as a stable location, so they are skipped.
        if !dir.is_absolute() {
            scan.warnings
                .push(format!("skipped relative PATH entry {}", dir.display()));
            continue;
        }
        scan.directories.push(dir.display().to_string());
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                if dir.exists() {
                    scan.warnings
                        .push(format!("cannot read {}: {err}", dir.display()));
                }
                continue;
            }
        };
        let mut names: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                continue;
            }
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            let path_str = path.display().to_string();
            if let Some(existing) = found.get_mut(&name) {
                existing.location.shadows.push(path_str);
                continue;
            }
            let fingerprint = match executable_fingerprint(&path) {
                Ok(f) => f,
                Err(err) => {
                    scan.warnings
                        .push(format!("cannot fingerprint {path_str}: {err}"));
                    continue;
                }
            };
            found.insert(
                name.clone(),
                executable_capability(&name, &path_str, fingerprint, now),
            );
        }
    }
    scan.capabilities = found.into_values().collect();
    scan
}

pub fn executable_capability(name: &str, path: &str, fingerprint: String, now: &str) -> Capability {
    Capability {
        schema_version: CAPABILITY_SCHEMA_VERSION.into(),
        id: format!("executable:{name}"),
        kind: CapabilityKind::Executable,
        name: name.to_string(),
        location: Location {
            path: path.to_string(),
            project_root: None,
            shadows: vec![],
        },
        source: DiscoverySource {
            kind: DiscoverySourceKind::Path,
            discovered_at: now.to_string(),
        },
        fingerprint,
        enrichment: EnrichmentState::Discovered,
        purpose: None,
        interface: Interface {
            executable: name.to_string(),
            arguments: vec![],
            parameters: vec![],
            known_flags: vec![],
            options: vec![],
            completeness: InterfaceCompleteness::Unknown,
            source: "path".into(),
            capture: None,
        },
        effects: heuristic_effects_for_executable(name),
        postconditions: vec![],
        help: None,
        characterization: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_exe(dir: &Path, name: &str) {
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn first_match_wins_and_shadows_are_recorded() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        make_exe(a.path(), "tool");
        make_exe(b.path(), "tool");
        make_exe(b.path(), "other");
        fs::write(b.path().join("notexec"), "x").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        let scan = scan_path(&path, "now");
        let ids: Vec<_> = scan.capabilities.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["executable:other", "executable:tool"]);
        let tool = &scan.capabilities[1];
        assert!(tool
            .location
            .path
            .starts_with(&a.path().display().to_string()));
        assert_eq!(tool.location.shadows.len(), 1);
    }
}

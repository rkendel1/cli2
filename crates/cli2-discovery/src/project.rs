//! Project manifest discovery: npm scripts, Make targets, Cargo package
//! metadata and `just` recipes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cli2_core::{
    sha256_hex, Capability, CapabilityKind, Check, DiscoverySource, DiscoverySourceKind,
    EnrichmentState, Interface, InterfaceCompleteness, Location, Parameter, Provenanced,
    CAPABILITY_SCHEMA_VERSION,
};

use crate::effects::heuristic_effects_for_task;

pub const MANIFEST_NAMES: &[&str] = &[
    "package.json",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "Cargo.toml",
    "justfile",
    "Justfile",
    ".justfile",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInfo {
    pub kind: DiscoverySourceKind,
    pub path: String,
    /// SHA-256 of the manifest content.
    pub content_hash: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectScan {
    pub root: String,
    pub capabilities: Vec<Capability>,
    pub manifests: Vec<ManifestInfo>,
    pub warnings: Vec<String>,
}

impl ProjectScan {
    /// Fingerprint of the whole project scope: changes when any manifest is
    /// added, removed, or edited.
    pub fn scope_fingerprint(&self) -> String {
        manifests_fingerprint(&self.manifests)
    }
}

pub fn manifests_fingerprint(manifests: &[ManifestInfo]) -> String {
    let mut parts: Vec<String> = manifests
        .iter()
        .map(|m| format!("{}={}", m.path, m.content_hash))
        .collect();
    parts.sort();
    format!("manifests:v1:{}", sha256_hex(parts.join("\n").as_bytes()))
}

/// Cheap freshness probe: hash the manifests currently present under `root`.
pub fn probe_manifests(root: &Path) -> Vec<ManifestInfo> {
    let mut out = Vec::new();
    for name in MANIFEST_NAMES {
        let path = root.join(name);
        if let Ok(bytes) = fs::read(&path) {
            if let Some(kind) = manifest_kind(name) {
                out.push(ManifestInfo {
                    kind,
                    path: path.display().to_string(),
                    content_hash: sha256_hex(&bytes),
                });
            }
        }
    }
    out
}

fn manifest_kind(name: &str) -> Option<DiscoverySourceKind> {
    Some(match name {
        "package.json" => DiscoverySourceKind::PackageManifest,
        "Makefile" | "makefile" | "GNUmakefile" => DiscoverySourceKind::Makefile,
        "Cargo.toml" => DiscoverySourceKind::CargoManifest,
        "justfile" | "Justfile" | ".justfile" => DiscoverySourceKind::Justfile,
        _ => return None,
    })
}

/// The nearest ancestor of `start` (inclusive) that contains a supported manifest.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| MANIFEST_NAMES.iter().any(|m| dir.join(m).is_file()))
        .map(Path::to_path_buf)
}

pub fn scan_project(root: &Path, now: &str) -> ProjectScan {
    let mut scan = ProjectScan {
        root: root.display().to_string(),
        ..Default::default()
    };
    scan.manifests = probe_manifests(root);
    let mut seen_make = false;
    let mut seen_just = false;
    for manifest in scan.manifests.clone() {
        let path = PathBuf::from(&manifest.path);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err) => {
                scan.warnings
                    .push(format!("cannot read {}: {err}", manifest.path));
                continue;
            }
        };
        let result = match manifest.kind {
            DiscoverySourceKind::PackageManifest => parse_package_json(&text),
            DiscoverySourceKind::Makefile if !seen_make => {
                seen_make = true;
                Ok(parse_makefile(&text))
            }
            DiscoverySourceKind::CargoManifest => parse_cargo_toml(&text),
            DiscoverySourceKind::Justfile if !seen_just => {
                seen_just = true;
                Ok(parse_justfile(&text))
            }
            _ => continue,
        };
        match result {
            Ok(tasks) => {
                for task in tasks {
                    scan.capabilities
                        .push(task.into_capability(&manifest, &scan.root, now));
                }
            }
            Err(err) => scan
                .warnings
                .push(format!("cannot parse {}: {err}", manifest.path)),
        }
    }
    scan.capabilities.sort_by(|a, b| a.id.cmp(&b.id));
    scan.capabilities.dedup_by(|a, b| a.id == b.id);
    scan
}

/// A task extracted from a manifest, before it becomes a capability.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTask {
    pub id: String,
    pub name: String,
    pub runner: String,
    pub arguments: Vec<String>,
    pub parameters: Vec<Parameter>,
    /// The task definition used for fingerprinting and effect heuristics.
    pub body: String,
    pub purpose: String,
    pub purpose_source: String,
    pub postconditions: Vec<Provenanced<Check>>,
    pub notes: Vec<String>,
}

impl RawTask {
    fn into_capability(self, manifest: &ManifestInfo, root: &str, now: &str) -> Capability {
        let fingerprint = format!(
            "task:v1:{}",
            sha256_hex(
                format!(
                    "{}\n{}\n{}\n{}",
                    manifest.path,
                    self.id,
                    self.arguments.join("\u{1f}"),
                    self.body
                )
                .as_bytes()
            )
        );
        let mut effects = heuristic_effects_for_task(&self.name, &self.body);
        effects.notes.extend(self.notes);
        let source_name = match manifest.kind {
            DiscoverySourceKind::PackageManifest => "package_manifest",
            DiscoverySourceKind::Makefile => "makefile",
            DiscoverySourceKind::CargoManifest => "cargo_manifest",
            DiscoverySourceKind::Justfile => "justfile",
            DiscoverySourceKind::Path => "path",
        };
        Capability {
            schema_version: CAPABILITY_SCHEMA_VERSION.into(),
            id: self.id,
            kind: CapabilityKind::ProjectTask,
            name: self.name,
            location: Location {
                path: manifest.path.clone(),
                project_root: Some(root.to_string()),
                shadows: vec![],
            },
            source: DiscoverySource {
                kind: manifest.kind,
                discovered_at: now.to_string(),
            },
            fingerprint,
            enrichment: EnrichmentState::Described,
            purpose: Some(Provenanced::new(self.purpose, self.purpose_source, 1.0)),
            interface: Interface {
                executable: self.runner,
                arguments: self.arguments,
                parameters: self.parameters,
                known_flags: vec![],
                options: vec![],
                completeness: InterfaceCompleteness::Declared,
                source: source_name.into(),
                capture: None,
            },
            effects,
            postconditions: self.postconditions,
            help: None,
            characterization: None,
        }
    }
}

pub fn parse_package_json(text: &str) -> Result<Vec<RawTask>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Some(scripts) = value.get("scripts") else {
        return Ok(vec![]);
    };
    let scripts = scripts
        .as_object()
        .ok_or_else(|| "\"scripts\" is not an object".to_string())?;
    let bodies: BTreeMap<&str, &str> = scripts
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
        .collect();
    let mut tasks = Vec::new();
    for (name, body) in &bodies {
        // npm runs pre<name>/post<name> automatically; their bodies are part of
        // what `npm run <name>` does.
        let mut full_body = body.to_string();
        let mut notes = Vec::new();
        for hook in [format!("pre{name}"), format!("post{name}")] {
            if let Some(hook_body) = bodies.get(hook.as_str()) {
                full_body.push('\n');
                full_body.push_str(hook_body);
                notes.push(format!("npm also runs the `{hook}` script"));
            }
        }
        tasks.push(RawTask {
            id: format!("project-script:npm:{name}"),
            name: name.to_string(),
            runner: "npm".into(),
            arguments: vec!["run".into(), name.to_string()],
            parameters: vec![],
            body: full_body,
            purpose: format!("npm script `{name}`: {body}"),
            purpose_source: "package_manifest".into(),
            postconditions: vec![],
            notes,
        });
    }
    Ok(tasks)
}

pub fn parse_makefile(text: &str) -> Vec<RawTask> {
    let mut tasks: Vec<RawTask> = Vec::new();
    let mut current: Option<usize> = None;
    let mut pending_comment: Option<String> = None;
    for line in text.lines() {
        if let Some(recipe) = line.strip_prefix('\t') {
            if let Some(i) = current {
                tasks[i].body.push('\n');
                tasks[i].body.push_str(recipe.trim());
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            pending_comment = None;
            continue;
        }
        if let Some(c) = trimmed.strip_prefix('#') {
            pending_comment = Some(c.trim_start_matches('#').trim().to_string());
            continue;
        }
        current = None;
        let Some(colon) = trimmed.find(':') else {
            pending_comment = None;
            continue;
        };
        let after = &trimmed[colon + 1..];
        if after.starts_with('=') || trimmed[..colon].contains('=') {
            pending_comment = None;
            continue; // variable assignment
        }
        let (deps, inline_doc) = match after.split_once("##") {
            Some((d, doc)) => (d, Some(doc.trim().to_string())),
            None => (after, None),
        };
        let deps = deps.split(';').next().unwrap_or("").trim();
        let doc = inline_doc.or(pending_comment.take());
        for target in trimmed[..colon].split_whitespace() {
            if target.starts_with('.')
                || target.contains('%')
                || target.contains('$')
                || !target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-./".contains(c))
            {
                continue;
            }
            if tasks.iter().any(|t| t.name == target) {
                continue;
            }
            let purpose = match &doc {
                Some(d) if !d.is_empty() => format!("make target `{target}`: {d}"),
                _ if !deps.is_empty() => format!("make target `{target}` (depends on {deps})"),
                _ => format!("make target `{target}`"),
            };
            tasks.push(RawTask {
                id: format!("project-task:make:{target}"),
                name: target.to_string(),
                runner: "make".into(),
                arguments: vec![target.to_string()],
                parameters: vec![],
                body: format!("deps: {deps}"),
                purpose,
                purpose_source: "makefile".into(),
                postconditions: vec![],
                notes: vec![],
            });
            current = Some(tasks.len() - 1);
        }
    }
    tasks
}

pub fn parse_cargo_toml(text: &str) -> Result<Vec<RawTask>, String> {
    let value: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let package = value.get("package").and_then(|p| p.as_table());
    let workspace = value.get("workspace").is_some();
    if package.is_none() && !workspace {
        return Ok(vec![]);
    }
    let pkg_name = package
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string);
    let what = match &pkg_name {
        Some(n) => format!("package `{n}`"),
        None => "the workspace".to_string(),
    };
    let target_check = Provenanced::new(
        Check::PathExists {
            path: "target".into(),
        },
        "cargo_manifest:build_output_directory",
        0.9,
    );
    let mut tasks = vec![
        (
            "build",
            format!("Build {what} with cargo"),
            vec![target_check.clone()],
        ),
        (
            "test",
            format!("Run the tests of {what} with cargo"),
            vec![],
        ),
        ("check", format!("Type-check {what} with cargo"), vec![]),
    ];
    if pkg_name.is_some() || value.get("bin").is_some() {
        tasks.push((
            "run",
            format!("Run the binary of {what} with cargo"),
            vec![],
        ));
    }
    Ok(tasks
        .into_iter()
        .map(|(sub, purpose, post)| RawTask {
            id: format!("project-task:cargo:{sub}"),
            name: sub.to_string(),
            runner: "cargo".into(),
            arguments: vec![sub.to_string()],
            parameters: vec![],
            body: format!("cargo {sub} ({})", pkg_name.clone().unwrap_or_default()),
            purpose,
            purpose_source: "cargo_manifest".into(),
            postconditions: post,
            notes: if sub == "test" || sub == "check" {
                vec!["cargo writes build artifacts under target/".into()]
            } else {
                vec![]
            },
        })
        .collect())
}

pub fn parse_justfile(text: &str) -> Vec<RawTask> {
    let mut tasks: Vec<RawTask> = Vec::new();
    let mut current: Option<usize> = None;
    let mut doc: Option<String> = None;
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(i) = current {
                tasks[i].body.push('\n');
                tasks[i].body.push_str(line.trim());
            }
            continue;
        }
        current = None;
        let t = line.trim();
        if t.is_empty() {
            doc = None;
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            if !c.starts_with('!') {
                doc = Some(c.trim().to_string());
            }
            continue;
        }
        if t.starts_with('[')
            || ["set ", "alias ", "export ", "import ", "mod "]
                .iter()
                .any(|p| t.starts_with(p))
            || t.contains(":=")
        {
            doc = None;
            continue;
        }
        let Some(colon) = t.find(':') else {
            doc = None;
            continue;
        };
        let header = t[..colon].trim_start_matches('@');
        let mut words = header.split_whitespace();
        let Some(name) = words.next() else { continue };
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let mut parameters = Vec::new();
        for p in words {
            let variadic = p.starts_with('+') || p.starts_with('*');
            let p = p.trim_start_matches(['+', '*', '$']);
            let (pname, has_default) = match p.split_once('=') {
                Some((n, _)) => (n, true),
                None => (p, false),
            };
            parameters.push(Parameter {
                name: pname.to_lowercase(),
                required: !has_default && !p.starts_with('*'),
                variadic,
                source: "justfile".into(),
                omitted_means: None,
            });
        }
        let purpose = match doc.take() {
            Some(d) if !d.is_empty() => format!("just recipe `{name}`: {d}"),
            _ => format!("just recipe `{name}`"),
        };
        tasks.push(RawTask {
            id: format!("project-task:just:{name}"),
            name: name.to_string(),
            runner: "just".into(),
            arguments: vec![name.to_string()],
            parameters,
            body: String::new(),
            purpose,
            purpose_source: "justfile".into(),
            postconditions: vec![],
            notes: vec![],
        });
        current = Some(tasks.len() - 1);
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_scripts_with_hooks() {
        let t = parse_package_json(
            r#"{"scripts":{"test":"node test.js","pretest":"node lint.js","publish":"node pub.js"}}"#,
        )
        .unwrap();
        let test = t.iter().find(|t| t.name == "test").unwrap();
        assert_eq!(test.arguments, vec!["run", "test"]);
        assert!(test.body.contains("lint.js"));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn makefile_targets() {
        let t = parse_makefile(
            ".PHONY: test build\nCC := gcc\nX = 1\n# Build everything\nbuild: deps\n\techo build\n\ntest: build ## Run tests\n\t./run-tests\n%.o: %.c\n\tcc $<\n",
        );
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test"]);
        assert!(t[0].purpose.contains("Build everything"));
        assert!(t[1].purpose.contains("Run tests"));
        assert!(t[1].body.contains("./run-tests"));
    }

    #[test]
    fn cargo_package() {
        let t = parse_cargo_toml("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "check", "run"]);
    }

    #[test]
    fn justfile_recipes() {
        let t = parse_justfile(
            "set shell := [\"bash\", \"-c\"]\nversion := \"1\"\n# Greet someone\ngreet name greeting='hi':\n    echo {{greeting}} {{name}}\n\n@lint:\n    echo lint\n",
        );
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].parameters.len(), 2);
        assert!(t[0].parameters[0].required);
        assert!(!t[0].parameters[1].required);
        assert!(t[0].purpose.contains("Greet someone"));
    }
}

//! Heuristic effect assessment.
//!
//! Heuristics are recorded with `certainty: heuristic` and low confidence. The
//! approval policy never treats a heuristic assessment as well characterized,
//! and an empty assessment is `unknown`, never read-only.

use cli2_core::{EffectCertainty, EffectClass, Effects};

/// Split a name or script body into lowercase word tokens.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn has_word(words: &[String], candidates: &[&str]) -> bool {
    words.iter().any(|w| {
        candidates
            .iter()
            .any(|c| w == c || w.split(['-', '_', ':']).any(|part| part == *c))
    })
}

fn has_phrase(text: &str, phrases: &[&str]) -> bool {
    let lower = text.to_lowercase();
    phrases.iter().any(|p| lower.contains(p))
}

/// Assess a project task from its name and definition body.
pub fn heuristic_effects_for_task(name: &str, body: &str) -> Effects {
    let name_words = words(name);
    let body_words = words(body);
    let all: Vec<String> = name_words
        .iter()
        .chain(body_words.iter())
        .cloned()
        .collect();
    let mut classes = Vec::new();
    let mut notes = Vec::new();

    if has_word(&body_words, &["sudo", "doas", "pkexec", "su"]) {
        classes.push(EffectClass::PrivilegeEscalation);
        notes.push("definition invokes a privilege-escalation command".into());
    }
    if has_word(&all, &["publish", "release"])
        || has_phrase(
            body,
            &["git push", "docker push", "twine upload", "gh release"],
        )
    {
        classes.push(EffectClass::Publishes);
        classes.push(EffectClass::Network);
        notes.push("name or definition suggests publishing to an external system".into());
    }
    if has_word(&all, &["deploy", "deployment"])
        || has_phrase(
            body,
            &[
                "kubectl apply",
                "terraform apply",
                "helm upgrade",
                "helm install",
            ],
        )
    {
        classes.push(EffectClass::Deploys);
        classes.push(EffectClass::Network);
        notes.push("name or definition suggests deployment".into());
    }
    if has_word(
        &body_words,
        &["curl", "wget", "ssh", "scp", "rsync", "fetch"],
    ) || has_phrase(
        body,
        &[
            "http://",
            "https://",
            "npm install",
            "npm i ",
            "npm ci",
            "pip install",
            "cargo install",
            "yarn install",
            "pnpm install",
        ],
    ) || has_word(&name_words, &["install", "download", "upload", "sync"])
    {
        classes.push(EffectClass::Network);
        notes.push("definition may access the network".into());
    }
    if has_word(
        &all,
        &[
            "clean", "rimraf", "rm", "rmdir", "unlink", "purge", "delete", "del",
        ],
    ) {
        classes.push(EffectClass::Deletes);
        notes.push("name or definition suggests deleting files".into());
    }
    if has_word(
        &all,
        &[
            "build", "compile", "bundle", "fmt", "format", "fix", "generate", "gen", "codegen",
            "install", "migrate", "mkdir", "touch", "cp", "mv", "tee", "write", "update", "init",
        ],
    ) || body.contains('>')
        || has_phrase(body, &["sed -i", "--write", "--fix"])
    {
        classes.push(EffectClass::ModifiesFiles);
        notes.push("name or definition suggests writing files".into());
    }

    if classes.is_empty() {
        if has_word(
            &name_words,
            &[
                "test",
                "tests",
                "lint",
                "check",
                "typecheck",
                "verify",
                "status",
                "list",
                "audit",
            ],
        ) {
            return Effects {
                classes: vec![EffectClass::ReadOnly],
                certainty: EffectCertainty::Heuristic,
                source: "heuristic:task_name".into(),
                confidence: 0.4,
                notes: vec![
                    "probably read-only judging by its name; tests and linters can still write caches or reports".into(),
                ],
            };
        }
        return Effects::unknown("effects of this task have not been characterized");
    }

    classes.sort();
    classes.dedup();
    Effects {
        classes,
        certainty: EffectCertainty::Heuristic,
        source: "heuristic:task_definition".into(),
        confidence: 0.5,
        notes,
    }
}

/// Assess an executable from its name alone.
pub fn heuristic_effects_for_executable(name: &str) -> Effects {
    let class = match name {
        "sudo" | "doas" | "su" | "pkexec" | "runuser" => Some(EffectClass::PrivilegeEscalation),
        "rm" | "rmdir" | "shred" | "unlink" | "wipefs" | "mkfs" => Some(EffectClass::Deletes),
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "rsync" | "nc" | "ncat" | "telnet" | "ftp" => {
            Some(EffectClass::Network)
        }
        _ => None,
    };
    match class {
        Some(c) => Effects {
            classes: vec![c],
            certainty: EffectCertainty::Heuristic,
            source: "heuristic:executable_name".into(),
            confidence: 0.6,
            notes: vec![format!("`{name}` is commonly used for {}", c.as_str())],
        },
        None => Effects::unknown("effects of this executable have not been characterized"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_is_high_impact() {
        let e = heuristic_effects_for_task("publish", "node scripts/publish.js");
        assert!(e.classes.contains(&EffectClass::Publishes));
        assert_eq!(e.certainty, EffectCertainty::Heuristic);
    }

    #[test]
    fn tests_are_probably_read_only_but_not_declared() {
        let e = heuristic_effects_for_task("test", "node test.js");
        assert_eq!(e.classes, vec![EffectClass::ReadOnly]);
        assert!(!e.is_well_characterized_read_only());
    }

    #[test]
    fn unknown_is_not_read_only() {
        let e = heuristic_effects_for_task("frobnicate", "node x.js");
        assert_eq!(e.certainty, EffectCertainty::Unknown);
        assert!(e.classes.is_empty());
        let e = heuristic_effects_for_executable("mytool");
        assert_eq!(e.certainty, EffectCertainty::Unknown);
    }

    #[test]
    fn clean_deletes() {
        let e = heuristic_effects_for_task("clean", "rm -rf dist");
        assert!(e.classes.contains(&EffectClass::Deletes));
    }
}

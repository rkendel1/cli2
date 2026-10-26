//! Human-readable rendering. Agents should use `--json` instead of parsing this.

use std::fmt::Write as _;

use cli2_core::*;
use cli2_evidence::TaskRecord;
use cli2_index::ReconcileReport;

use crate::engine::ScanReport;

fn effects_line(e: &Effects) -> String {
    let classes = if e.classes.is_empty() {
        "unknown".to_string()
    } else {
        e.classes
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let certainty = match e.certainty {
        EffectCertainty::Unknown => "not characterized".to_string(),
        EffectCertainty::Heuristic => {
            format!("heuristic, {} (confidence {:.1})", e.source, e.confidence)
        }
        EffectCertainty::Declared => format!("declared, {}", e.source),
    };
    format!("{classes}  [{certainty}]")
}

fn approval_line(p: &Plan) -> String {
    let level = match p.approval.level {
        ApprovalLevel::None => "not required (default local policy)",
        ApprovalLevel::Confirm => "confirmation required (--yes accepted)",
        ApprovalLevel::Explicit => "EXPLICIT approval required (--yes is not sufficient)",
        ApprovalLevel::Forbidden => "FORBIDDEN by policy",
    };
    level.to_string()
}

pub fn preview(plan: &Plan) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Proposed action: {}", plan.intent);
    for step in &plan.steps {
        let _ = writeln!(s, "  Capability    {}", step.capability_id);
        let _ = writeln!(s, "  Command       {}", step.display_command());
        let _ = writeln!(s, "  Executable    {}", step.resolved_executable);
        let _ = writeln!(s, "  Directory     {}", step.working_directory);
    }
    let _ = writeln!(s, "  Why           {}", plan.selection_reason);
    let _ = writeln!(s, "  Fits intent   {}", plan.rationale);
    if plan.candidates.len() > 1 {
        let _ = writeln!(s, "  Candidates");
        for c in plan.candidates.iter().take(5) {
            let _ = writeln!(
                s,
                "    {:>6.2}  {}{}",
                c.score,
                c.capability_id,
                if c.viable { "" } else { "  (not viable)" }
            );
        }
    }
    let _ = writeln!(s, "  Effects       {}", effects_line(&plan.effects));
    for n in &plan.effects.notes {
        let _ = writeln!(s, "                - {n}");
    }
    let _ = writeln!(s, "  Risk          {:?}", plan.risk);
    let _ = writeln!(s, "  Approval      {}", approval_line(plan));
    for r in &plan.approval.requirements {
        let _ = writeln!(s, "                - {}", r.reason);
    }
    let _ = writeln!(s, "  Verification");
    for c in &plan.verification.checks {
        let _ = writeln!(
            s,
            "                - {} [{}, {}]",
            c.check.describe(),
            if c.task_level {
                "task-level"
            } else {
                "process-level"
            },
            c.source
        );
    }
    let _ = writeln!(s, "  Timeout       {}s", plan.timeout_secs);
    let _ = writeln!(
        s,
        "  Plan          {}  digest {}",
        plan.plan_id,
        plan.short_digest()
    );
    s
}

pub fn result(r: &ExecutionResult) -> String {
    let mut s = String::new();
    let mark = match r.status {
        Status::Succeeded | Status::Planned => "✔",
        Status::AwaitingApproval | Status::NeedsClarification => "?",
        _ => "✘",
    };
    let _ = writeln!(s, "{mark} {}", r.message);
    if let Some(c) = &r.clarification {
        for (i, o) in c.options.iter().enumerate() {
            let inv = o
                .invocation
                .as_ref()
                .map(|v| {
                    format!(
                        "  → {}",
                        v.iter()
                            .map(|a| shell_quote(a))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                })
                .unwrap_or_default();
            let _ = writeln!(s, "  {}. {}: {}{inv}", i + 1, o.capability_id, o.summary);
        }
    }
    for reason in &r.rejection {
        let _ = writeln!(s, "  - {reason}");
    }
    for step in &r.steps {
        let status = match (step.termination, step.exit_code, step.signal) {
            (Termination::Exited, Some(c), _) => format!("exit {c}"),
            (t, _, Some(sig)) => format!("{t:?} (signal {sig})"),
            (t, _, _) => format!("{t:?}"),
        };
        let _ = writeln!(
            s,
            "  step {}: {} in {}ms; stdout {} bytes{}, stderr {} bytes{}",
            step.index,
            status,
            step.duration_ms,
            step.stdout_bytes,
            if step.stdout_truncated {
                " (truncated)"
            } else {
                ""
            },
            step.stderr_bytes,
            if step.stderr_truncated {
                " (truncated)"
            } else {
                ""
            },
        );
    }
    if r.verification.status != VerificationStatus::NotRun {
        let _ = writeln!(s, "  verification: {:?}", r.verification.status);
        for c in &r.verification.checks {
            let m = match c.status {
                CheckStatus::Passed => "✓",
                CheckStatus::Failed => "✗",
                CheckStatus::Skipped => "-",
            };
            let _ = writeln!(s, "    {m} {} — {}", c.check.describe(), c.detail);
        }
    }
    if !r.limitations.is_empty() {
        let _ = writeln!(s, "  limitations:");
        for l in &r.limitations {
            let _ = writeln!(s, "    - {l}");
        }
    }
    let _ = writeln!(s, "  evidence: cli2 task show {}", r.request_id);
    s
}

pub fn output_tail(label: &str, bytes: &[u8], max_lines: usize) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "── {label}{} ──",
        if start > 0 {
            format!(" (last {max_lines} lines)")
        } else {
            String::new()
        }
    );
    for l in &lines[start..] {
        let _ = writeln!(s, "{l}");
    }
    s
}

fn reconcile(s: &mut String, label: &str, r: &ReconcileReport) {
    let _ = writeln!(
        s,
        "{label}: {} added, {} changed, {} removed, {} unchanged",
        r.added.len(),
        r.changed.len(),
        r.removed.len(),
        r.unchanged
    );
    for id in r.added.iter().take(20) {
        let _ = writeln!(s, "  + {id}");
    }
    if r.added.len() > 20 {
        let _ = writeln!(s, "  + … {} more", r.added.len() - 20);
    }
    for c in &r.changed {
        let _ = writeln!(
            s,
            "  ~ {} (invalidated: {})",
            c.id,
            c.invalidated.join(", ")
        );
    }
    for id in &r.removed {
        let _ = writeln!(s, "  - {id}");
    }
}

pub fn scan(r: &ScanReport) -> String {
    let mut s = String::new();
    if let Some(from) = &r.rebuilt_from {
        let _ = writeln!(
            s,
            "Rebuilt the index; the previous file was moved to {from}"
        );
    }
    let _ = writeln!(s, "Index: {}", r.index_path);
    let _ = writeln!(
        s,
        "Shell: {}   Working directory: {}",
        r.shell.as_deref().unwrap_or("unknown"),
        r.working_directory
    );
    reconcile(
        &mut s,
        &format!(
            "Executables on PATH ({} directories)",
            r.path_directories.len()
        ),
        &r.environment,
    );
    match (&r.project_root, &r.project) {
        (Some(root), Some(p)) => reconcile(&mut s, &format!("Project tasks in {root}"), p),
        _ => {
            let _ = writeln!(
                s,
                "No project manifest found in or above the working directory."
            );
        }
    }
    for w in &r.warnings {
        let _ = writeln!(s, "warning: {w}");
    }
    s
}

pub fn inventory(caps: &[Capability]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:<44} {:<13} {:<24} DESCRIPTION / LOCATION",
        "ID", "STATE", "EFFECTS"
    );
    for c in caps {
        let effects = if c.effects.classes.is_empty() {
            "unknown".to_string()
        } else {
            c.effects
                .classes
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        let desc = c
            .purpose
            .as_ref()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| c.location.path.clone());
        let desc: String = desc.chars().take(70).collect();
        let _ = writeln!(
            s,
            "{:<44} {:<13} {:<24} {desc}",
            c.id,
            format!("{:?}", c.enrichment).to_lowercase(),
            effects
        );
    }
    let _ = writeln!(s, "{} capabilities", caps.len());
    s
}

pub fn capability(c: &Capability) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{}", c.id);
    let _ = writeln!(s, "  kind          {:?}", c.kind);
    let _ = writeln!(s, "  location      {}", c.location.path);
    if let Some(root) = &c.location.project_root {
        let _ = writeln!(s, "  project       {root}");
    }
    for sh in &c.location.shadows {
        let _ = writeln!(s, "  shadows       {sh}");
    }
    let _ = writeln!(
        s,
        "  discovered    {} via {:?}",
        c.source.discovered_at, c.source.kind
    );
    let _ = writeln!(s, "  fingerprint   {}", c.fingerprint);
    let _ = writeln!(s, "  knowledge     {:?}", c.enrichment);
    if let Some(p) = &c.purpose {
        let _ = writeln!(
            s,
            "  purpose       {} [{}, confidence {:.1}]",
            p.value, p.source, p.confidence
        );
    }
    let mut inv = vec![c.interface.executable.clone()];
    inv.extend(c.interface.arguments.iter().cloned());
    for p in &c.interface.parameters {
        inv.push(if p.required {
            format!("<{}>", p.name)
        } else {
            format!("[{}]", p.name)
        });
    }
    let _ = writeln!(
        s,
        "  interface     {}  [{:?}, {}]",
        inv.join(" "),
        c.interface.completeness,
        c.interface.source
    );
    if !c.interface.known_flags.is_empty() {
        let _ = writeln!(s, "  flags         {}", c.interface.known_flags.join(" "));
    }
    let _ = writeln!(s, "  effects       {}", effects_line(&c.effects));
    for n in &c.effects.notes {
        let _ = writeln!(s, "                - {n}");
    }
    if let Some(h) = &c.help {
        let first: String = h.excerpt.lines().take(3).collect::<Vec<_>>().join(" | ");
        let _ = writeln!(s, "  help          captured {}: {first}", h.captured_at);
    }
    s
}

pub fn task(t: &TaskRecord) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Request {}  ({})",
        t.request.request_id, t.request.received_at
    );
    let _ = writeln!(s, "  intent        {}", t.request.intent);
    let _ = writeln!(s, "  directory     {}", t.request.working_directory);
    let _ = writeln!(
        s,
        "  caller        {:?} (established by {})",
        t.request.caller.kind, t.request.caller.established_by
    );
    for p in &t.plans {
        let _ = writeln!(s, "Plan {}  digest {}", p.plan_id, p.short_digest());
        for st in &p.steps {
            let _ = writeln!(s, "  {}  →  {}", st.capability_id, st.display_command());
        }
        let _ = writeln!(s, "  approval      {}", approval_line(p));
    }
    for a in &t.approvals {
        let _ = writeln!(
            s,
            "Approval {}: {:?} via {:?} for {} at {}",
            a.approval_id, a.decision, a.method, a.plan_id, a.decided_at
        );
    }
    for r in &t.results {
        s.push_str(&result(r));
    }
    let _ = writeln!(
        s,
        "Events ({}), journal integrity: {}",
        t.events.len(),
        t.journal_integrity
    );
    for e in &t.events {
        let _ = writeln!(s, "  {:>5}  {}  {}", e.seq, e.recorded_at, e.kind);
    }
    s
}

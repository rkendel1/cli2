//! Parsing of captured help output.
//!
//! Only conventional, unambiguous forms are understood. Anything else leaves
//! the interface `unknown`/partial rather than guessing: parameters and flags
//! are never invented.

use cli2_core::Parameter;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedHelp {
    pub usage_line: Option<String>,
    pub purpose: Option<String>,
    pub parameters: Vec<Parameter>,
    pub flags: Vec<String>,
    /// True when the usage line was fully understood.
    pub usage_complete: bool,
}

const OPTION_PLACEHOLDERS: &[&str] = &[
    "options",
    "option",
    "flags",
    "flag",
    "opts",
    "args...",
    "arguments",
];

pub fn parse_help(text: &str, exe_name: &str) -> ParsedHelp {
    let mut out = ParsedHelp::default();
    let lines: Vec<&str> = text.lines().collect();

    let usage_idx = lines
        .iter()
        .position(|l| l.trim_start().to_lowercase().starts_with("usage:"));
    if let Some(i) = usage_idx {
        let mut usage = lines[i].trim_start()[6..].trim().to_string();
        // clap-style: "Usage:" alone with the command on the next line.
        if usage.is_empty() {
            if let Some(next) = lines.get(i + 1) {
                usage = next.trim().to_string();
            }
        }
        out.usage_line = Some(usage.clone());
        let (params, complete) = parse_usage(&usage, exe_name);
        out.parameters = params;
        out.usage_complete = complete;
    }

    out.purpose = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != usage_idx && Some(i.wrapping_sub(1)) != usage_idx)
        .map(|(_, l)| l.trim())
        .find(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.to_lowercase().starts_with("usage")
                && !l.ends_with(':')
                && l.split_whitespace().count() >= 2
        })
        .map(|l| {
            let l = l
                .strip_prefix(&format!("{exe_name} - "))
                .or_else(|| l.strip_prefix(&format!("{exe_name}: ")))
                .unwrap_or(l);
            l.chars().take(200).collect()
        });

    let mut flags = Vec::new();
    for line in &lines {
        let t = line.trim_start();
        if !t.starts_with('-') {
            continue;
        }
        for tok in t.split_whitespace() {
            let tok = tok.trim_end_matches(',');
            let tok = tok.split(['=', '[']).next().unwrap_or("");
            if tok.starts_with('-') && tok.len() > 1 && tok != "--" && is_flag_token(tok) {
                flags.push(tok.to_string());
            } else {
                break;
            }
        }
    }
    flags.sort();
    flags.dedup();
    out.flags = flags;
    out
}

fn is_flag_token(tok: &str) -> bool {
    let body = tok.trim_start_matches('-');
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse positional parameters from a usage line. Returns the parameters and
/// whether the whole line was understood.
fn parse_usage(usage: &str, exe_name: &str) -> (Vec<Parameter>, bool) {
    let mut tokens = usage.split_whitespace().peekable();
    // First token is the program name (possibly a path).
    match tokens.next() {
        Some(first) if first.rsplit('/').next() == Some(exe_name) => {}
        _ => return (vec![], false),
    }
    let mut params = Vec::new();
    let source = "help_output:usage";
    for raw in tokens {
        let variadic = raw.ends_with("...");
        let tok = raw.trim_end_matches("...");
        let (optional, inner) = if tok.starts_with('[') && tok.ends_with(']') {
            (true, &tok[1..tok.len() - 1])
        } else {
            (false, tok)
        };
        let inner = inner.trim_end_matches("...");
        if inner.starts_with('-') {
            continue; // a flag
        }
        if OPTION_PLACEHOLDERS.contains(&inner.to_lowercase().as_str()) {
            continue;
        }
        let name = if inner.starts_with('<') && inner.ends_with('>') && inner.len() > 2 {
            &inner[1..inner.len() - 1]
        } else if !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-')
            && inner.chars().any(|c| c.is_ascii_uppercase())
        {
            inner
        } else {
            // Subcommands, alternations ({a|b}), nested brackets: not understood.
            return (params, false);
        };
        let lower = name.to_lowercase();
        if OPTION_PLACEHOLDERS.contains(&lower.as_str()) {
            continue;
        }
        params.push(Parameter {
            name: lower,
            required: !optional,
            variadic,
            source: source.into(),
        });
    }
    (params, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_style_help() {
        let text = "Write a summary report of a directory\n\nUsage: mkreport [OPTIONS] <OUTPUT> [SOURCE]\n\nOptions:\n  -t, --title <TITLE>  Report title\n  -h, --help           Print help\n";
        let p = parse_help(text, "mkreport");
        assert_eq!(
            p.purpose.as_deref(),
            Some("Write a summary report of a directory")
        );
        assert!(p.usage_complete);
        assert_eq!(p.parameters.len(), 2);
        assert_eq!(p.parameters[0].name, "output");
        assert!(p.parameters[0].required);
        assert!(!p.parameters[1].required);
        assert_eq!(p.flags, vec!["--help", "--title", "-h", "-t"]);
    }

    #[test]
    fn subcommands_are_not_guessed() {
        let p = parse_help("usage: git [--version] <command> [<args>]\n", "git");
        assert!(p.usage_complete);
        let p = parse_help("usage: tool {start|stop}\n", "tool");
        assert!(!p.usage_complete);
        assert!(p.parameters.is_empty());
    }

    #[test]
    fn name_dash_description() {
        let p = parse_help(
            "frob - turn widgets into gadgets\nusage: frob FILE...\n",
            "frob",
        );
        assert_eq!(p.purpose.as_deref(), Some("turn widgets into gadgets"));
        assert!(p.parameters[0].variadic);
    }
}

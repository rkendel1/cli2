//! Parsing of captured help output into a structured interface.
//!
//! Help output is **untrusted text**: it is only ever read, never executed or
//! interpreted as instructions. The parser is deliberately conservative:
//!
//! * only conventional, unambiguous forms are recognized;
//! * a usage line that is not fully understood yields a *partial* interface —
//!   it is never taken as proof that the executable has no arguments;
//! * options are recorded with what the text says (flag vs. value, aliases),
//!   but recognition never makes an option usable by the planner by itself;
//! * everything not understood is listed in `uncertainties`.

use cli2_core::{OptionSpec, OptionValue, Parameter, ParameterRole};

/// Bump when parsing behaviour changes; stored captures with an older version
/// are re-parsed from their saved output.
pub const PARSER_VERSION: u32 = 2;

const MAX_LINES: usize = 3000;
const MAX_OPTIONS: usize = 600;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedHelp {
    pub usage_line: Option<String>,
    pub purpose: Option<String>,
    pub parameters: Vec<Parameter>,
    /// Every recognized option name, sorted.
    pub flags: Vec<String>,
    pub options: Vec<OptionSpec>,
    /// True when the primary usage form was fully understood.
    pub usage_complete: bool,
    /// Usage forms after the first that were seen and not interpreted.
    pub alternate_usage_forms: u32,
    pub uncertainties: Vec<String>,
}

impl ParsedHelp {
    /// Whether anything usable was recognized at all.
    pub fn recognized_anything(&self) -> bool {
        self.usage_line.is_some() || !self.options.is_empty() || self.purpose.is_some()
    }
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

/// Remove terminal escape sequences and control characters.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI / OSC sequences.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for n in chars.by_ref() {
                            if ('@'..='~').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        for n in chars.by_ref() {
                            if n == '\u{7}' {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            '\u{8}' => {
                // man-style overstrike: "x\bx" -> "x"
                out.pop();
            }
            '\t' => out.push_str("    "),
            '\n' | '\r' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn parse_help(text: &str, exe_name: &str) -> ParsedHelp {
    let text = sanitize(text);
    let lines: Vec<&str> = text.lines().take(MAX_LINES).collect();
    let mut out = ParsedHelp::default();

    // ---- usage block
    let usage_idx = lines.iter().position(|l| {
        let t = l.trim_start().to_lowercase();
        t.starts_with("usage:") || t.starts_with("usage :")
    });
    let mut usage_range = None;
    if let Some(i) = usage_idx {
        let after = lines[i].trim_start();
        let after = after[after.find(':').map(|c| c + 1).unwrap_or(6)..].trim();
        let mut forms: Vec<String> = Vec::new();
        if !after.is_empty() {
            forms.push(after.to_string());
        }
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.trim().is_empty() {
                // A blank line right after a bare "Usage:" is skipped; otherwise it ends the block.
                if forms.is_empty() {
                    j += 1;
                    continue;
                }
                break;
            }
            let t = l.trim();
            let indented = l.starts_with(' ');
            if !indented {
                break;
            }
            let starts_form = t.rsplit('/').next().is_some_and(|_| {
                t.split_whitespace()
                    .next()
                    .is_some_and(|w| w.rsplit('/').next() == Some(exe_name))
            }) || t.split_whitespace().any(|w| w == exe_name);
            if starts_form || forms.is_empty() {
                forms.push(t.to_string());
            } else if t.starts_with('[') || t.starts_with('<') {
                // A physical wrap of the current form.
                let last = forms.last_mut().unwrap();
                last.push(' ');
                last.push_str(t);
            } else {
                break; // the options table or prose, not usage
            }
            j += 1;
        }
        usage_range = Some((i, j));
        if let Some(primary) = forms.first() {
            out.usage_line = Some(primary.chars().take(500).collect());
            let (params, complete, notes) = parse_usage(primary, exe_name);
            out.parameters = params;
            out.usage_complete = complete;
            out.uncertainties.extend(notes);
        }
        out.alternate_usage_forms = forms.len().saturating_sub(1) as u32;
        if out.alternate_usage_forms > 0 {
            out.uncertainties.push(format!(
                "{} further usage form(s) were not interpreted; only the first was used",
                out.alternate_usage_forms
            ));
        }
    } else {
        out.uncertainties.push("no `usage:` line found".into());
    }

    // ---- options
    let (options, option_notes) = parse_options(&lines, usage_range, &out.usage_line);
    out.options = options;
    out.uncertainties.extend(option_notes);
    let mut flags: Vec<String> = out
        .options
        .iter()
        .flat_map(|o| o.names.iter().cloned())
        .collect();
    flags.sort();
    flags.dedup();
    out.flags = flags;

    // ---- purpose
    out.purpose = purpose(&lines, usage_range, exe_name);

    // ---- what omitting a path-like optional positional means
    let intro: String = lines
        .iter()
        .take(25)
        .map(|l| l.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let cwd_stated = intro.contains("current directory") || intro.contains("working directory");
    if cwd_stated {
        for p in out.parameters.iter_mut() {
            if !p.required && p.role() == ParameterRole::Path {
                p.omitted_means =
                    Some("searches the current directory (stated in help text)".into());
            }
        }
    }
    out
}

fn purpose(lines: &[&str], usage_range: Option<(usize, usize)>, exe_name: &str) -> Option<String> {
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((a, b)) = usage_range {
            if i >= a && i < b {
                i += 1;
                continue;
            }
        }
        let t = lines[i].trim();
        let skip = t.is_empty()
            || t.starts_with('-')
            || t.ends_with(':')
            || looks_like_banner(t, exe_name);
        if skip {
            if !para.is_empty() {
                break;
            }
            i += 1;
            continue;
        }
        if para.is_empty() && t.split_whitespace().count() < 3 {
            i += 1;
            continue;
        }
        para.push(t);
        i += 1;
    }
    if para.is_empty() {
        return None;
    }
    let mut joined = para.join(" ");
    let prefixes = [format!("{exe_name} - "), format!("{exe_name}: ")];
    for p in &prefixes {
        if let Some(rest) = joined.strip_prefix(p.as_str()) {
            joined = rest.to_string();
        }
    }
    if let Some(end) = joined.find(". ") {
        joined.truncate(end + 1);
    }
    Some(joined.chars().take(200).collect())
}

fn looks_like_banner(t: &str, exe_name: &str) -> bool {
    let lower = t.to_lowercase();
    lower.starts_with("usage")
        || lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("project home")
        || (t.contains('<') && t.contains('@') && t.contains('>'))
        || {
            // "name 1.2.3" version banner
            let mut it = t.split_whitespace();
            let first = it.next().unwrap_or("");
            let second = it.next().unwrap_or("");
            it.next().is_none()
                && (first == exe_name || first.chars().all(|c| c.is_alphanumeric() || c == '-'))
                && second
                    .trim_start_matches('v')
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
        }
}

// ---------------------------------------------------------------- usage line

/// Split a usage line into top-level tokens, keeping bracketed groups whole.
fn tokenize(usage: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in usage.chars() {
        match c {
            '[' | '<' | '{' | '(' => {
                depth += 1;
                cur.push(c);
            }
            ']' | '>' | '}' | ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                depth = 0;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // A lone "..." belongs to the previous token ("[file ...]" is one group, "FILE ..." is two).
    let mut merged: Vec<String> = Vec::new();
    for t in out {
        if (t == "..." || t == "…") && !merged.is_empty() {
            let last = merged.last_mut().unwrap();
            last.push_str("...");
        } else {
            merged.push(t);
        }
    }
    merged
}

fn positional_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with('<') && raw.ends_with('>') && raw.len() > 2 {
        let inner = &raw[1..raw.len() - 1];
        if inner
            .chars()
            .all(|c| c.is_alphanumeric() || "_-.".contains(c))
        {
            return Some(inner.to_lowercase());
        }
        return None;
    }
    let ok_upper = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && raw.chars().any(|c| c.is_ascii_uppercase());
    ok_upper.then(|| raw.to_lowercase())
}

/// Returns (positional parameters, whether the line was fully understood, notes).
fn parse_usage(usage: &str, exe_name: &str) -> (Vec<Parameter>, bool, Vec<String>) {
    let tokens = tokenize(usage);
    let mut it = tokens.into_iter();
    match it.next() {
        Some(first) if first.rsplit('/').next() == Some(exe_name) => {}
        _ => {
            return (
                vec![],
                false,
                vec![format!("the usage line does not start with `{exe_name}`")],
            )
        }
    }
    let mut params: Vec<Parameter> = Vec::new();
    let mut complete = true;
    let mut notes = Vec::new();
    let source = "help_output:usage";

    for raw in it {
        let variadic_outer = raw.ends_with("...") || raw.ends_with('…');
        let tok = raw.trim_end_matches("...").trim_end_matches('…');
        let (optional, inner) = if tok.starts_with('[') && tok.ends_with(']') && tok.len() >= 2 {
            (true, tok[1..tok.len() - 1].trim())
        } else {
            (false, tok)
        };
        let inner_trim = inner.trim_end_matches("...").trim_end_matches('…').trim();
        let variadic_inner = inner.trim_end().ends_with("...") || inner.trim_end().ends_with('…');
        let variadic = variadic_outer || variadic_inner;

        if inner_trim.starts_with('-') {
            if !optional {
                complete = false;
                notes.push(format!(
                    "a required option `{inner_trim}` in the usage line was not interpreted"
                ));
            }
            continue; // options come from the options section / bracket groups
        }
        if OPTION_PLACEHOLDERS.contains(&inner_trim.to_lowercase().as_str()) {
            continue;
        }
        if inner_trim.contains('|') || inner_trim.contains('{') || inner_trim.contains('(') {
            complete = false;
            notes.push(format!(
                "alternatives or grouping `{inner_trim}` were not interpreted"
            ));
            continue;
        }
        // A bracketed group may hold a name and its ellipsis: "file ...".
        let name_part = inner_trim.split_whitespace().next().unwrap_or("");
        let extra = inner_trim.split_whitespace().skip(1).collect::<Vec<_>>();
        let extras_ok = extra.iter().all(|e| *e == "..." || *e == "…");
        let name = if optional {
            // Inside brackets lowercase words are conventional placeholders (`[file ...]`).
            let n = name_part.trim_start_matches('<').trim_end_matches('>');
            (!n.is_empty()
                && extras_ok
                && n.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)))
            .then(|| n.to_lowercase())
        } else if extras_ok {
            positional_name(name_part)
        } else {
            None
        };
        let Some(name) = name else {
            // Subcommands, bare lowercase words, nested groups: not understood.
            complete = false;
            notes.push(format!("usage token `{raw}` was not understood"));
            continue;
        };
        if OPTION_PLACEHOLDERS.contains(&name.as_str()) {
            continue;
        }
        params.push(Parameter {
            name,
            required: !optional,
            variadic,
            source: source.into(),
            omitted_means: None,
        });
    }
    (params, complete, notes)
}

// ------------------------------------------------------------------- options

fn valid_option_name(n: &str) -> bool {
    if let Some(rest) = n.strip_prefix("--") {
        !rest.is_empty()
            && rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    } else if let Some(rest) = n.strip_prefix('-') {
        !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && rest.len() <= 24
    } else {
        false
    }
}

/// Parse one comma-separated alias chunk: `-A NUM`, `--context=<NUM>`, `-n`.
fn parse_chunk(chunk: &str, in_usage: bool) -> Option<(String, OptionValue, Option<String>)> {
    let chunk = chunk.trim();
    let mut parts = chunk.splitn(2, [' ', '=']);
    let first = parts.next()?;
    let rest = chunk[first.len()..].trim_start_matches(' ');
    // `-C[num]` / `--color[=when]`
    if let Some(i) = first.find('[') {
        let name = &first[..i];
        if !valid_option_name(name) {
            return None;
        }
        let vn = chunk[i..]
            .trim_matches(|c| c == '[' || c == ']' || c == '=')
            .to_string();
        return Some((
            name.to_string(),
            OptionValue::Optional,
            Some(vn).filter(|v| !v.is_empty()),
        ));
    }
    if !valid_option_name(first) {
        return None;
    }
    let rest = rest.trim_start_matches('=');
    if rest.is_empty() {
        return Some((first.to_string(), OptionValue::None, None));
    }
    let v = rest.split_whitespace().next().unwrap_or("");
    let looks_like_value = (v.starts_with('<') && v.ends_with('>'))
        || (v.chars().any(|c| c.is_ascii_uppercase())
            && v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_-.".contains(c)))
        || chunk[first.len()..].starts_with('=')
        // Inside a usage bracket `[-A num]` the word is a placeholder.
        || (in_usage
            && v.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            && !v.starts_with('-'));
    if looks_like_value {
        let name = v.trim_matches(|c| c == '<' || c == '>').to_string();
        Some((first.to_string(), OptionValue::Required, Some(name)))
    } else {
        // A lowercase word after the name is more likely the description.
        Some((first.to_string(), OptionValue::None, None))
    }
}

fn parse_options(
    lines: &[&str],
    usage_range: Option<(usize, usize)>,
    usage_line: &Option<String>,
) -> (Vec<OptionSpec>, Vec<String>) {
    let mut options: Vec<OptionSpec> = Vec::new();
    let mut notes = Vec::new();

    // From the usage line's bracket groups: `[-A num]`, `[-abc]`, `[--x=value]`.
    if let Some(u) = usage_line {
        for tok in tokenize(u) {
            if !(tok.starts_with('[') && tok.ends_with(']') && tok.len() > 2) {
                continue;
            }
            let inner = &tok[1..tok.len() - 1];
            if !inner.starts_with('-') {
                continue;
            }
            for alt in inner.split('|') {
                let alt = alt.trim();
                // BSD cluster `-abcDE`: each letter is a flag.
                if alt.starts_with('-')
                    && !alt.starts_with("--")
                    && alt.len() > 2
                    && alt[1..].chars().all(|c| c.is_ascii_alphabetic())
                {
                    for c in alt[1..].chars() {
                        add_option(
                            &mut options,
                            vec![format!("-{c}")],
                            OptionValue::None,
                            None,
                            None,
                            "help_output:usage",
                        );
                    }
                    continue;
                }
                if let Some((name, value, vn)) = parse_chunk(alt, true) {
                    add_option(
                        &mut options,
                        vec![name],
                        value,
                        vn,
                        None,
                        "help_output:usage",
                    );
                }
            }
        }
    }

    // From option tables: lines beginning with `-`.
    let mut i = 0;
    while i < lines.len() {
        if let Some((a, b)) = usage_range {
            if i >= a && i < b {
                i += 1;
                continue;
            }
        }
        let line = lines[i];
        let t = line.trim_start();
        if !t.starts_with('-') || t.starts_with("- ") || t == "-" || t == "--" {
            i += 1;
            continue;
        }
        // The specification ends at the first run of 2+ spaces.
        let (spec, desc_inline) = match t.find("  ") {
            Some(k) => (&t[..k], t[k..].trim()),
            None => (t, ""),
        };
        let mut names = Vec::new();
        let mut value = OptionValue::None;
        let mut value_name = None;
        let mut ok = true;
        for chunk in spec.split(',') {
            match parse_chunk(chunk, false) {
                Some((n, v, vn)) => {
                    names.push(n);
                    if v != OptionValue::None {
                        value = v;
                        value_name = vn;
                    }
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || names.is_empty() {
            i += 1;
            continue;
        }
        let mut desc = desc_inline.to_string();
        if desc.is_empty() {
            // rg-style: description on the next indented line.
            if let Some(next) = lines.get(i + 1) {
                let nt = next.trim();
                if next.starts_with(' ') && !nt.is_empty() && !nt.starts_with('-') {
                    desc = nt.to_string();
                }
            }
        }
        add_option(
            &mut options,
            names,
            value,
            value_name,
            Some(desc)
                .filter(|d| !d.is_empty())
                .map(|d| d.chars().take(160).collect()),
            "help_output:options",
        );
        i += 1;
    }
    if options.len() > MAX_OPTIONS {
        options.truncate(MAX_OPTIONS);
        notes.push(format!(
            "more than {MAX_OPTIONS} options; the rest were ignored"
        ));
    }
    if options.is_empty() {
        notes.push("no options were recognized".into());
    }
    (options, notes)
}

fn add_option(
    options: &mut Vec<OptionSpec>,
    names: Vec<String>,
    value: OptionValue,
    value_name: Option<String>,
    description: Option<String>,
    source: &str,
) {
    if let Some(existing) = options
        .iter_mut()
        .find(|o| o.names.iter().any(|n| names.contains(n)))
    {
        // The options table is richer than the usage line: merge aliases and details.
        for n in &names {
            if !existing.names.contains(n) {
                existing.names.push(n.clone());
            }
        }
        if existing.value == OptionValue::None && value != OptionValue::None {
            existing.value = value;
            existing.value_name = value_name;
        }
        if existing.description.is_none() {
            existing.description = description;
        }
        if source == "help_output:options" {
            existing.source = source.into();
        }
        return;
    }
    options.push(OptionSpec {
        names,
        value,
        value_name,
        description,
        source: source.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt<'a>(p: &'a ParsedHelp, name: &str) -> &'a OptionSpec {
        p.options
            .iter()
            .find(|o| o.names.iter().any(|n| n == name))
            .unwrap_or_else(|| panic!("option {name} not recognized: {:?}", p.options))
    }

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
        let t = opt(&p, "--title");
        assert_eq!(t.value, OptionValue::Required);
        assert_eq!(t.value_name.as_deref(), Some("TITLE"));
        assert_eq!(t.names, vec!["-t", "--title"]);
        assert_eq!(opt(&p, "-h").value, OptionValue::None);
    }

    #[test]
    fn flags_aliases_and_value_options() {
        let text = "usage: tool [OPTIONS] <PATH>\n\n  -n, --line-number        Show line numbers\n      --context NUM        Lines of context\n  -o, --output FILE        Write here\n  -A NUM, --after-context=NUM  After\n      --color[=WHEN]       Colorize\n";
        let p = parse_help(text, "tool");
        assert_eq!(opt(&p, "--line-number").value, OptionValue::None);
        assert_eq!(opt(&p, "-n").names, vec!["-n", "--line-number"]);
        assert_eq!(opt(&p, "--context").value, OptionValue::Required);
        assert_eq!(opt(&p, "--output").value_name.as_deref(), Some("FILE"));
        assert_eq!(opt(&p, "--after-context").names.len(), 2);
        assert_eq!(opt(&p, "--color").value, OptionValue::Optional);
        assert_eq!(p.parameters[0].name, "path");
        assert!(p.parameters[0].required);
    }

    #[test]
    fn bsd_usage_with_bracket_groups() {
        let text = "usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz] [-A num] [-B num] [-C[num]]\n\t[-e pattern] [--binary-files=value] [--color=when] [--label]\n\t[pattern] [file ...]\n";
        let p = parse_help(text, "grep");
        // The usage line wraps; only the first physical line is the primary form.
        let one_line = text.replace("\n\t", " ");
        let p2 = parse_help(&one_line, "grep");
        assert!(p2.usage_complete, "{:?}", p2.uncertainties);
        let names: Vec<&str> = p2.parameters.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["pattern", "file"]);
        assert!(!p2.parameters[0].required && p2.parameters[1].variadic);
        assert_eq!(opt(&p2, "-A").value, OptionValue::Required);
        assert_eq!(opt(&p2, "-a").value, OptionValue::None);
        assert_eq!(opt(&p2, "--binary-files").value, OptionValue::Required);
        assert_eq!(opt(&p2, "--label").value, OptionValue::None);
        assert_eq!(opt(&p2, "-C").value, OptionValue::Optional);
        // The wrapped form is not silently treated as complete knowledge of "no more arguments".
        assert!(p.alternate_usage_forms > 0 || !p.usage_complete || p.parameters.len() <= 2);
    }

    #[test]
    fn multiple_usage_forms_are_counted_not_merged() {
        let text = "tool 1.0\n\nA tool that searches files for patterns.\n\nUSAGE:\n    tool [OPTIONS] PATTERN [PATH ...]\n    tool [OPTIONS] -e PATTERN ... [PATH ...]\n    command | tool [OPTIONS] PATTERN\n\nOPTIONS:\n    -i, --ignore-case\n        Search case insensitively.\n";
        let p = parse_help(text, "tool");
        assert!(p.usage_complete);
        assert_eq!(p.alternate_usage_forms, 2);
        assert!(p
            .uncertainties
            .iter()
            .any(|u| u.contains("further usage form")));
        assert_eq!(p.parameters.len(), 2);
        assert!(p.parameters[0].required && !p.parameters[1].required && p.parameters[1].variadic);
        assert_eq!(
            opt(&p, "--ignore-case").description.as_deref(),
            Some("Search case insensitively.")
        );
        assert_eq!(
            p.purpose.as_deref(),
            Some("A tool that searches files for patterns.")
        );
    }

    #[test]
    fn omission_meaning_is_only_recorded_when_the_help_states_it() {
        let stated = parse_help("tool recursively searches the current directory for a pattern.\n\nUsage: tool PATTERN [PATH ...]\n", "tool");
        assert!(stated.parameters[1].omitted_means.is_some());
        let silent = parse_help("usage: tool [pattern] [file ...]\n", "tool");
        assert!(silent.parameters.iter().all(|p| p.omitted_means.is_none()));
    }

    #[test]
    fn subcommands_alternatives_and_unknown_syntax_are_not_guessed() {
        let p = parse_help("usage: git [--version] <command> [<args>]\n", "git");
        assert!(
            !p.usage_complete
                || p.parameters.iter().all(|x| x.name != "command")
                || p.usage_complete
        );
        let p = parse_help("usage: tool {start|stop}\n", "tool");
        assert!(!p.usage_complete);
        assert!(p.parameters.is_empty());
        let p = parse_help("usage: tool subcommand FILE\n", "tool");
        assert!(
            !p.usage_complete,
            "bare lowercase words are not positionals"
        );
        let p = parse_help(
            "usage: find [-H | -L | -P] [-EXdsx] [-f path] path ... [expression]\n",
            "find",
        );
        assert!(!p.usage_complete);
    }

    #[test]
    fn malformed_or_hostile_help_yields_nothing_trusted() {
        for text in [
            "",
            "   \n\n",
            "\u{1b}[31mnot help\u{1b}[0m",
            "Segmentation fault",
            "$(rm -rf /)\n`reboot`\n--help; rm x",
        ] {
            let p = parse_help(text, "tool");
            assert!(!p.usage_complete, "{text:?}");
            assert!(p.parameters.is_empty(), "{text:?}");
        }
        // Commands mentioned in help are plain text, never executed or promoted to options.
        let p = parse_help("usage: tool FILE\n  -x; rm -rf /   evil\n", "tool");
        assert!(p
            .options
            .iter()
            .all(|o| o.names.iter().all(|n| !n.contains(';'))));
    }

    #[test]
    fn ansi_and_overstrike_are_stripped() {
        let p = parse_help("\u{1b}[1mUsage:\u{1b}[0m tool [OPTIONS] <FILE>\n  \u{1b}[32m-v\u{1b}[0m, --verbose   Be loud\n", "tool");
        assert!(p.usage_complete);
        assert_eq!(opt(&p, "-v").names, vec!["-v", "--verbose"]);
    }

    #[test]
    fn name_dash_description_and_banners() {
        let p = parse_help(
            "frob - turn widgets into gadgets\nusage: frob FILE...\n",
            "frob",
        );
        assert_eq!(p.purpose.as_deref(), Some("turn widgets into gadgets"));
        assert!(p.parameters[0].variadic);
        let p = parse_help("ripgrep 15.2.0\nAndrew <a@b.c>\n\nripgrep (rg) recursively searches the current directory for lines. By default x.\n\nUSAGE:\n    rg [OPTIONS] PATTERN [PATH ...]\n", "rg");
        assert_eq!(
            p.purpose.as_deref(),
            Some("ripgrep (rg) recursively searches the current directory for lines.")
        );
    }
}

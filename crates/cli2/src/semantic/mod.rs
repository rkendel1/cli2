//! Semantic resolution with Apple Foundation Models.
//!
//! The model is a *semantic resolver*, not an execution agent: it only reorders
//! candidates retrieved from the capability index (and proposes search terms
//! that are matched against that same index). Everything it returns is
//! validated here before it can influence a score, and plan construction,
//! authority and verification are unchanged.
//!
//! * [`bridge`]   — subprocess protocol with the Swift helper
//! * [`validate`] — independent validation of the helper's response
//! * [`ranker`]   — the [`cli2_resolver::SemanticRanker`] implementation
//! * [`status`]   — `cli2 model status` / `cli2 model test`
//! * [`bench`]    — `cli2 model benchmark`

pub mod bench;
pub mod bridge;
pub mod ranker;
pub mod status;
pub mod validate;

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use ranker::AppleFmRanker;

pub const PROVIDER_ID: &str = "apple-foundation-models";

/// Which resolver produces the final candidate ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverMode {
    /// Existing deterministic resolver only.
    Deterministic,
    /// Apple Foundation Models ranking; deterministic fallback per `fallback`.
    AppleFm,
    /// Deterministic first; Apple ranking only when it cannot plan.
    Auto,
}

impl ResolverMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "deterministic" => Some(Self::Deterministic),
            "apple-fm" | "apple-foundation-models" => Some(Self::AppleFm),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::AppleFm => "apple-fm",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fallback {
    /// Use the deterministic resolver when the model cannot be used and it can
    /// plan the request.
    Deterministic,
    /// Never fall back: a failed model is an error in `apple-fm` mode.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Provider {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "apple-foundation-models")]
    AppleFoundationModels,
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_CANDIDATES: usize = 24;
pub const MAX_CANDIDATES_LIMIT: usize = 40;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 128 * 1024;

/// Validated `resolver` section of `$CLI2_HOME/config.json`.
///
/// ```json
/// { "resolver": {
///     "provider": "apple-foundation-models",   // "none" (default) | "apple-foundation-models"
///     "fallback": "deterministic",             // "deterministic" (default) | "none"
///     "mode": "auto",                          // optional: deterministic | apple-fm | auto
///     "bridge_path": "/path/to/cli2-apple-fm", // optional
///     "timeout_secs": 30,                      // 1..=300, per inference call
///     "max_candidates": 24                     // 1..=40 sent to the model
/// } }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ResolverSettings {
    pub provider: Provider,
    pub fallback: Fallback,
    /// Mode used when `--resolver` is not given. Defaults to `auto` when a
    /// provider is configured and `deterministic` otherwise.
    pub mode: Option<ResolverMode>,
    pub bridge_path: Option<PathBuf>,
    pub timeout_secs: u64,
    pub max_candidates: usize,
    pub max_response_bytes: usize,
}

impl Default for ResolverSettings {
    fn default() -> Self {
        Self {
            provider: Provider::None,
            fallback: Fallback::Deterministic,
            mode: None,
            bridge_path: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    resolver: Option<RawResolver>,
    /// Validated by `output::OutputSettings`.
    #[allow(dead_code)]
    output: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResolver {
    provider: Option<Provider>,
    fallback: Option<Fallback>,
    mode: Option<String>,
    bridge_path: Option<PathBuf>,
    timeout_secs: Option<u64>,
    max_candidates: Option<usize>,
}

impl ResolverSettings {
    /// Parse and validate the contents of `config.json`.
    pub fn from_config_json(text: &str) -> Result<Self, String> {
        let file: ConfigFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut s = Self::default();
        let Some(raw) = file.resolver else {
            return Ok(s);
        };
        if let Some(p) = raw.provider {
            s.provider = p;
        }
        if let Some(f) = raw.fallback {
            s.fallback = f;
        }
        if let Some(m) = raw.mode {
            s.mode = Some(ResolverMode::parse(&m).ok_or_else(|| {
                format!("resolver.mode {m:?} must be deterministic, apple-fm or auto")
            })?);
        }
        s.bridge_path = raw.bridge_path;
        if let Some(t) = raw.timeout_secs {
            if !(1..=300).contains(&t) {
                return Err(format!(
                    "resolver.timeout_secs {t} must be between 1 and 300"
                ));
            }
            s.timeout_secs = t;
        }
        if let Some(n) = raw.max_candidates {
            if !(1..=MAX_CANDIDATES_LIMIT).contains(&n) {
                return Err(format!(
                    "resolver.max_candidates {n} must be between 1 and {MAX_CANDIDATES_LIMIT}"
                ));
            }
            s.max_candidates = n;
        }
        if s.provider == Provider::None
            && matches!(s.mode, Some(ResolverMode::AppleFm | ResolverMode::Auto))
        {
            return Err("resolver.mode needs resolver.provider \"apple-foundation-models\"".into());
        }
        Ok(s)
    }

    /// Load `$home/config.json`; absent means defaults.
    pub fn load(home: &Path) -> Result<Self, String> {
        let path = home.join("config.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_config_json(&text)
                .map_err(|e| format!("invalid config file {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The mode in effect when `--resolver` is absent.
    pub fn default_mode(&self) -> Option<ResolverMode> {
        match (self.mode, self.provider) {
            (Some(m), _) => Some(m),
            (None, Provider::AppleFoundationModels) => Some(ResolverMode::Auto),
            (None, Provider::None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let s = ResolverSettings::from_config_json(
            r#"{"resolver":{"provider":"apple-foundation-models","fallback":"deterministic"}}"#,
        )
        .unwrap();
        assert_eq!(s.provider, Provider::AppleFoundationModels);
        assert_eq!(s.default_mode(), Some(ResolverMode::Auto));
        assert_eq!(s.max_candidates, DEFAULT_MAX_CANDIDATES);
    }

    #[test]
    fn empty_config_is_deterministic() {
        let s = ResolverSettings::from_config_json("{}").unwrap();
        assert_eq!(s.default_mode(), None);
    }

    #[test]
    fn rejects_unknown_and_out_of_range_values() {
        for bad in [
            r#"{"resolver":{"provider":"openai"}}"#,
            r#"{"resolver":{"fallback":"retry"}}"#,
            r#"{"resolver":{"timeout_secs":0}}"#,
            r#"{"resolver":{"max_candidates":500}}"#,
            r#"{"resolver":{"api_key":"x"}}"#,
            r#"{"resolver":{"mode":"auto"}}"#,
            r#"{"other":1}"#,
        ] {
            assert!(ResolverSettings::from_config_json(bad).is_err(), "{bad}");
        }
    }
}

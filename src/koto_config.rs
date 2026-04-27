//! Project-level config (`koto.yaml`).
//!
//! Optional file in the working directory that defines capability tiers,
//! project defaults and template variables. When the file is absent everything
//! works as before -- callers receive `None` from [`KotoConfig::load_optional`].
//!
//! Resolution cascade and roles are handled in #129; seeds in #130.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::config::Version;

/// File name looked up in the working directory.
pub const KOTO_CONFIG_FILE: &str = "koto.yaml";

#[derive(Debug, thiserror::Error)]
pub enum KotoConfigError {
    #[error("failed to read koto.yaml: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse koto.yaml: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("validation error: {0}")]
    Validation(String),
}

impl KotoConfigError {
    /// The error payload without any thiserror prefix.
    ///
    /// Use this when forwarding the error into another error type that adds
    /// its own prefix (for example [`crate::config::ConfigError::Validation`]).
    /// Avoids the duplicated "validation error: validation error: ..." you'd
    /// get from `to_string()` plus a wrapping `Validation` variant.
    pub fn message(&self) -> String {
        match self {
            Self::Io(e) => e.to_string(),
            Self::Parse(e) => e.to_string(),
            Self::Validation(msg) => msg.clone(),
        }
    }
}

/// Project-level backend choice. Distinct from the runtime [`crate::config::Backend`]
/// enum: this is the operator's policy (spawn a subprocess vs call the HTTP API),
/// while `Backend` names a concrete provider/transport pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KotoBackend {
    Cli,
    Api,
}

// --- Raw deserialization ---

#[derive(Debug, Deserialize)]
struct RawKotoConfig {
    version: Version,
    #[serde(default)]
    tiers: HashMap<String, String>,
    #[serde(default)]
    defaults: Option<RawKotoDefaults>,
    #[serde(default)]
    vars: HashMap<String, String>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawKotoDefaults {
    #[serde(default)]
    backend: Option<KotoBackend>,
    #[serde(flatten)]
    unknown: HashMap<String, serde_yaml::Value>,
}

// --- Resolved config ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KotoConfig {
    pub version: String,
    pub tiers: HashMap<String, String>,
    pub default_backend: Option<KotoBackend>,
    pub vars: HashMap<String, String>,
}

impl KotoConfig {
    /// Load `koto.yaml` from `dir` if it exists. Returns `Ok(None)` when the
    /// file is missing -- callers must treat that as the no-op case to keep
    /// the no-koto.yaml workflow intact.
    pub fn load_optional(dir: &Path) -> Result<Option<Self>, KotoConfigError> {
        let path = dir.join(KOTO_CONFIG_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)?;
        Ok(Some(Self::from_yaml_str(&contents)?))
    }

    pub fn from_yaml_str(contents: &str) -> Result<Self, KotoConfigError> {
        let raw: RawKotoConfig = serde_yaml::from_str(contents)?;

        for key in raw.unknown.keys() {
            eprintln!("warning: unknown field '{key}' in koto.yaml");
        }
        if let Some(ref defaults) = raw.defaults {
            for key in defaults.unknown.keys() {
                eprintln!("warning: unknown field '{key}' in koto.yaml defaults");
            }
        }

        if raw.version.0 != "1" {
            return Err(KotoConfigError::Validation(format!(
                "unsupported version '{}', expected '1'",
                raw.version.0
            )));
        }

        // Iterate in sorted order so error messages for invalid tiers are
        // deterministic across runs -- HashMap iteration order is not stable.
        let mut tier_names: Vec<&String> = raw.tiers.keys().collect();
        tier_names.sort();
        for tier_name in tier_names {
            if tier_name.trim().is_empty() {
                return Err(KotoConfigError::Validation(
                    "tier name must not be empty".to_string(),
                ));
            }
            validate_model_format(&raw.tiers[tier_name])?;
        }

        Ok(Self {
            version: raw.version.0,
            tiers: raw.tiers,
            default_backend: raw.defaults.and_then(|d| d.backend),
            vars: raw.vars,
        })
    }

    /// Resolve a tier name to its `<provider>/<model-id>` string.
    ///
    /// The error variant lists available tiers sorted alphabetically so the
    /// message is stable across runs.
    pub fn resolve_tier(&self, tier_name: &str) -> Result<&str, KotoConfigError> {
        self.tiers
            .get(tier_name)
            .map(String::as_str)
            .ok_or_else(|| {
                let mut available: Vec<&str> = self.tiers.keys().map(String::as_str).collect();
                available.sort();
                KotoConfigError::Validation(format!(
                    "tier \"{tier_name}\" not defined in koto.yaml (available: {})",
                    available.join(", ")
                ))
            })
    }
}

fn validate_model_format(model: &str) -> Result<(), KotoConfigError> {
    let mut parts = model.split('/');
    let provider = parts.next().unwrap_or("");
    let model_id = parts.next().unwrap_or("");
    let extra = parts.next();
    if provider.is_empty() || model_id.is_empty() || extra.is_some() {
        return Err(KotoConfigError::Validation(format!(
            "model must be <provider>/<model-id>, got \"{model}\""
        )));
    }
    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_KOTO_YAML: &str = r#"
version: "1"

tiers:
  reasoning: claude/opus-4-7
  general: claude/sonnet-4-6
  quick: claude/haiku-4-5

defaults:
  backend: cli

vars:
  owner: nestrai
  repo: koto
"#;

    #[test]
    fn full_config_parses() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        assert_eq!(cfg.version, "1");
        assert_eq!(cfg.tiers.len(), 3);
        assert_eq!(cfg.tiers.get("reasoning").unwrap(), "claude/opus-4-7");
        assert_eq!(cfg.default_backend, Some(KotoBackend::Cli));
        assert_eq!(cfg.vars.get("owner").unwrap(), "nestrai");
        assert_eq!(cfg.vars.get("repo").unwrap(), "koto");
    }

    #[test]
    fn version_only_parses() {
        let cfg = KotoConfig::from_yaml_str(r#"version: "1""#).unwrap();
        assert_eq!(cfg.version, "1");
        assert!(cfg.tiers.is_empty());
        assert!(cfg.default_backend.is_none());
        assert!(cfg.vars.is_empty());
    }

    #[test]
    fn version_as_integer_parses() {
        let cfg = KotoConfig::from_yaml_str("version: 1").unwrap();
        assert_eq!(cfg.version, "1");
    }

    #[test]
    fn unsupported_version_errors() {
        let err = KotoConfig::from_yaml_str(r#"version: "2""#).unwrap_err();
        assert!(err.to_string().contains("unsupported version"));
    }

    #[test]
    fn missing_version_errors() {
        let err = KotoConfig::from_yaml_str("tiers:\n  general: claude/sonnet-4-6").unwrap_err();
        assert!(matches!(err, KotoConfigError::Parse(_)));
    }

    #[test]
    fn malformed_yaml_propagates_parse_error() {
        // Mismatched indentation / broken YAML
        let err = KotoConfig::from_yaml_str("version: \"1\"\ntiers:\n  bad: : :").unwrap_err();
        let msg = err.to_string();
        // serde_yaml errors include a position reference; just verify it's a parse error
        assert!(matches!(err, KotoConfigError::Parse(_)), "got: {msg}");
    }

    #[test]
    fn invalid_model_format_no_slash_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: opus-4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("model must be <provider>/<model-id>, got \"opus-4-7\""),
            "got: {err}"
        );
    }

    #[test]
    fn invalid_model_format_two_slashes_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: claude/opus/4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn invalid_model_format_empty_provider_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: /opus-4-7
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn invalid_model_format_empty_id_errors() {
        let yaml = r#"
version: "1"
tiers:
  reasoning: claude/
"#;
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("model must be"), "got: {err}");
    }

    #[test]
    fn resolve_tier_returns_model() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        assert_eq!(cfg.resolve_tier("reasoning").unwrap(), "claude/opus-4-7");
    }

    #[test]
    fn resolve_tier_missing_errors_with_available() {
        let cfg = KotoConfig::from_yaml_str(FULL_KOTO_YAML).unwrap();
        let err = cfg.resolve_tier("phantom").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tier \"phantom\" not defined"), "got: {msg}");
        assert!(msg.contains("available:"), "got: {msg}");
        // The available list is alphabetically sorted
        assert!(msg.contains("general"), "got: {msg}");
        assert!(msg.contains("quick"), "got: {msg}");
        assert!(msg.contains("reasoning"), "got: {msg}");
    }

    #[test]
    fn defaults_backend_api_parses() {
        let yaml = r#"
version: "1"
defaults:
  backend: api
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.default_backend, Some(KotoBackend::Api));
    }

    #[test]
    fn load_optional_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn load_optional_present_file_returns_some() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KOTO_CONFIG_FILE), FULL_KOTO_YAML).unwrap();
        let cfg = KotoConfig::load_optional(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.tiers.len(), 3);
    }

    #[test]
    fn unknown_top_level_field_does_not_fail() {
        // Unknown fields warn but parsing succeeds, matching flow YAML behavior.
        let yaml = r#"
version: "1"
something_new: hello
"#;
        let cfg = KotoConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.version, "1");
    }

    #[test]
    fn invalid_model_format_error_is_deterministic() {
        // Regression: HashMap iteration order is non-deterministic, so
        // multiple invalid tiers used to produce different error messages
        // across runs. Validation iterates sorted by tier name -- the first
        // invalid tier alphabetically should always be the one reported.
        let yaml = r#"
version: "1"
tiers:
  zeta: missing-slash
  alpha: also-missing
  middle: fine/model
"#;
        // Run several times: alphabetic order means "alpha" is checked first,
        // so the message must always quote "also-missing".
        for _ in 0..20 {
            let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
            assert!(
                err.to_string().contains("\"also-missing\""),
                "expected first-by-name tier in error, got: {err}"
            );
        }
    }

    #[test]
    fn empty_tier_name_errors() {
        // YAML allows an empty key only via explicit quoting.
        let yaml = "version: \"1\"\ntiers:\n  \"\": claude/opus-4-7\n";
        let err = KotoConfig::from_yaml_str(yaml).unwrap_err();
        assert!(err.to_string().contains("tier name"), "got: {err}");
    }
}

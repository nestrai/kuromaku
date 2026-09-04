//! Role resolution cascade for `kuro run`.
//!
//! Combines four override layers to produce a final agent, model and backend
//! for each role used by a flow run:
//!
//! 1. CLI `--role NAME=AGENT` and `--role NAME:FIELD=VALUE`
//! 2. Project config `roles.<name>` (project-level binding)
//! 3. tiers (when the agent declares one) and project config
//!    `defaults.backend`
//! 4. Agent YAML `model:` / `backend:` and the flow-level role default
//!
//! Each [`ResolvedRole`] carries source labels so the audit output can show
//! exactly which layer won.

use std::collections::HashMap;

use crate::config::{Backend, FlowConfig};
#[cfg(test)]
use crate::koto_config::RoleOverlay;
use crate::koto_config::{KOTO_CONFIG_FILE, KotoBackend, KotoConfig, KotoRole, Seeds};

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("invalid --role override '{value}': {reason}")]
    BadOverrideSyntax { value: String, reason: String },

    #[error("unknown role field \"{field}\" (valid: model, backend)")]
    UnknownRoleField { field: String },

    #[error("role \"{name}\" not defined in {KOTO_CONFIG_FILE}")]
    UnknownRole { name: String },

    #[error("invalid model format \"{model}\": must be <provider>/<model-id>")]
    BadModelFormat { model: String },

    #[error("invalid backend \"{value}\": must be 'cli' or 'api'")]
    BadBackend { value: String },
}

/// One CLI `--role` flag invocation, parsed but not yet applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleOverride {
    /// `--role developer=Kai` -- rebind the role's agent.
    Agent { role: String, agent: String },
    /// `--role developer:model=ollama/codestral`.
    Model { role: String, model: String },
    /// `--role developer:backend=api`.
    Backend { role: String, backend: KotoBackend },
}

impl RoleOverride {
    pub fn role_name(&self) -> &str {
        match self {
            RoleOverride::Agent { role, .. } => role,
            RoleOverride::Model { role, .. } => role,
            RoleOverride::Backend { role, .. } => role,
        }
    }
}

/// Final binding for one role after the cascade has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRole {
    pub name: String,
    pub agent: String,
    pub model: String,
    pub backend: Backend,
    /// Where the model value came from, e.g. "tier: reasoning" or "CLI override".
    pub model_source: String,
    /// Where the backend value came from.
    pub backend_source: String,
    /// Display string of the seed the agent file was loaded from, when known.
    /// `None` for callers that don't track seeds (e.g. the legacy single-dir
    /// path). The audit prints this as `<- <seed-display>` when present.
    pub seed_origin: Option<String>,
    /// Agent-level `extra_args` for the resolved backend (#236). Empty when
    /// the agent declares no `extra_args` for this backend. Step-level
    /// overrides do not feed into the audit -- this captures the agent
    /// binding so the user sees what would be applied unless a step
    /// declares its own `extra_args`.
    pub extra_args: Vec<String>,
}

// --- Override parsing ---

/// Parse a single `--role` value. Accepts:
/// - `NAME=AGENT`
/// - `NAME:FIELD=VALUE` where FIELD is `model` or `backend`
pub fn parse_role_override(value: &str) -> Result<RoleOverride, ResolverError> {
    let (lhs, rhs) = value
        .split_once('=')
        .ok_or_else(|| ResolverError::BadOverrideSyntax {
            value: value.to_string(),
            reason: "expected NAME=AGENT or NAME:FIELD=VALUE".to_string(),
        })?;

    if let Some((role, field)) = lhs.split_once(':') {
        if role.trim().is_empty() {
            return Err(ResolverError::BadOverrideSyntax {
                value: value.to_string(),
                reason: "empty role name".to_string(),
            });
        }
        match field {
            "model" => {
                validate_model_format(rhs)?;
                Ok(RoleOverride::Model {
                    role: role.to_string(),
                    model: rhs.to_string(),
                })
            }
            "backend" => {
                let backend = KotoBackend::parse(rhs).ok_or_else(|| ResolverError::BadBackend {
                    value: rhs.to_string(),
                })?;
                Ok(RoleOverride::Backend {
                    role: role.to_string(),
                    backend,
                })
            }
            other => Err(ResolverError::UnknownRoleField {
                field: other.to_string(),
            }),
        }
    } else {
        if lhs.trim().is_empty() {
            return Err(ResolverError::BadOverrideSyntax {
                value: value.to_string(),
                reason: "empty role name".to_string(),
            });
        }
        if rhs.trim().is_empty() {
            return Err(ResolverError::BadOverrideSyntax {
                value: value.to_string(),
                reason: "empty agent".to_string(),
            });
        }
        Ok(RoleOverride::Agent {
            role: lhs.to_string(),
            agent: rhs.to_string(),
        })
    }
}

fn validate_model_format(model: &str) -> Result<(), ResolverError> {
    let mut parts = model.split('/');
    let provider = parts.next().unwrap_or("");
    let model_id = parts.next().unwrap_or("");
    let extra = parts.next();
    if provider.is_empty() || model_id.is_empty() || extra.is_some() {
        return Err(ResolverError::BadModelFormat {
            model: model.to_string(),
        });
    }
    Ok(())
}

/// Validate that every CLI `--role` override targets a role that exists in
/// either the project config or the flow YAML. Run once before flow
/// execution.
pub fn validate_role_overrides(
    overrides: &[RoleOverride],
    flow: &FlowConfig,
    koto: Option<&KotoConfig>,
) -> Result<(), ResolverError> {
    for ov in overrides {
        let name = ov.role_name();
        let in_flow = flow.roles.contains_key(name);
        let in_koto = koto.map(|c| c.roles.contains_key(name)).unwrap_or(false);
        if !in_flow && !in_koto {
            return Err(ResolverError::UnknownRole {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Look up the runtime [`Backend`] that goes with a [`KotoBackend`] policy.
///
/// `cli` maps to the existing default of [`Backend::ClaudeCli`] (the only CLI
/// backend wired in today). `api` maps to [`Backend::Api`]. This bridges the
/// project-level policy in the project config to the runtime executor enum.
pub fn project_backend_to_runtime(b: KotoBackend) -> Backend {
    match b {
        KotoBackend::Cli => Backend::ClaudeCli,
        KotoBackend::Api => Backend::Api,
    }
}

/// Inputs needed to resolve a single role.
pub struct RoleResolveInput<'a> {
    pub role_name: &'a str,
    /// The agent's own `model:` field (after tier resolution if any happened).
    pub agent_model: &'a str,
    /// What tier the agent declared (for "tier was: X" labels).
    pub agent_tier: Option<&'a str>,
    /// The agent's own `backend:` field.
    pub agent_backend: Backend,
    /// The flow's `defaults.model`.
    pub flow_default_model: &'a str,
    /// The agent's `extra_args` map keyed by backend (#236). `None` when the
    /// caller has no agent yet (e.g. unresolved binding); otherwise the
    /// resolver picks the slice for the resolved backend and stores it on
    /// [`ResolvedRole`] so the audit can show it.
    pub agent_extra_args: Option<&'a HashMap<Backend, Vec<String>>>,
}

/// Resolve the agent ID for a single role from the cascade.
///
/// Precedence: CLI `--role NAME=AGENT` > flow `roles.<name>.default` >
/// project-config `roles.<name>.agent`. Returns `None` when no layer provides
/// a binding -- callers should treat that as an error.
///
/// This is THE single rule for the agent cascade. Both pre-flow application
/// (writing back into `FlowConfig.roles` and `Step.agent` so the right agent
/// files get loaded) and the full role resolution below read from this
/// function so the rule lives in exactly one place.
pub fn resolve_role_agent(
    role_name: &str,
    flow_role_agent: Option<&str>,
    project_role: Option<&KotoRole>,
    cli_overrides: &[RoleOverride],
) -> Option<String> {
    for ov in cli_overrides {
        if ov.role_name() == role_name
            && let RoleOverride::Agent { agent, .. } = ov
        {
            return Some(agent.clone());
        }
    }
    flow_role_agent
        .map(str::to_string)
        .or_else(|| project_role.map(|r| r.agent.clone()))
}

/// Apply the cascade to one role.
///
/// Returns the resolved agent ID, model, backend and the source labels for
/// each. The agent decision is delegated to [`resolve_role_agent`] so the
/// rule has a single home; this function just labels the result and runs
/// the model/backend cascade. Returns `None` when no agent binding can be
/// found.
pub fn resolve_role(
    input: &RoleResolveInput<'_>,
    flow_role_agent: Option<&str>,
    project_role: Option<&KotoRole>,
    cli_overrides: &[RoleOverride],
    project_default_backend: Option<KotoBackend>,
) -> Option<ResolvedRole> {
    // --- Agent ---
    let agent = resolve_role_agent(
        input.role_name,
        flow_role_agent,
        project_role,
        cli_overrides,
    )?;

    // --- Model / Backend CLI scans ---
    let mut model_cli: Option<&str> = None;
    let mut backend_cli: Option<KotoBackend> = None;
    for ov in cli_overrides {
        if ov.role_name() != input.role_name {
            continue;
        }
        match ov {
            RoleOverride::Agent { .. } => {}
            RoleOverride::Model { model, .. } => model_cli = Some(model.as_str()),
            RoleOverride::Backend { backend, .. } => backend_cli = Some(*backend),
        }
    }

    // --- Model ---
    let project_model = project_role.and_then(|r| r.model.as_deref());
    let (model, model_source) = if let Some(m) = model_cli {
        let label = match (project_model, input.agent_tier) {
            (Some(prev), _) => format!("CLI override, role was: {prev}"),
            (None, Some(t)) => format!("CLI override, tier was: {t}"),
            (None, None) => "CLI override".to_string(),
        };
        (m.to_string(), label)
    } else if let Some(m) = project_model {
        let label = match input.agent_tier {
            Some(t) => format!("role override, tier was: {t}"),
            None => "role override".to_string(),
        };
        (m.to_string(), label)
    } else if let Some(t) = input.agent_tier {
        // The agent's model field has already been tier-resolved at load
        // time in load_agent_file -- we just label it here.
        (input.agent_model.to_string(), format!("tier: {t}"))
    } else if input.agent_model != input.flow_default_model {
        (input.agent_model.to_string(), "agent".to_string())
    } else {
        (input.agent_model.to_string(), "default".to_string())
    };

    // --- Backend ---
    let project_role_backend = project_role.and_then(|r| r.backend);
    let (backend, backend_source) = if let Some(b) = backend_cli {
        let prev = project_role_backend
            .map(|p| format!(", role was: {}", p.as_str()))
            .unwrap_or_default();
        (project_backend_to_runtime(b), format!("CLI override{prev}"))
    } else if let Some(b) = project_role_backend {
        (project_backend_to_runtime(b), "role override".to_string())
    } else if let Some(b) = project_default_backend {
        // `defaults.backend` only takes effect when the agent has not asked
        // for a different backend. If the agent's runtime backend matches the
        // project default we label it as "default", otherwise we keep the
        // agent's backend and label it "agent".
        let project_runtime = project_backend_to_runtime(b);
        if input.agent_backend == project_runtime {
            (project_runtime, "default".to_string())
        } else {
            (input.agent_backend, "agent".to_string())
        }
    } else {
        (input.agent_backend, "agent".to_string())
    };

    // --- extra_args (#236) ---
    // Agent-level slice keyed by the resolved backend. Step-level overrides
    // are merged later in the runner, but the audit reflects the per-role
    // resolution which is agent-scoped.
    let extra_args = input
        .agent_extra_args
        .and_then(|m| m.get(&backend))
        .cloned()
        .unwrap_or_default();

    Some(ResolvedRole {
        name: input.role_name.to_string(),
        agent,
        model,
        backend,
        model_source,
        backend_source,
        seed_origin: None,
        extra_args,
    })
}

/// Format the audit block as a single string (newline-terminated lines).
///
/// Format mirrors the issue's example with one block per role. We sort by role
/// name so the output is stable across runs. Used both for stderr printing
/// and for the `resolution-audit.txt` file written into the run directory
/// (issue #31), so the on-disk record matches what the user saw in the
/// terminal.
///
/// `overlay_summaries` (issue #364): pre-rendered overlay contribution per
/// role (e.g. `"rules+=2, model"`). Empty / absent entries suppress the
/// extra audit line entirely so the no-overlay case is byte-identical to
/// the pre-#364 output.
pub fn format_audit(
    seeds: &Seeds,
    resolved: &[ResolvedRole],
    cli_vars: &HashMap<String, String>,
    overlay_summaries: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    // Seeds line first -- the user sees the search order before any role-level
    // detail. We always emit it (even with the implicit `.kuro/` default) so
    // the audit makes the resolution path explicit.
    out.push_str(&format!("[resolve] seeds: {}\n", seeds.audit_line()));

    let mut sorted: Vec<&ResolvedRole> = resolved.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for r in sorted {
        // Append `<- <seed-display>` when we know which seed produced the
        // agent file. Direct-agent steps don't go through this path; their
        // origin doesn't appear in the audit (matches issue #130 example).
        match &r.seed_origin {
            Some(origin) => out.push_str(&format!(
                "[resolve] {}: {} <- {}\n",
                r.name, r.agent, origin
            )),
            None => out.push_str(&format!("[resolve] {}: {}\n", r.name, r.agent)),
        }
        out.push_str(&format!(
            "           model: {} ({})\n",
            r.model, r.model_source
        ));
        out.push_str(&format!(
            "           backend: {} ({})\n",
            backend_label(r.backend),
            r.backend_source
        ));
        // #236: surface the resolved extra_args so the audit captures the
        // full per-role binding. Skip the line entirely when the agent
        // declares nothing for the resolved backend -- the audit is meant
        // to be terse, and an empty list adds noise.
        if !r.extra_args.is_empty() {
            out.push_str(&format!(
                "           extra_args: [{}]\n",
                r.extra_args.join(" ")
            ));
        }
        // #364: pin the overlay contribution into the audit so the
        // run-directory record explains why the agent's effective model
        // / rules / extra_args differ from the seed YAML. Skipped when
        // the role had no overlays so the audit stays terse.
        if let Some(summary) = overlay_summaries.get(&r.name)
            && !summary.is_empty()
        {
            out.push_str(&format!("           overlays: {summary}\n"));
        }
    }
    if !cli_vars.is_empty() {
        let mut keys: Vec<&String> = cli_vars.keys().collect();
        keys.sort();
        let summary = keys
            .iter()
            .map(|k| format!("{k}={}", cli_vars[*k]))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("[resolve] vars: {summary}\n"));
    }
    out
}

/// Print the audit block to stderr before flow execution. Thin wrapper around
/// [`format_audit`] -- both the terminal and the run-directory copy share the
/// exact same lines.
pub fn print_audit(
    seeds: &Seeds,
    resolved: &[ResolvedRole],
    cli_vars: &HashMap<String, String>,
    overlay_summaries: &HashMap<String, String>,
) {
    let text = format_audit(seeds, resolved, cli_vars, overlay_summaries);
    // Strip the single trailing newline so eprintln! doesn't double up.
    eprint!("{text}");
}

fn backend_label(b: Backend) -> &'static str {
    match b {
        Backend::Api => "api",
        Backend::ClaudeCli => "cli",
        Backend::Codex => "codex",
        Backend::Ollama => "ollama",
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn ki<'a>(name: &'a str, model: &'a str, tier: Option<&'a str>) -> RoleResolveInput<'a> {
        RoleResolveInput {
            role_name: name,
            agent_model: model,
            agent_tier: tier,
            agent_backend: Backend::ClaudeCli,
            flow_default_model: "claude-sonnet-4-5",
            agent_extra_args: None,
        }
    }

    // --- parse_role_override ---

    #[test]
    fn parse_role_rebind() {
        let ov = parse_role_override("developer=Kai").unwrap();
        assert_eq!(
            ov,
            RoleOverride::Agent {
                role: "developer".to_string(),
                agent: "Kai".to_string()
            }
        );
    }

    #[test]
    fn parse_role_model_override() {
        let ov = parse_role_override("reviewer:model=ollama/codestral").unwrap();
        assert_eq!(
            ov,
            RoleOverride::Model {
                role: "reviewer".to_string(),
                model: "ollama/codestral".to_string()
            }
        );
    }

    #[test]
    fn parse_role_backend_override() {
        let ov = parse_role_override("reviewer:backend=api").unwrap();
        assert_eq!(
            ov,
            RoleOverride::Backend {
                role: "reviewer".to_string(),
                backend: KotoBackend::Api
            }
        );
    }

    #[test]
    fn parse_role_unknown_field_errors() {
        let err = parse_role_override("developer:banana=x").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown role field \"banana\""), "got: {msg}");
        assert!(msg.contains("valid: model, backend"), "got: {msg}");
    }

    #[test]
    fn parse_role_bad_model_format_errors() {
        let err = parse_role_override("developer:model=badformat").unwrap_err();
        assert!(matches!(err, ResolverError::BadModelFormat { .. }));
    }

    #[test]
    fn parse_role_bad_backend_errors() {
        let err = parse_role_override("developer:backend=lambda").unwrap_err();
        assert!(matches!(err, ResolverError::BadBackend { .. }));
    }

    #[test]
    fn parse_role_no_equals_errors() {
        let err = parse_role_override("just-a-name").unwrap_err();
        assert!(matches!(err, ResolverError::BadOverrideSyntax { .. }));
    }

    #[test]
    fn parse_role_empty_role_errors() {
        let err = parse_role_override(":model=foo/bar").unwrap_err();
        assert!(matches!(err, ResolverError::BadOverrideSyntax { .. }));
        let err = parse_role_override("=Kai").unwrap_err();
        assert!(matches!(err, ResolverError::BadOverrideSyntax { .. }));
    }

    // --- resolve_role: model cascade ---

    #[test]
    fn model_from_cli_override_beats_everything() {
        let project = KotoRole {
            agent: "Sage".into(),
            model: Some("project/m".into()),
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let cli = vec![RoleOverride::Model {
            role: "dev".into(),
            model: "cli/m".into(),
        }];
        let input = ki("dev", "claude/opus-4-7", Some("reasoning"));
        let r = resolve_role(&input, Some("Sage"), Some(&project), &cli, None).unwrap();
        assert_eq!(r.model, "cli/m");
        assert!(
            r.model_source.contains("CLI override"),
            "got: {}",
            r.model_source
        );
        // Includes what was overridden -- prefers role's previous value.
        assert!(
            r.model_source.contains("role was"),
            "got: {}",
            r.model_source
        );
    }

    #[test]
    fn model_from_role_beats_tier() {
        let project = KotoRole {
            agent: "Sage".into(),
            model: Some("ollama/llama3-70b".into()),
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let input = ki("rev", "claude/opus-4-7", Some("reasoning"));
        let r = resolve_role(&input, Some("Sage"), Some(&project), &[], None).unwrap();
        assert_eq!(r.model, "ollama/llama3-70b");
        assert!(
            r.model_source.contains("role override"),
            "got: {}",
            r.model_source
        );
        assert!(
            r.model_source.contains("tier was: reasoning"),
            "got: {}",
            r.model_source
        );
    }

    #[test]
    fn model_from_role_simple_form_preserved_byte_for_byte() {
        // Issue #383 AC 4/8: role-level model overrides are free strings like
        // the agent-file `model:` literal. A simple-form value (no `/`) must
        // reach ResolvedRole.model exactly as configured -- no splitting, no
        // rewriting -- with source "role override".
        let project = KotoRole {
            agent: "Babis".into(),
            model: Some("claude-opus-4-7".into()),
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let input = ki("writer", "claude-sonnet-4-5", None);
        let r = resolve_role(&input, Some("Babis"), Some(&project), &[], None).unwrap();
        assert_eq!(r.model, "claude-opus-4-7");
        assert_eq!(r.model_source, "role override");
    }

    #[test]
    fn model_from_role_provider_prefixed_preserved_byte_for_byte() {
        // Issue #383 AC 3/4/9: a `/`-containing role model is a literal
        // backend identifier, not a tier reference -- preserved untouched.
        let project = KotoRole {
            agent: "Babis".into(),
            model: Some("anthropic/claude-opus-4-7".into()),
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let input = ki("writer", "claude-sonnet-4-5", None);
        let r = resolve_role(&input, Some("Babis"), Some(&project), &[], None).unwrap();
        assert_eq!(r.model, "anthropic/claude-opus-4-7");
        assert_eq!(r.model_source, "role override");
    }

    #[test]
    fn model_from_tier_when_no_role_override() {
        // Tier is reflected via agent_tier label; agent_model is what tier
        // resolution already produced.
        let input = ki("dev", "claude/opus-4-7", Some("reasoning"));
        let r = resolve_role(&input, Some("Sage"), None, &[], None).unwrap();
        assert_eq!(r.model, "claude/opus-4-7");
        assert_eq!(r.model_source, "tier: reasoning");
    }

    #[test]
    fn model_falls_back_to_agent_when_no_tier_no_role() {
        let input = ki("dev", "claude-haiku-4-5", None);
        let r = resolve_role(&input, Some("Sage"), None, &[], None).unwrap();
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!(r.model_source, "agent");
    }

    #[test]
    fn model_labelled_default_when_agent_uses_flow_default() {
        let input = ki("dev", "claude-sonnet-4-5", None);
        let r = resolve_role(&input, Some("Sage"), None, &[], None).unwrap();
        assert_eq!(r.model_source, "default");
    }

    // --- resolve_role: backend cascade ---

    #[test]
    fn backend_cli_beats_everything() {
        let project = KotoRole {
            agent: "Sage".into(),
            model: None,
            backend: Some(KotoBackend::Cli),
            overlays: RoleOverlay::default(),
        };
        let cli = vec![RoleOverride::Backend {
            role: "dev".into(),
            backend: KotoBackend::Api,
        }];
        let input = ki("dev", "x/y", None);
        let r = resolve_role(
            &input,
            Some("Sage"),
            Some(&project),
            &cli,
            Some(KotoBackend::Cli),
        )
        .unwrap();
        assert_eq!(r.backend, Backend::Api);
        assert!(r.backend_source.contains("CLI override"));
        assert!(r.backend_source.contains("role was: cli"));
    }

    #[test]
    fn backend_role_beats_default() {
        let project = KotoRole {
            agent: "Sage".into(),
            model: None,
            backend: Some(KotoBackend::Api),
            overlays: RoleOverlay::default(),
        };
        let input = ki("rev", "x/y", None);
        let r = resolve_role(
            &input,
            Some("Sage"),
            Some(&project),
            &[],
            Some(KotoBackend::Cli),
        )
        .unwrap();
        assert_eq!(r.backend, Backend::Api);
        assert_eq!(r.backend_source, "role override");
    }

    #[test]
    fn backend_default_when_agent_matches_project_default() {
        let input = ki("dev", "x/y", None); // agent_backend is ClaudeCli
        let r = resolve_role(&input, Some("Sage"), None, &[], Some(KotoBackend::Cli)).unwrap();
        assert_eq!(r.backend, Backend::ClaudeCli);
        assert_eq!(r.backend_source, "default");
    }

    #[test]
    fn extra_args_resolved_for_effective_backend() {
        // #236 audit: ResolvedRole.extra_args carries the agent slice for
        // the resolved backend. Slices for other backends are dropped --
        // the audit shows the binding actually applied at runtime.
        let mut agent_extra: HashMap<Backend, Vec<String>> = HashMap::new();
        agent_extra.insert(
            Backend::ClaudeCli,
            vec!["--dangerously-skip-permissions".to_string()],
        );
        agent_extra.insert(Backend::Codex, vec!["--sandbox".to_string()]);

        let input = RoleResolveInput {
            role_name: "dev",
            agent_model: "x/y",
            agent_tier: None,
            agent_backend: Backend::ClaudeCli,
            flow_default_model: "claude-sonnet-4-5",
            agent_extra_args: Some(&agent_extra),
        };
        let r = resolve_role(&input, Some("Sage"), None, &[], None).unwrap();
        assert_eq!(r.backend, Backend::ClaudeCli);
        assert_eq!(r.extra_args, vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn extra_args_empty_when_no_entry_for_resolved_backend() {
        // #236: the agent has extra_args for codex but the resolved
        // backend is claude-cli -- the audit should reflect "nothing
        // applies" via an empty Vec, not the codex slice.
        let mut agent_extra: HashMap<Backend, Vec<String>> = HashMap::new();
        agent_extra.insert(Backend::Codex, vec!["--sandbox".to_string()]);

        let input = RoleResolveInput {
            role_name: "dev",
            agent_model: "x/y",
            agent_tier: None,
            agent_backend: Backend::ClaudeCli,
            flow_default_model: "claude-sonnet-4-5",
            agent_extra_args: Some(&agent_extra),
        };
        let r = resolve_role(&input, Some("Sage"), None, &[], None).unwrap();
        assert!(r.extra_args.is_empty());
    }

    #[test]
    fn format_audit_includes_extra_args_when_present() {
        // #236 audit format: when the resolved role carries non-empty
        // extra_args, the audit prints them on a dedicated line. Empty
        // slices are suppressed to keep the audit terse (verified in
        // format_audit_omits_extra_args_when_empty).
        let role = ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "default".to_string(),
            backend_source: "default".to_string(),
            seed_origin: None,
            extra_args: vec!["--dangerously-skip-permissions".to_string()],
        };
        let audit = format_audit(
            &Seeds::default_local(),
            &[role],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            audit.contains("extra_args: [--dangerously-skip-permissions]"),
            "audit did not include extra_args line, got:\n{audit}"
        );
    }

    #[test]
    fn format_audit_omits_extra_args_when_empty() {
        // Counter to format_audit_includes_extra_args_when_present:
        // when the slice is empty the audit must not print an
        // `extra_args: []` line. Empty noise hurts readability and a
        // future reader would wonder whether the empty list is
        // semantically distinct from "no entry".
        let role = ResolvedRole {
            name: "dev".to_string(),
            agent: "Sage".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "default".to_string(),
            backend_source: "default".to_string(),
            seed_origin: None,
            extra_args: Vec::new(),
        };
        let audit = format_audit(
            &Seeds::default_local(),
            &[role],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            !audit.contains("extra_args"),
            "audit should not mention extra_args when empty, got:\n{audit}"
        );
    }

    /// AC5: the audit must surface overlay contributions for every
    /// role that had them, with the rendered summary appearing on a
    /// dedicated `overlays:` line. Suppressed for roles with no
    /// overlay -- pinned in the omits test below to keep the audit
    /// terse for the no-overlay case.
    #[test]
    fn format_audit_includes_overlays_when_provided() {
        let role = ResolvedRole {
            name: "writer".to_string(),
            agent: "Babis".to_string(),
            model: "claude/opus-4-7".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "agent".to_string(),
            backend_source: "agent".to_string(),
            seed_origin: None,
            extra_args: Vec::new(),
        };
        let mut overlays = HashMap::new();
        overlays.insert("writer".to_string(), "rules+=2, model".to_string());
        let audit = format_audit(&Seeds::default_local(), &[role], &HashMap::new(), &overlays);
        assert!(
            audit.contains("overlays: rules+=2, model"),
            "audit missing overlay summary, got:\n{audit}"
        );
    }

    #[test]
    fn format_audit_omits_overlays_when_absent() {
        let role = ResolvedRole {
            name: "writer".to_string(),
            agent: "Babis".to_string(),
            model: "claude/opus-4-7".to_string(),
            backend: Backend::ClaudeCli,
            model_source: "agent".to_string(),
            backend_source: "agent".to_string(),
            seed_origin: None,
            extra_args: Vec::new(),
        };
        let audit = format_audit(
            &Seeds::default_local(),
            &[role],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            !audit.contains("overlays:"),
            "audit should not mention overlays when empty, got:\n{audit}"
        );
    }

    #[test]
    fn backend_keeps_agent_when_agent_overrides_project_default() {
        let input = RoleResolveInput {
            role_name: "dev",
            agent_model: "x/y",
            agent_tier: None,
            agent_backend: Backend::Ollama,
            flow_default_model: "claude-sonnet-4-5",
            agent_extra_args: None,
        };
        let r = resolve_role(&input, Some("Sage"), None, &[], Some(KotoBackend::Cli)).unwrap();
        assert_eq!(r.backend, Backend::Ollama);
        assert_eq!(r.backend_source, "agent");
    }

    // --- agent rebind ---

    #[test]
    fn agent_cli_rebind_wins() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let cli = vec![RoleOverride::Agent {
            role: "dev".into(),
            agent: "Cli".into(),
        }];
        let input = ki("dev", "x/y", None);
        let r = resolve_role(&input, Some("Flow"), Some(&project), &cli, None).unwrap();
        assert_eq!(r.agent, "Cli");
    }

    #[test]
    fn agent_flow_beats_project() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let input = ki("dev", "x/y", None);
        let r = resolve_role(&input, Some("Flow"), Some(&project), &[], None).unwrap();
        assert_eq!(r.agent, "Flow");
    }

    #[test]
    fn agent_falls_back_to_project_when_flow_omits() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let input = ki("dev", "x/y", None);
        let r = resolve_role(&input, None, Some(&project), &[], None).unwrap();
        assert_eq!(r.agent, "Project");
    }

    #[test]
    fn agent_returns_none_when_no_binding() {
        let input = ki("dev", "x/y", None);
        let r = resolve_role(&input, None, None, &[], None);
        assert!(r.is_none());
    }

    // --- resolve_role_agent (single source of truth) ---

    #[test]
    fn resolve_role_agent_cli_wins() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let cli = vec![RoleOverride::Agent {
            role: "dev".into(),
            agent: "Cli".into(),
        }];
        let r = resolve_role_agent("dev", Some("Flow"), Some(&project), &cli);
        assert_eq!(r.as_deref(), Some("Cli"));
    }

    #[test]
    fn resolve_role_agent_flow_beats_project() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let r = resolve_role_agent("dev", Some("Flow"), Some(&project), &[]);
        assert_eq!(r.as_deref(), Some("Flow"));
    }

    #[test]
    fn resolve_role_agent_falls_back_to_project() {
        let project = KotoRole {
            agent: "Project".into(),
            model: None,
            backend: None,
            overlays: RoleOverlay::default(),
        };
        let r = resolve_role_agent("dev", None, Some(&project), &[]);
        assert_eq!(r.as_deref(), Some("Project"));
    }

    #[test]
    fn resolve_role_agent_returns_none_when_no_binding() {
        assert!(resolve_role_agent("dev", None, None, &[]).is_none());
    }

    #[test]
    fn resolve_role_agent_ignores_overrides_for_other_roles() {
        let cli = vec![RoleOverride::Agent {
            role: "other".into(),
            agent: "Cli".into(),
        }];
        let r = resolve_role_agent("dev", Some("Flow"), None, &cli);
        assert_eq!(r.as_deref(), Some("Flow"));
    }
}

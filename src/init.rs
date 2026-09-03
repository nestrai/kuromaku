//! `kuro init` -- non-interactive scaffold for a fresh project (issue #385).
//!
//! One-shot flow: preflight (refuse to touch existing config), detect the
//! project language from marker files, detect an installed backend CLI,
//! render the starter files, write them, print a summary. No prompts --
//! `--yes` is accepted at the CLI layer as a forward-compatibility no-op
//! for the future interactive wizard.
//!
//! The templates at the bottom of this file are the contract with the
//! config loaders: every generated file is fed through the *production*
//! parsers in the unit tests below, so a schema change in
//! [`crate::koto_config`] or [`crate::config`] breaks `just test` instead
//! of a fresh adopter's first `kuro run hello`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::eyre;

use crate::config::Backend;
use crate::koto_config::{KOTO_CONFIG_FILE_LEGACY_KOTO, KOTO_CONFIG_FILE_LEGACY_ROOT, KOTO_DIR};

/// Project language detected from marker files in the target directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectKind {
    Rust,
    Python,
    Go,
    Web,
    Tex,
    Generic,
}

impl ProjectKind {
    /// Human-readable language name, `None` for generic projects. The
    /// templates only mention a language when a marker file identified one
    /// -- acceptance criterion: no marker, no language named.
    fn language(self) -> Option<&'static str> {
        match self {
            ProjectKind::Rust => Some("Rust"),
            ProjectKind::Python => Some("Python"),
            ProjectKind::Go => Some("Go"),
            ProjectKind::Web => Some("web (JavaScript/TypeScript)"),
            ProjectKind::Tex => Some("LaTeX"),
            ProjectKind::Generic => None,
        }
    }
}

/// Marker files checked in order; first match wins. The order is the
/// issue's enumeration order -- e.g. a repo with both `Cargo.toml` and
/// `package.json` counts as Rust. `*.tex` is handled separately because it
/// is a glob, not a fixed name.
const PROJECT_MARKERS: [(&str, ProjectKind); 5] = [
    ("Cargo.toml", ProjectKind::Rust),
    ("pyproject.toml", ProjectKind::Python),
    ("setup.py", ProjectKind::Python),
    ("go.mod", ProjectKind::Go),
    ("package.json", ProjectKind::Web),
];

/// Backend candidates checked in order; first found wins. The env var is
/// the same override the executor honors (`src/executor/mod.rs`) -- init
/// and the executor must agree on what counts as installed.
const BACKEND_CANDIDATES: [(Backend, &str, &str); 3] = [
    (Backend::ClaudeCli, "claude", "CLAUDE_CLI_PATH"),
    (Backend::Codex, "codex", "CODEX_CLI_PATH"),
    (Backend::Ollama, "ollama", "OLLAMA_PATH"),
];

/// Entry point for `kuro init`. Scaffolds `.kuro/` in `dir` and prints a
/// summary plus next steps. Fails without writing anything when existing
/// configuration (canonical or legacy) is present.
pub fn run(dir: &Path) -> Result<()> {
    if let Err(existing) = preflight(dir) {
        eprintln!("found existing configuration:");
        for path in &existing {
            eprintln!("  {path}");
        }
        return Err(eyre!(
            "refusing to initialize: {} already configured -- remove or migrate the paths above first",
            existing.join(", ")
        ));
    }

    let kind = detect_project(dir);
    let backend = match detect_backend(std::env::var_os("PATH").as_deref(), |key| {
        std::env::var_os(key)
    }) {
        Some(backend) => backend,
        None => {
            eprintln!(
                "warning: none of the backend CLIs (claude, codex, ollama) were found on PATH; \
                 defaulting to claude-cli -- install one before running `kuro run hello`"
            );
            Backend::ClaudeCli
        }
    };

    let files = render_files(kind, backend);
    write_files(dir, &files)?;

    match kind.language() {
        Some(language) => println!(
            "Initialized {KOTO_DIR}/ for a {language} project (backend: {})",
            backend.yaml_name()
        ),
        None => println!(
            "Initialized {KOTO_DIR}/ (no project marker found, generic templates; backend: {})",
            backend.yaml_name()
        ),
    }
    for (path, _) in &files {
        println!("  created {}", path.display());
    }
    println!();
    println!("Next steps:");
    println!("  kuro context     inspect the resolved setup");
    println!("  kuro run hello   run the starter flow");
    Ok(())
}

/// Refuse-to-overwrite gate. Returns the list of existing config paths
/// (relative, as the user knows them) when any of the canonical or legacy
/// locations exist. The path literals come from [`crate::koto_config`] --
/// that constant set is the single owner of "where config may live".
fn preflight(dir: &Path) -> std::result::Result<(), Vec<String>> {
    // `.koto/` (directory) is the legacy unit the user migrates, derived
    // from the legacy config-file constant so a rename stays one change.
    let legacy_koto_dir = Path::new(KOTO_CONFIG_FILE_LEGACY_KOTO)
        .parent()
        .unwrap_or_else(|| Path::new(KOTO_CONFIG_FILE_LEGACY_KOTO));

    let candidates: [&Path; 3] = [
        Path::new(KOTO_DIR),
        legacy_koto_dir,
        Path::new(KOTO_CONFIG_FILE_LEGACY_ROOT),
    ];
    let existing: Vec<String> = candidates
        .iter()
        .filter(|rel| dir.join(rel).exists())
        .map(|rel| rel.display().to_string())
        .collect();

    if existing.is_empty() {
        Ok(())
    } else {
        Err(existing)
    }
}

/// Detect the project language from marker files in `dir`. Fixed-name
/// markers first (issue enumeration order), then a non-recursive scan for
/// `*.tex` at the top level -- nested TeX files do not make a TeX project.
fn detect_project(dir: &Path) -> ProjectKind {
    for (marker, kind) in PROJECT_MARKERS {
        if dir.join(marker).is_file() {
            return kind;
        }
    }
    if has_top_level_tex(dir) {
        return ProjectKind::Tex;
    }
    ProjectKind::Generic
}

fn has_top_level_tex(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "tex"))
        })
        .unwrap_or(false)
}

/// Find the first installed backend CLI in [`BACKEND_CANDIDATES`] order.
///
/// Per candidate the executor's env override (`CLAUDE_CLI_PATH` etc.) is
/// consulted first: a value containing a path separator must point at an
/// executable file; a bare name is searched on PATH like the default
/// binary name. An override that resolves to nothing does NOT fall back to
/// the default name -- the executor would use the override verbatim and
/// fail, so init must not claim the backend works.
///
/// PATH and the env lookup are injected so tests can fabricate both.
fn detect_backend(
    path_var: Option<&OsStr>,
    env_override: impl Fn(&str) -> Option<OsString>,
) -> Option<Backend> {
    BACKEND_CANDIDATES
        .iter()
        .find(|(_, binary, env_key)| match env_override(env_key) {
            Some(value) => {
                let value_path = Path::new(&value);
                if value_path.components().count() > 1 {
                    is_executable(value_path)
                } else {
                    find_on_path(&value, path_var)
                }
            }
            None => find_on_path(OsStr::new(binary), path_var),
        })
        .map(|(backend, _, _)| *backend)
}

/// True when any PATH entry contains an executable file named `binary`.
fn find_on_path(binary: &OsStr, path_var: Option<&OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    std::env::split_paths(path_var)
        .filter(|entry| !entry.as_os_str().is_empty())
        .any(|entry| is_executable(&entry.join(binary)))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Render the starter files as (relative path, contents) pairs. Pure --
/// no filesystem access -- so the unit tests can feed the rendered strings
/// straight into the production loaders.
fn render_files(kind: ProjectKind, backend: Backend) -> Vec<(PathBuf, String)> {
    // "a Rust project" / "this project" -- the only language-dependent
    // phrase in the agent role text.
    let project_desc = match kind.language() {
        Some(language) => format!("a {language} project"),
        None => "this project".to_string(),
    };
    // Extra rule-stub line naming the detected language; empty for generic
    // so no language is mentioned anywhere (acceptance criterion 4).
    let language_note = match kind.language() {
        Some(language) => format!("\nDetected project language: {language}.\n"),
        None => String::new(),
    };

    let render_agent = |template: &str| {
        template
            .replace("{backend}", backend.yaml_name())
            .replace("{project_desc}", &project_desc)
    };

    vec![
        (
            PathBuf::from(".kuro/config.yaml"),
            CONFIG_TEMPLATE.to_string(),
        ),
        (
            PathBuf::from(".kuro/agents/Developer.yaml"),
            render_agent(DEVELOPER_AGENT_TEMPLATE),
        ),
        (
            PathBuf::from(".kuro/agents/Reviewer.yaml"),
            render_agent(REVIEWER_AGENT_TEMPLATE),
        ),
        (
            PathBuf::from(".kuro/rules/project-conventions.md"),
            RULE_TEMPLATE.replace("{language_note}", &language_note),
        ),
        (
            PathBuf::from(".kuro/flows/hello.yaml"),
            HELLO_FLOW_TEMPLATE.to_string(),
        ),
    ]
}

/// Write the rendered files under `dir`, creating parent directories.
/// Preflight guarantees `.kuro/` did not exist before this call, so on any
/// write error the just-created `.kuro/` is removed again -- init either
/// completes or leaves no trace.
fn write_files(dir: &Path, files: &[(PathBuf, String)]) -> Result<()> {
    let created_root = dir.join(KOTO_DIR);
    let write_all = || -> std::io::Result<()> {
        for (rel, contents) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, contents)?;
        }
        Ok(())
    };
    if let Err(err) = write_all() {
        let _ = std::fs::remove_dir_all(&created_root);
        return Err(eyre!(
            "failed to write scaffold under {}: {err} (removed partial {KOTO_DIR}/)",
            dir.display()
        ));
    }
    Ok(())
}

// --- Templates ---
//
// Kept as consts next to the parse-tests that validate them. Placeholders
// (`{backend}`, `{project_desc}`, `{language_note}`) are substituted via
// plain `str::replace` in `render_files` -- five small files do not
// justify a templating engine.

/// `defaults.backend` is the project *policy* axis (`cli` | `api`), not
/// the runtime backend -- the detected CLI goes into the agent files
/// instead (see `RawAgentFile.backend`). Do not write `claude-cli` here;
/// `KotoBackend` would reject it.
const CONFIG_TEMPLATE: &str = r#"# Generated by `kuro init`. Edit freely -- this file is yours now.
version: "1"

defaults:
  backend: cli

roles:
  developer:
    agent: Developer
  reviewer:
    agent: Reviewer
"#;

const DEVELOPER_AGENT_TEMPLATE: &str = r#"# Generated by `kuro init`. Edit the role text to shape this agent.
name: Developer
title: Developer
description: Starter developer agent. Implements tasks handed to it by flows.
backend: {backend}
rules:
  - project-conventions
role: |
  You are the developer agent for {project_desc}. You implement the tasks
  that flows hand to you: read the relevant code first, make focused
  changes, and explain what you changed and why.

  Follow the project-conventions rule. Keep changes small. When a task is
  ambiguous, say what is unclear instead of guessing.
"#;

const REVIEWER_AGENT_TEMPLATE: &str = r#"# Generated by `kuro init`. Edit the role text to shape this agent.
name: Reviewer
title: Reviewer
description: Starter reviewer agent. Reviews changes produced by the developer.
backend: {backend}
rules:
  - project-conventions
role: |
  You are the reviewer agent for {project_desc}. You review changes for
  correctness, edge cases and consistency with the project-conventions
  rule. Report findings one per item with file and line references.

  Do not rubber-stamp: if you found nothing, say what you checked.
"#;

const RULE_TEMPLATE: &str = r#"# Project Conventions

Generated by `kuro init`. Every agent that lists this rule reads it before
working -- replace the TODO markers with your team's actual conventions.
{language_note}
- TODO: build and test commands (how agents verify their work)
- TODO: code style and formatting rules
- TODO: branch, commit and PR conventions
- TODO: what agents must never do in this repository
"#;

/// Linear starter flow using the `role:` indirection real flows use.
///
/// The flow declares its own `roles:` default instead of relying only on
/// the project-level binding in the generated config: `kuro context`
/// enumerates flows standalone (no project roles), so a flow that
/// resolves its role exclusively through `.kuro/config.yaml` would be
/// dropped from the inventory as invalid. Flow-level roles win over
/// project-level ones, so both declarations agree by construction here.
const HELLO_FLOW_TEMPLATE: &str = r#"# Generated by `kuro init`. Smoke-tests the setup: `kuro run hello`.
version: "1"
name: hello
# The runner requires a flow-level prompt (or `-t`) even when every step
# carries its own task -- without this line `kuro run hello` errors out.
prompt: |
  Confirm the kuro setup works.
roles:
  developer:
    default: Developer
flow:
  hello:
    role: developer
    task: |
      Reply with a short greeting confirming the kuro setup works. State
      your agent name and one sentence about your role. Do not read or
      modify any files.
    print_output: true
"#;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        Defaults, Flow, load_agent_file, load_flow_any_from_str, load_flow_from_str_with_project,
    };
    use crate::koto_config::{KotoBackend, KotoConfig};

    fn write_rendered(dir: &Path, kind: ProjectKind, backend: Backend) {
        write_files(dir, &render_files(kind, backend)).expect("write scaffold");
    }

    // --- Generated output through the production loaders ---
    // These are the load-bearing tests: schema drift in koto_config.rs or
    // config.rs must break here, not on an adopter's machine.

    #[test]
    fn generated_config_parses_with_both_roles() {
        let dir = tempfile::tempdir().unwrap();
        write_rendered(dir.path(), ProjectKind::Rust, Backend::ClaudeCli);

        let config = KotoConfig::load_optional(dir.path())
            .expect("generated config must parse")
            .expect("config.yaml must exist");
        assert_eq!(config.version, "1");
        assert_eq!(config.default_backend, Some(KotoBackend::Cli));
        assert_eq!(config.roles["developer"].agent, "Developer");
        assert_eq!(config.roles["reviewer"].agent, "Reviewer");
    }

    #[test]
    fn generated_agents_parse_and_carry_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        write_rendered(dir.path(), ProjectKind::Rust, Backend::Ollama);
        let koto_dir = dir.path().join(KOTO_DIR);

        for id in ["Developer", "Reviewer"] {
            let agent = load_agent_file(&koto_dir, id, &Defaults::default(), None)
                .unwrap_or_else(|e| panic!("generated agent {id} must parse: {e}"));
            assert_eq!(agent.backend, Backend::Ollama, "agent {id}");
            assert!(
                agent.role.contains("Rust"),
                "agent {id} must name the language"
            );
            assert_eq!(agent.rules, vec!["project-conventions".to_string()]);
        }
    }

    #[test]
    fn generated_hello_flow_parses_standalone_and_with_project_roles() {
        let files = render_files(ProjectKind::Generic, Backend::ClaudeCli);
        let hello = &files
            .iter()
            .find(|(p, _)| p.ends_with("hello.yaml"))
            .expect("hello.yaml rendered")
            .1;

        // Standalone parse -- the contract `kuro context` relies on: flow
        // enumeration passes no project roles, so the flow must resolve
        // its role from its own `roles:` block.
        let flow = match load_flow_any_from_str(hello).expect("generated hello flow must parse") {
            Flow::Linear(flow) => flow,
            Flow::Graph(_) => panic!("hello flow must be linear"),
        };
        assert_eq!(flow.name, "hello");
        assert_eq!(flow.steps.len(), 1);
        assert_eq!(flow.steps[0].agent, "Developer");
        assert!(flow.steps[0].print_output);

        // Runtime parse (`kuro run hello`) additionally passes the
        // project roles from the generated config -- the flow-level
        // default and the project-level binding must agree.
        let project_roles: HashMap<String, String> =
            [("developer".to_string(), "Developer".to_string())].into();
        let runtime = load_flow_from_str_with_project(hello, &HashMap::new(), &project_roles)
            .expect("generated hello flow must parse with project roles");
        assert_eq!(runtime.steps[0].agent, "Developer");
    }

    #[test]
    fn generic_scaffold_names_no_language() {
        for (_, contents) in render_files(ProjectKind::Generic, Backend::ClaudeCli) {
            for language in ["Rust", "Python", "Go", "JavaScript", "LaTeX"] {
                assert!(
                    !contents.contains(language),
                    "generic template must not mention {language}: {contents}"
                );
            }
        }
    }

    // --- detect_project ---

    #[test]
    fn detect_project_table() {
        let cases: Vec<(Vec<&str>, ProjectKind)> = vec![
            (vec![], ProjectKind::Generic),
            (vec!["Cargo.toml"], ProjectKind::Rust),
            (vec!["pyproject.toml"], ProjectKind::Python),
            (vec!["setup.py"], ProjectKind::Python),
            (vec!["go.mod"], ProjectKind::Go),
            (vec!["package.json"], ProjectKind::Web),
            (vec!["paper.tex"], ProjectKind::Tex),
            // Priority: first marker in issue enumeration order wins.
            (vec!["Cargo.toml", "package.json"], ProjectKind::Rust),
            (vec!["go.mod", "package.json"], ProjectKind::Go),
            (vec!["package.json", "paper.tex"], ProjectKind::Web),
            (vec!["README.md"], ProjectKind::Generic),
        ];
        for (markers, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            for marker in &markers {
                std::fs::write(dir.path().join(marker), "").unwrap();
            }
            assert_eq!(detect_project(dir.path()), expected, "markers: {markers:?}");
        }
    }

    #[test]
    fn detect_project_ignores_nested_tex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/paper.tex"), "").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectKind::Generic);
    }

    // --- detect_backend ---

    #[cfg(unix)]
    fn fake_bin_dir(binaries: &[&str]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for binary in binaries {
            let path = dir.path().join(binary);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[cfg(unix)]
    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_finds_each_binary() {
        for (expected, binary) in [
            (Backend::ClaudeCli, "claude"),
            (Backend::Codex, "codex"),
            (Backend::Ollama, "ollama"),
        ] {
            let bin = fake_bin_dir(&[binary]);
            let detected = detect_backend(Some(bin.path().as_os_str()), no_env);
            assert_eq!(detected, Some(expected), "binary: {binary}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_prefers_claude_over_others() {
        let bin = fake_bin_dir(&["ollama", "codex", "claude"]);
        let detected = detect_backend(Some(bin.path().as_os_str()), no_env);
        assert_eq!(detected, Some(Backend::ClaudeCli));
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_empty_path_is_none() {
        assert_eq!(detect_backend(Some(OsStr::new("")), no_env), None);
        assert_eq!(detect_backend(None, no_env), None);
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_skips_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude"), "").unwrap(); // 0o644
        assert_eq!(detect_backend(Some(dir.path().as_os_str()), no_env), None);
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_honors_env_override_path() {
        let bin = fake_bin_dir(&["my-codex"]);
        let override_path = bin.path().join("my-codex").into_os_string();
        let detected = detect_backend(Some(OsStr::new("")), |key| {
            (key == "CODEX_CLI_PATH").then(|| override_path.clone())
        });
        assert_eq!(detected, Some(Backend::Codex));
    }

    #[cfg(unix)]
    #[test]
    fn detect_backend_broken_override_does_not_claim_backend() {
        // CLAUDE_CLI_PATH points nowhere: the executor would fail with it,
        // so init must not report claude as available -- but a later
        // candidate found on PATH still wins.
        let bin = fake_bin_dir(&["ollama"]);
        let detected = detect_backend(Some(bin.path().as_os_str()), |key| {
            (key == "CLAUDE_CLI_PATH").then(|| OsString::from("/nonexistent/claude"))
        });
        assert_eq!(detected, Some(Backend::Ollama));
    }

    // --- preflight ---

    #[test]
    fn preflight_reports_each_existing_location() {
        let cases: Vec<(Vec<&str>, Vec<&str>)> = vec![
            (vec![], vec![]),
            (vec![".kuro"], vec![".kuro"]),
            (vec![".koto"], vec![".koto"]),
            (vec!["koto.yaml"], vec!["koto.yaml"]),
            (vec![".kuro", "koto.yaml"], vec![".kuro", "koto.yaml"]),
            (
                vec![".kuro", ".koto", "koto.yaml"],
                vec![".kuro", ".koto", "koto.yaml"],
            ),
        ];
        for (present, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            for path in &present {
                if path.ends_with(".yaml") {
                    std::fs::write(dir.path().join(path), "version: \"1\"\n").unwrap();
                } else {
                    std::fs::create_dir(dir.path().join(path)).unwrap();
                }
            }
            let result = preflight(dir.path());
            if expected.is_empty() {
                assert!(result.is_ok(), "present: {present:?}");
            } else {
                assert_eq!(result.unwrap_err(), expected, "present: {present:?}");
            }
        }
    }

    // --- write_files atomicity ---

    #[cfg(unix)]
    #[test]
    fn write_failure_removes_created_kuro_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = render_files(ProjectKind::Generic, Backend::ClaudeCli);

        // Make the first write succeed, then break the world: read-only
        // .kuro/ so the agents/ subdir cannot be created.
        std::fs::create_dir(dir.path().join(KOTO_DIR)).unwrap();
        std::fs::set_permissions(
            dir.path().join(KOTO_DIR),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let result = write_files(dir.path(), &files);
        assert!(result.is_err());
        // Cleanup removed the (empty, read-only) .kuro/ again -- removing
        // it needs write permission on the parent tempdir, not on .kuro/
        // itself, so the read-only bit does not block the rollback.
        assert!(
            !dir.path().join(KOTO_DIR).exists(),
            "failed init must leave no {KOTO_DIR}/ behind"
        );
    }
}

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    #[error("failed to read skills lock file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse skills lock: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("skill '{name}' directory not found at {path} (hint: run `koto pull` to fetch skills)")]
    SkillNotFound { name: String, path: String },

    #[error("no .md files found in skill directory '{0}'")]
    EmptySkill(String),

    #[error("git command failed: {0}")]
    Git(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsLock {
    #[serde(default)]
    pub skills: HashMap<String, SkillEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillEntry {
    pub source: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[allow(dead_code)]
    pub integrity: Option<String>,
}

/// Load the skills lock file from `.kuro/skills.lock`.
pub fn load_skills_lock(path: &Path) -> Result<SkillsLock, SkillsError> {
    let contents = std::fs::read_to_string(path)?;
    let lock: SkillsLock = serde_yaml::from_str(&contents)?;
    Ok(lock)
}

/// Resolve a skill by name: read all .md files from its directory and concatenate them.
pub fn resolve_skill(name: &str, skills_dir: &Path) -> Result<String, SkillsError> {
    let skill_path = skills_dir.join(name);

    if !skill_path.exists() || !skill_path.is_dir() {
        return Err(SkillsError::SkillNotFound {
            name: name.to_string(),
            path: skill_path.display().to_string(),
        });
    }

    let mut md_files: Vec<_> = std::fs::read_dir(&skill_path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
        .collect();

    if md_files.is_empty() {
        return Err(SkillsError::EmptySkill(name.to_string()));
    }

    // Sort for deterministic output
    md_files.sort_by_key(|e| e.file_name());

    let mut parts = Vec::new();
    for entry in &md_files {
        let content = std::fs::read_to_string(entry.path())?;
        parts.push(content);
    }

    Ok(parts.join("\n\n"))
}

/// Load skills content for all agents that reference them.
/// Returns a map from skill name -> concatenated content.
pub fn load_skills_for_agents(
    skills: &[String],
    skills_dir: &Path,
) -> Result<HashMap<String, String>, SkillsError> {
    let mut cache: HashMap<String, String> = HashMap::new();

    for skill_name in skills {
        if cache.contains_key(skill_name) {
            continue;
        }
        let content = resolve_skill(skill_name, skills_dir)?;
        cache.insert(skill_name.clone(), content);
    }

    Ok(cache)
}

/// Pull skills from remote sources defined in the lock file.
pub fn pull_skills(lock: &SkillsLock, skills_dir: &Path) -> Result<(), SkillsError> {
    for (name, entry) in &lock.skills {
        pull_single_skill(name, entry, skills_dir)?;
    }
    Ok(())
}

fn pull_single_skill(name: &str, entry: &SkillEntry, skills_dir: &Path) -> Result<(), SkillsError> {
    // Parse source: github.com/<owner>/<repo>/skills/<path>
    let (repo_url, subpath) = parse_source(&entry.source)?;

    let temp_dir = tempfile::tempdir().map_err(SkillsError::Io)?;
    let temp_path = temp_dir.path();

    // Clone at the pinned ref
    let output = std::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            &entry.git_ref,
            &repo_url,
            &temp_path.display().to_string(),
        ])
        .output()
        .map_err(|e| SkillsError::Git(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SkillsError::Git(format!(
            "git clone failed for skill '{name}': {stderr}"
        )));
    }

    // Copy the subdirectory to skills_dir/<name>
    let source_path = temp_path.join(&subpath);
    if !source_path.exists() {
        return Err(SkillsError::Git(format!(
            "path '{subpath}' not found in repo for skill '{name}'"
        )));
    }

    let dest_path = skills_dir.join(name);
    if dest_path.exists() {
        std::fs::remove_dir_all(&dest_path)?;
    }
    copy_dir_recursive(&source_path, &dest_path)?;

    Ok(())
}

/// Parse a source string like "github.com/owner/repo/path/to/skill" into
/// (https clone URL, subdirectory path).
fn parse_source(source: &str) -> Result<(String, String), SkillsError> {
    let parts: Vec<&str> = source.splitn(4, '/').collect();
    if parts.len() < 3 {
        return Err(SkillsError::Git(format!(
            "invalid source format: '{source}' (expected: github.com/owner/repo[/path])"
        )));
    }

    let host = parts[0];
    let owner = parts[1];
    let repo = parts[2];
    let subpath = if parts.len() > 3 { parts[3] } else { "" };

    let repo_url = format!("https://{host}/{owner}/{repo}.git");
    Ok((repo_url, subpath.to_string()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if entry_path.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            std::fs::copy(&entry_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Collect all unique skill names referenced by agents.
pub fn collect_skill_names(agents: &[crate::config::Agent]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for agent in agents {
        for skill in &agent.skills {
            if !names.contains(skill) {
                names.push(skill.clone());
            }
        }
    }
    names
}

/// Check that all referenced skill directories exist.
/// Returns the list of missing skills.
pub fn check_skills_available(skill_names: &[String], skills_dir: &Path) -> Vec<String> {
    skill_names
        .iter()
        .filter(|name| {
            let path = skills_dir.join(name);
            !path.exists() || !path.is_dir()
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_full_path() {
        let (url, path) =
            parse_source("github.com/actionbook/rust-skills/skills/domain-cli").unwrap();
        assert_eq!(url, "https://github.com/actionbook/rust-skills.git");
        assert_eq!(path, "skills/domain-cli");
    }

    #[test]
    fn parse_source_repo_only() {
        let (url, path) = parse_source("github.com/actionbook/rust-skills").unwrap();
        assert_eq!(url, "https://github.com/actionbook/rust-skills.git");
        assert_eq!(path, "");
    }

    #[test]
    fn parse_source_invalid() {
        assert!(parse_source("invalid").is_err());
    }

    #[test]
    fn resolve_skill_reads_md_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("01-intro.md"), "# Intro\nHello").unwrap();
        std::fs::write(skill_dir.join("02-details.md"), "# Details\nWorld").unwrap();
        std::fs::write(skill_dir.join("notes.txt"), "ignored").unwrap();

        let content = resolve_skill("my-skill", dir.path()).unwrap();
        assert!(content.contains("# Intro"));
        assert!(content.contains("# Details"));
        assert!(!content.contains("ignored"));
    }

    #[test]
    fn resolve_skill_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_skill("nonexistent", dir.path()).unwrap_err();
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains("koto pull"));
    }

    #[test]
    fn resolve_skill_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("empty-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let err = resolve_skill("empty-skill", dir.path()).unwrap_err();
        assert!(err.to_string().contains("no .md files"));
    }

    #[test]
    fn load_skills_lock_parses() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("skills.lock");
        std::fs::write(
            &lock_path,
            r#"
skills:
  domain-cli:
    source: github.com/actionbook/rust-skills/skills/domain-cli
    ref: v1.2.0
    integrity: sha256-abc123
  error-handling:
    source: github.com/actionbook/rust-skills/skills/m06-error-handling
    ref: v1.2.0
"#,
        )
        .unwrap();

        let lock = load_skills_lock(&lock_path).unwrap();
        assert_eq!(lock.skills.len(), 2);
        assert_eq!(lock.skills["domain-cli"].git_ref, "v1.2.0");
        assert_eq!(
            lock.skills["domain-cli"].integrity.as_deref(),
            Some("sha256-abc123")
        );
        assert!(lock.skills["error-handling"].integrity.is_none());
    }

    #[test]
    fn check_skills_available_finds_missing() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("exists");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let names = vec!["exists".to_string(), "missing".to_string()];
        let missing = check_skills_available(&names, dir.path());
        assert_eq!(missing, vec!["missing"]);
    }

    #[test]
    fn load_skills_for_agents_caches() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("guide.md"), "skill content").unwrap();

        let names = vec!["my-skill".to_string(), "my-skill".to_string()];
        let cache = load_skills_for_agents(&names, dir.path()).unwrap();
        assert_eq!(cache.len(), 1);
        assert!(cache["my-skill"].contains("skill content"));
    }
}

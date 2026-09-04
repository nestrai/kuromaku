//! Guard against maintainer-specific absolute paths in tracked
//! user-facing files (issue #398).
//!
//! A public clone must not reference the maintainer's filesystem. This
//! locks the issue's grep criterion in CI: no tracked configuration or
//! documentation may contain a home-directory-specific path.
//!
//! Scope: user-facing trees only (`.kuro/`, `seeds/`, `docs/`, root-level
//! markdown). Source code is covered implicitly -- paths there would be
//! test fixtures, and the patterns below never belong in fixtures either,
//! but `src/` legitimately documents `~/`-relative *runtime* paths like
//! `~/.kuro/stacks`, so the check stays on the config/docs surface where
//! the criterion lives.

use std::path::{Path, PathBuf};

/// Patterns that mark a maintainer-specific path. `~/` alone is allowed
/// (runtime paths like `~/.kuro/stacks` are user-relative by design);
/// `~/code/` is the maintainer's checkout convention and is not.
const FORBIDDEN: [&str; 3] = ["/home/charemma", "/Users/", "~/code/"];

/// File extensions considered user-facing text. Binary or generated
/// assets (e.g. `.html` reference cards) are skipped.
const TEXT_EXTENSIONS: [&str; 3] = ["md", "yaml", "yml"];

fn collect_files(dir: &Path, acc: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, acc);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| TEXT_EXTENSIONS.contains(&e))
        {
            acc.push(path);
        }
    }
}

#[test]
fn tracked_user_facing_files_contain_no_maintainer_paths() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in [".kuro", "seeds", "docs"] {
        collect_files(&root.join(dir), &mut files);
    }
    for name in ["README.md", "AGENTS.md", "CLAUDE.md"] {
        let path = root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    assert!(!files.is_empty(), "expected user-facing files to scan");

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let contents = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (lineno, line) in contents.lines().enumerate() {
            for pattern in FORBIDDEN {
                if line.contains(pattern) {
                    violations.push(format!(
                        "{}:{}: contains {pattern:?}: {}",
                        file.strip_prefix(root).unwrap_or(file).display(),
                        lineno + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "maintainer-specific paths in tracked user-facing files:\n{}",
        violations.join("\n")
    );
}

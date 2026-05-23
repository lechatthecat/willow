use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// project.toml — project manifest.
///
/// ```toml
/// [project]
/// name    = "my_project"
/// version = "0.1.0"
/// entry   = "src/main.wi"   # optional; defaults to "src/main.wi"
/// ```
#[derive(Debug, Deserialize)]
pub struct ProjectManifest {
    pub project: ProjectSection,
}

#[derive(Debug, Deserialize)]
pub struct ProjectSection {
    pub name: String,
    pub version: String,
    /// Entry-point source file, relative to the project root.
    /// Defaults to `src/main.wi` when omitted.
    pub entry: Option<String>,
}

impl ProjectManifest {
    pub fn load(manifest_path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(manifest_path)
            .with_context(|| format!("cannot read {}", manifest_path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("invalid project.toml at {}", manifest_path.display()))
    }

    /// Absolute path of the entry-point source file.
    pub fn entry_point(&self, project_root: &Path) -> PathBuf {
        let rel = self.project.entry.as_deref().unwrap_or("src/main.wi");
        project_root.join(rel)
    }
}

/// Locate a `project.toml` by walking up from `start_dir`.
/// Returns `(manifest, project_root)` if found.
pub fn find_project_manifest(start_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join("project.toml");
        if candidate.exists() {
            return Some((candidate, dir));
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

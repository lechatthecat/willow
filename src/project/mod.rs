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
        let manifest: Self = toml::from_str(&text)
            .with_context(|| format!("invalid project.toml at {}", manifest_path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Absolute path of the entry-point source file.
    pub fn entry_point(&self, project_root: &Path) -> PathBuf {
        let rel = self.project.entry.as_deref().unwrap_or("src/main.wi");
        project_root.join(rel)
    }

    fn validate(&self) -> Result<()> {
        if is_reserved_package_name(&self.project.name) {
            anyhow::bail!(
                "error[E2005]: package name `{}` is reserved\nhelp: `std` is the standard library namespace and cannot be used as a package name",
                self.project.name
            );
        }
        Ok(())
    }
}

fn is_reserved_package_name(name: &str) -> bool {
    name == "std" || name.starts_with("std.") || name.starts_with("std::")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_manifest(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "willow_project_manifest_{}_{}.toml",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::write(&path, contents).expect("write temp manifest");
        path
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos()
    }

    #[test]
    fn rejects_reserved_std_project_name() {
        let path = temp_manifest(
            r#"
[project]
name = "std"
version = "0.1.0"
"#,
        );
        let err = ProjectManifest::load(&path)
            .expect_err("std project name should be rejected")
            .to_string();
        let _ = std::fs::remove_file(path);

        assert!(err.contains("error[E2005]"), "err: {err}");
        assert!(err.contains("package name `std` is reserved"), "err: {err}");
    }

    #[test]
    fn rejects_reserved_std_subnamespace_project_name() {
        let path = temp_manifest(
            r#"
[project]
name = "std.collections"
version = "0.1.0"
"#,
        );
        let err = ProjectManifest::load(&path)
            .expect_err("std.* project name should be rejected")
            .to_string();
        let _ = std::fs::remove_file(path);

        assert!(err.contains("error[E2005]"), "err: {err}");
        assert!(
            err.contains("package name `std.collections` is reserved"),
            "err: {err}"
        );
    }

    #[test]
    fn accepts_non_reserved_project_name() {
        let path = temp_manifest(
            r#"
[project]
name = "stdlib_helpers"
version = "0.1.0"
"#,
        );
        let manifest = ProjectManifest::load(&path).expect("non-reserved name should load");
        let _ = std::fs::remove_file(path);

        assert_eq!(manifest.project.name, "stdlib_helpers");
    }
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod dependency;
pub use dependency::{CanonicalGitUrl, DependencySource, GitSelector};

use anyhow::Result;
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
    #[serde(default)]
    pub dependencies: BTreeMap<String, DependencySource>,
    pub willow: Option<WillowSection>,
}

#[derive(Debug, Deserialize)]
pub struct WillowSection {
    #[serde(rename = "manifest-version")]
    pub manifest_version: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("error[{code}]: manifest_invalid: {0}", code = crate::diagnostics::ErrorCode::E2012.as_str())]
    Invalid(String),
    #[error("error[{code}]: dependency_alias_invalid: `{0}`", code = crate::diagnostics::ErrorCode::E2013.as_str())]
    InvalidAlias(String),
    #[error("error[{code}]: unsupported_manifest_version: found {found}, supported {supported}", code = crate::diagnostics::ErrorCode::E2014.as_str())]
    UnsupportedVersion { found: u64, supported: u64 },
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
        let text = std::fs::read_to_string(manifest_path).map_err(|e| {
            ManifestError::Invalid(format!("cannot read {}: {e}", manifest_path.display()))
        })?;
        let manifest: Self = toml::from_str(&text)
            .map_err(|e| ManifestError::Invalid(format!("{}: {e}", manifest_path.display())))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Dependencies must opt into the versioned manifest format.
    pub fn load_dependency(manifest_path: &Path) -> Result<Self> {
        let manifest = Self::load(manifest_path)?;
        if manifest.willow.is_none() {
            return Err(ManifestError::Invalid(
                "dependency requires [willow] manifest-version = 1".into(),
            )
            .into());
        }
        Ok(manifest)
    }

    /// Absolute path of the entry-point source file.
    pub fn entry_point(&self, project_root: &Path) -> PathBuf {
        let rel = self.project.entry.as_deref().unwrap_or("src/main.wi");
        project_root.join(rel)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.project.name.trim().is_empty() {
            return Err(ManifestError::Invalid("project.name must not be empty".into()).into());
        }
        if let Some(marker) = &self.willow
            && marker.manifest_version != 1
        {
            return Err(ManifestError::UnsupportedVersion {
                found: marker.manifest_version,
                supported: 1,
            }
            .into());
        }
        semver::Version::parse(&self.project.version)
            .map_err(|e| ManifestError::Invalid(format!("project.version: {e}")))?;
        for alias in self.dependencies.keys() {
            let mut bytes = alias.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || alias == "std"
            {
                return Err(ManifestError::InvalidAlias(alias.clone()).into());
            }
        }
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
    let mut dir = std::fs::canonicalize(start_dir).ok()?;
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

    pub(super) fn temp_manifest(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "willow_project_manifest_{}_{}.toml",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&path, contents).expect("write temp manifest");
        path
    }

    fn unique_suffix() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        format!("{time}-{sequence}")
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

#[cfg(test)]
mod dependency_tests {
    use super::*;

    fn load(extra: &str, dependency: bool) -> Result<ProjectManifest> {
        let path = tests::temp_manifest(&format!(
            "[project]\nname = \"demo\"\nversion = \"1.2.3\"\n{extra}"
        ));
        let result = if dependency {
            ProjectManifest::load_dependency(&path)
        } else {
            ProjectManifest::load(&path)
        };
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn canonical_sources_preserve_transport() {
        assert_eq!(
            CanonicalGitUrl::new("file:///tmp/repo.git").as_str(),
            "file:///tmp/repo.git"
        );
        assert_ne!(
            CanonicalGitUrl::new("file:///tmp/repo.git"),
            CanonicalGitUrl::new("file:///tmp/repo")
        );
        assert_eq!(
            CanonicalGitUrl::new("https://github.com/a/b"),
            CanonicalGitUrl::new("https://github.com/a/b.git")
        );
        assert_ne!(
            CanonicalGitUrl::new("https://github.com/a/b"),
            CanonicalGitUrl::new("ssh://git@github.com/a/b.git")
        );
    }

    #[test]
    fn growing_dependency_tables() {
        for count in [1, 16, 256, 1024] {
            let mut source = String::from("[dependencies]\n");
            for i in 0..count {
                use std::fmt::Write;
                writeln!(
                    source,
                    "dep_{i:04} = {{ git = 'https://example.org/shared.git', version = '^1.4' }}"
                )
                .unwrap();
            }
            dependency::NORMALIZATIONS.with(|count| count.set(0));
            let m = load(&source, false).unwrap();
            assert_eq!(dependency::NORMALIZATIONS.with(|count| count.get()), count);
            assert_eq!(m.dependencies.len(), count);
            assert!(m.dependencies.values().all(|dep| matches!(dep, DependencySource::Git { url, .. } if url.as_str() == "https://example.org/shared")));
        }
    }

    #[test]
    fn sources_and_selectors() {
        for spec in [
            "path = './lib'",
            "git = 'https://example.org/lib.git'",
            "git = 'https://example.org/lib.git', version = '^1.4'",
            "git = 'x', rev = 'abc'",
            "git = 'x', branch = 'main'",
            "git = 'x', tag = 'v1.0.0'",
        ] {
            let m = load(&format!("[dependencies]\nlib = {{ {spec} }}"), false).unwrap();
            assert_eq!(m.dependencies.len(), 1);
        }
        let m = load(
            "[dependencies]\nlib = { git = 'x', version = '^1.4' }",
            false,
        )
        .unwrap();
        assert!(
            matches!(&m.dependencies["lib"], DependencySource::Git { selector: GitSelector::Version(req), .. } if req.matches(&semver::Version::new(1, 5, 0)))
        );
    }

    #[test]
    fn invalid_dependencies() {
        for spec in [
            "path = 'x', git = 'x'",
            "path = 'x', version = '1'",
            "git = 'x', version = 'bad'",
            "git = 'x', rev = ''",
            "path = ''",
            "git = ''",
            "",
            "git = 'x', typo = 'x'",
        ] {
            assert!(
                load(&format!("[dependencies]\nlib = {{ {spec} }}"), false).is_err(),
                "{spec}"
            );
        }
        let selectors = ["version", "rev", "branch", "tag"];
        for (i, a) in selectors.iter().enumerate() {
            for b in &selectors[i + 1..] {
                assert!(
                    load(
                        &format!("[dependencies]\nlib = {{ git = 'x', {a} = '1', {b} = '1' }}"),
                        false
                    )
                    .is_err()
                );
            }
        }
        for alias in ["std", "a-b", "1abc", "", "日本"] {
            let err = load(
                &format!("[dependencies]\n'{alias}' = {{ path = 'x' }}"),
                false,
            )
            .unwrap_err();
            assert!(matches!(
                err.downcast_ref::<ManifestError>(),
                Some(ManifestError::InvalidAlias(_))
            ));
        }
        assert!(
            load(
                "[dependencies]\na = { path = 'x' }\na = { path = 'y' }",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn marker_and_legacy_entry() {
        assert_eq!(
            load("", false).unwrap().entry_point(Path::new("/tmp")),
            Path::new("/tmp/src/main.wi")
        );
        assert!(load("", true).is_err());
        assert!(load("[willow]\nmanifest-version = 1", true).is_ok());
        let err = load("[willow]\nmanifest-version = 7", false).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ManifestError>(),
            Some(ManifestError::UnsupportedVersion {
                found: 7,
                supported: 1
            })
        ));
        assert!(load("[willow]\nmanifest-version = '1'", false).is_err());
    }

    #[test]
    fn version_ranges() {
        for (req, yes, no) in [
            ("=1.4.2", "1.4.2", "1.4.3"),
            ("^1.4", "1.9.0", "2.0.0"),
            ("~1.4", "1.4.9", "1.5.0"),
            (">=1.2,<2", "1.8.0", "2.0.0"),
            ("1.4", "1.8.0", "2.0.0"),
            ("0.2", "0.2.9", "0.3.0"),
            ("*", "1.0.0", "1.0.0-alpha.1"),
            ("^1.0.0-alpha.1", "1.0.0-alpha.2", "1.1.0-alpha.1"),
        ] {
            let m = load(
                &format!("[dependencies]\nlib = {{ git = 'x', version = '{req}' }}"),
                false,
            )
            .unwrap();
            let DependencySource::Git {
                selector: GitSelector::Version(range),
                ..
            } = &m.dependencies["lib"]
            else {
                panic!()
            };
            assert!(range.matches(&semver::Version::parse(yes).unwrap()));
            assert!(!range.matches(&semver::Version::parse(no).unwrap()));
        }
    }

    #[test]
    fn invalid_project_version_and_missing_file() {
        let path = tests::temp_manifest("[project]\nname='demo'\nversion='1.2'");
        assert!(ProjectManifest::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(ProjectManifest::load(&path).is_err());
    }
}

//! `[rust-dependencies]` and `[rust]` manifest configuration.
//!
//! Rust crates are an external native ecosystem, not Willow packages: nothing
//! here reaches the Willow package resolver, and version resolution stays with
//! Cargo. Deserialization only checks the shape of the table; every semantic
//! rule is enforced by [`RustDependencySpec::normalize`] and
//! [`RustSection::relative_bridge`] so failures carry the
//! `rust_dependency_invalid` / `rust_bridge_missing` diagnostics instead of a
//! generic manifest parse error.
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, de::value::MapAccessDeserializer};

use super::{CanonicalGitUrl, ManifestError};

/// Raw `[rust-dependencies]` entry, accepted as either the crates.io short form
/// (`regex = "1.12"`) or the expanded table form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RustDependencySpec {
    pub version: Option<String>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub tag: Option<String>,
    /// Parsed only so that the rejection can name `branch` instead of reporting
    /// it as an unknown key.
    pub branch: Option<String>,
    pub path: Option<String>,
    pub features: Vec<String>,
    pub default_features: Option<bool>,
    /// v1 does not bind Rust dependencies to a Willow feature system (spec §7.6).
    pub optional: Option<bool>,
}

impl<'de> Deserialize<'de> for RustDependencySpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SpecVisitor;
        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = RustDependencySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a version requirement string or a Rust dependency table")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(RustDependencySpec {
                    version: Some(value.to_owned()),
                    ..Default::default()
                })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                RustDependencyTable::deserialize(MapAccessDeserializer::new(map))
                    .map(RustDependencySpec::from)
            }
        }
        deserializer.deserialize_any(SpecVisitor)
    }
}

/// Normalized dependency: exactly one source, features already separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustDependency {
    pub source: RustDependencySource,
    pub features: Vec<String>,
    pub default_features: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustDependencySource {
    /// crates.io. The requirement text is handed to Cargo verbatim; it is parsed
    /// here only so a typo fails at manifest load instead of inside Cargo.
    Registry { version: String },
    Git {
        url: CanonicalGitUrl,
        selector: RustGitSelector,
    },
    /// Relative to the root `project.toml`. Leaving the project root is normal
    /// Cargo usage (`path = "../my-native"`, spec §7.3) and stays allowed.
    Path { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustGitSelector {
    /// `git = "..."` with neither `rev` nor `tag`: Cargo picks the default branch.
    Default,
    Revision(String),
    Tag(String),
}

impl RustDependencySpec {
    /// Apply every `[rust-dependencies]` rule and produce the normalized form.
    pub fn normalize(&self, alias: &str) -> Result<RustDependency, ManifestError> {
        let invalid =
            |message: String| ManifestError::RustDependencyInvalid(format!("`{alias}`: {message}"));
        validate_rust_alias(alias)?;
        if self.optional == Some(true) {
            return Err(invalid(
                "`optional = true` is not supported in v1; remove the key".into(),
            ));
        }
        if self.branch.is_some() {
            return Err(invalid(
                "`branch` is not supported in v1; use `rev` or `tag`".into(),
            ));
        }
        for (key, value) in [
            ("version", &self.version),
            ("git", &self.git),
            ("rev", &self.rev),
            ("tag", &self.tag),
            ("path", &self.path),
        ] {
            if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                return Err(invalid(format!("`{key}` must not be empty")));
            }
        }
        if self
            .features
            .iter()
            .any(|feature| feature.trim().is_empty())
        {
            return Err(invalid("`features` must not contain empty names".into()));
        }
        let selected: Vec<&str> = [
            ("version", self.version.is_some()),
            ("git", self.git.is_some()),
            ("path", self.path.is_some()),
        ]
        .into_iter()
        .filter_map(|(key, present)| present.then_some(key))
        .collect();
        let source = match selected.as_slice() {
            [] => {
                return Err(invalid(
                    "exactly one of `version`, `git` or `path` is required".into(),
                ));
            }
            [only] => self.source(only, &invalid)?,
            many => {
                return Err(invalid(format!(
                    "`version`, `git` and `path` are mutually exclusive, found {}",
                    many.join(" and ")
                )));
            }
        };
        if !matches!(source, RustDependencySource::Git { .. })
            && (self.rev.is_some() || self.tag.is_some())
        {
            return Err(invalid("`rev` and `tag` require a `git` source".into()));
        }
        Ok(RustDependency {
            source,
            features: self.features.clone(),
            default_features: self.default_features.unwrap_or(true),
        })
    }

    fn source(
        &self,
        key: &str,
        invalid: &impl Fn(String) -> ManifestError,
    ) -> Result<RustDependencySource, ManifestError> {
        Ok(match key {
            "version" => {
                let version = self.version.clone().expect("version source");
                semver::VersionReq::parse(&version)
                    .map_err(|error| invalid(format!("`version`: {error}")))?;
                RustDependencySource::Registry { version }
            }
            "git" => {
                let selector = match (&self.rev, &self.tag) {
                    (Some(_), Some(_)) => {
                        return Err(invalid("only one of `rev` or `tag` is allowed".into()));
                    }
                    (Some(rev), None) => RustGitSelector::Revision(rev.clone()),
                    (None, Some(tag)) => RustGitSelector::Tag(tag.clone()),
                    (None, None) => RustGitSelector::Default,
                };
                RustDependencySource::Git {
                    url: CanonicalGitUrl::new(self.git.as_deref().expect("git source")),
                    selector,
                }
            }
            _ => RustDependencySource::Path {
                path: self.path.clone().expect("path source"),
            },
        })
    }
}

/// Manifest aliases are consumer-local names for a Cargo package, so they follow
/// Cargo naming (hyphens allowed) rather than Willow import-alias naming.
fn validate_rust_alias(alias: &str) -> Result<(), ManifestError> {
    let mut bytes = alias.bytes();
    let valid = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(ManifestError::RustDependencyInvalid(format!(
            "`{alias}`: alias must be a Cargo package name (letters, digits, `_`, `-`)"
        )))
    }
}

/// `[rust]` section: the single user-authored bridge module (spec §13).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustSection {
    pub bridge: String,
}

impl RustSection {
    /// Bridge path as declared, proven to stay inside the project root lexically.
    pub fn relative_bridge(&self) -> Result<PathBuf, ManifestError> {
        let invalid = |message: &str| {
            ManifestError::RustDependencyInvalid(format!(
                "[rust] bridge `{}`: {message}",
                self.bridge
            ))
        };
        if self.bridge.trim().is_empty() {
            return Err(invalid("path must not be empty"));
        }
        let path = Path::new(&self.bridge);
        let mut depth = 0usize;
        for component in path.components() {
            match component {
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                Component::ParentDir if depth > 0 => depth -= 1,
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid("path must stay inside the project root"));
                }
            }
        }
        if depth == 0 {
            return Err(invalid("path must name a file"));
        }
        Ok(path.to_owned())
    }

    /// Absolute bridge path, checked against the real file system so that a
    /// symlink cannot lead outside the project root.
    pub fn resolve_bridge(&self, project_root: &Path) -> Result<PathBuf, ManifestError> {
        let declared = project_root.join(self.relative_bridge()?);
        let resolved = std::fs::canonicalize(&declared).map_err(|error| {
            ManifestError::RustBridgeMissing(format!("{}: {error}", declared.display()))
        })?;
        if !resolved.is_file() {
            return Err(ManifestError::RustBridgeMissing(format!(
                "{}: not a file",
                declared.display()
            )));
        }
        // `project_root` may itself be a symlink; compare canonical forms.
        let root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_owned());
        if !resolved.starts_with(&root) {
            return Err(ManifestError::RustDependencyInvalid(format!(
                "[rust] bridge `{}`: path must stay inside the project root",
                self.bridge
            )));
        }
        Ok(resolved)
    }
}

/// Field-for-field mirror of the expanded table form. Kept separate from
/// [`RustDependencySpec`] so the hand-written visitor can reuse the derive.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RustDependencyTable {
    version: Option<String>,
    git: Option<String>,
    rev: Option<String>,
    tag: Option<String>,
    branch: Option<String>,
    path: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    default_features: Option<bool>,
    optional: Option<bool>,
}

impl From<RustDependencyTable> for RustDependencySpec {
    fn from(table: RustDependencyTable) -> Self {
        Self {
            version: table.version,
            git: table.git,
            rev: table.rev,
            tag: table.tag,
            branch: table.branch,
            path: table.path,
            features: table.features,
            default_features: table.default_features,
            optional: table.optional,
        }
    }
}

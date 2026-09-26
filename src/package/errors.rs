//! Typed errors provide stable machine kinds independently of human wording.
use super::{PackageError, PackageImportError};
use crate::project::ManifestError;
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct CommandError {
    pub kind: &'static str,
    pub message: String,
    pub fields: serde_json::Map<String, Value>,
}
impl CommandError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            fields: Default::default(),
        }
    }
    pub fn with_field(mut self, key: &str, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), Value::String(value.into()));
        self
    }
}
fn manifest(error: &ManifestError) -> Value {
    match error {
        ManifestError::Invalid(_) => json!({"kind":"manifest_invalid"}),
        ManifestError::InvalidAlias(alias) => {
            json!({"kind":"dependency_alias_invalid", "alias":alias})
        }
        ManifestError::UnsupportedVersion { found, supported } => {
            json!({"kind":"unsupported_manifest_version", "found":found, "supported":supported})
        }
        ManifestError::RustDependencyInvalid(detail) => {
            json!({"kind":"rust_dependency_invalid", "detail":detail})
        }
        ManifestError::RustBridgeMissing(detail) => {
            json!({"kind":"rust_bridge_missing", "detail":detail})
        }
    }
}
impl PackageError {
    pub fn machine_error(&self) -> Value {
        match self {
            Self::CacheMissingOffline(_) => json!({"kind":"cache_missing_offline"}),
            Self::CacheChecksumMismatch(_) => json!({"kind":"cache_checksum_mismatch"}),
            Self::Cache(_) => json!({"kind":"package_cache"}),
            Self::SourceUnreachable { url, .. } => {
                json!({"kind":"source_unreachable", "source":url})
            }
            Self::GitRevisionNotFound { url, revision } => {
                json!({"kind":"git_revision_not_found", "source":url, "revision":revision})
            }
            Self::VersionNotFound { url, requirement } => {
                json!({"kind":"version_not_found", "source":url, "requirement":requirement})
            }
            Self::VersionConflict { url, requirements } => {
                json!({"kind":"version_conflict", "source":url, "requirements":requirements})
            }
            Self::TagManifestVersionMismatch { tag, version } => {
                json!({"kind":"tag_manifest_version_mismatch", "tag":tag, "version":version})
            }
            Self::GitMaterialization(_) => json!({"kind":"git_materialization_failed"}),
            Self::NotFound { path, .. } => {
                json!({"kind":"source_unreachable", "path":path.to_string_lossy()})
            }
            Self::NotDirectory(path) => {
                json!({"kind":"source_unreachable", "path":path.to_string_lossy()})
            }
            Self::ManifestMissing(path) | Self::NotWillowPackage(path) => {
                json!({"kind":"not_willow_package", "path":path.to_string_lossy()})
            }
            Self::ManifestInvalid { path, source } => {
                let mut value = source
                    .chain()
                    .find_map(|e| e.downcast_ref::<ManifestError>())
                    .map(manifest)
                    .unwrap_or(json!({"kind":"manifest_invalid"}));
                value["path"] = json!(path.to_string_lossy());
                value
            }
            Self::SourceMissing(path) => {
                json!({"kind":"source_layout_invalid", "path":path.to_string_lossy()})
            }
            Self::PathEscape { root, path } => {
                json!({"kind":"package_path_escape", "root":root.to_string_lossy(), "path":path.to_string_lossy()})
            }
            Self::Cycle(paths) => {
                json!({"kind":"package_dependency_cycle", "paths":paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>()})
            }
            Self::UnsupportedSource { root, alias } => {
                json!({"kind":"package_source_unsupported", "root":root.to_string_lossy(), "alias":alias})
            }
            Self::VersionUnavailable(_) => json!({"kind":"version_not_found"}),
            Self::RevisionMismatch(path) => {
                json!({"kind":"package_revision_mismatch", "path":path.to_string_lossy()})
            }
            Self::TooManyPackages => json!({"kind":"package_count_overflow"}),
        }
    }
}
impl PackageImportError {
    pub fn machine_error(&self) -> Value {
        match self {
            Self::AliasConflict { alias, root } => {
                json!({"kind":"dependency_alias_conflict", "alias":alias, "root":root.to_string_lossy()})
            }
            Self::ModuleRequired { alias } => {
                json!({"kind":"package_module_not_found", "alias":alias})
            }
            Self::ModuleNotFound { path, root } => {
                json!({"kind":"package_module_not_found", "path":path, "root":root.to_string_lossy()})
            }
            Self::InvalidPath(path) => json!({"kind":"package_module_not_found", "path":path}),
            Self::UnknownPackage(_) => json!({"kind":"package_graph_invalid"}),
            Self::Source(error) => error.machine_error(),
        }
    }
}
pub fn package_error_json(error: &anyhow::Error) -> Value {
    let mut detail = error
        .chain()
        .find_map(|e| {
            if let Some(e) = e.downcast_ref::<CommandError>() {
                let mut value = Value::Object(e.fields.clone());
                value["kind"] = json!(e.kind);
                return Some(value);
            }
            if let Some(e) = e.downcast_ref::<PackageImportError>() {
                return Some(e.machine_error());
            }
            if let Some(e) = e.downcast_ref::<PackageError>() {
                return Some(e.machine_error());
            }
            if let Some(e) = e.downcast_ref::<ManifestError>() {
                return Some(manifest(e));
            }
            None
        })
        .unwrap_or(json!({"kind":"package_command_failed"}));
    // Invalid internal IDs are intentionally omitted even from human detail.
    detail["message"] = if detail["kind"] == "package_graph_invalid" {
        json!("invalid package graph")
    } else {
        json!(format!("{error:#}"))
    };
    json!({"schema":1, "ok":false, "kind":"package.error", "error":detail})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_required_kinds_are_typed_and_survive_context_and_json() {
        let cases: Vec<(&str, anyhow::Error)> = vec![
            (
                "manifest_invalid",
                ManifestError::Invalid("bad".into()).into(),
            ),
            (
                "dependency_alias_invalid",
                PackageError::ManifestInvalid {
                    path: "project.toml".into(),
                    source: ManifestError::InvalidAlias("a-b".into()).into(),
                }
                .into(),
            ),
            (
                "dependency_alias_conflict",
                PackageImportError::AliasConflict {
                    alias: "a".into(),
                    root: "src".into(),
                }
                .into(),
            ),
            (
                "source_unreachable",
                PackageError::SourceUnreachable {
                    url: "git://unreachable".into(),
                    message: "failed".into(),
                }
                .into(),
            ),
            (
                "git_revision_not_found",
                PackageError::GitRevisionNotFound {
                    url: "git://repo".into(),
                    revision: "missing".into(),
                }
                .into(),
            ),
            (
                "version_not_found",
                PackageError::VersionNotFound {
                    url: "git://repo".into(),
                    requirement: "^9".into(),
                }
                .into(),
            ),
            (
                "version_conflict",
                PackageError::VersionConflict {
                    url: "git://repo".into(),
                    requirements: vec![super::super::ResolutionRequirement {
                        required_by: "app".into(),
                        requirement: "^1".into(),
                    }],
                }
                .into(),
            ),
            (
                "tag_manifest_version_mismatch",
                PackageError::TagManifestVersionMismatch {
                    tag: "v1.0.0".into(),
                    version: "2.0.0".into(),
                }
                .into(),
            ),
            (
                "lockfile_missing",
                CommandError::new("lockfile_missing", "project.lock").into(),
            ),
            (
                "lockfile_stale",
                CommandError::new("lockfile_stale", "project.lock").into(),
            ),
            (
                "cache_missing_offline",
                PackageError::CacheMissingOffline("repo".into()).into(),
            ),
            (
                "package_module_not_found",
                PackageImportError::ModuleNotFound {
                    path: "lib::missing".into(),
                    root: "src".into(),
                }
                .into(),
            ),
            (
                "package_path_escape",
                PackageError::PathEscape {
                    root: "root".into(),
                    path: "outside".into(),
                }
                .into(),
            ),
            (
                "unsupported_manifest_version",
                PackageError::ManifestInvalid {
                    path: "project.toml".into(),
                    source: ManifestError::UnsupportedVersion {
                        found: 2,
                        supported: 1,
                    }
                    .into(),
                }
                .into(),
            ),
            (
                "not_willow_package",
                PackageError::NotWillowPackage("legacy".into()).into(),
            ),
        ];
        for (kind, error) in cases {
            let value = package_error_json(&error.context("operation failed"));
            let text = serde_json::to_string(&value).unwrap();
            assert!(!text.contains('\x1b'));
            let parsed: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed["error"]["kind"], kind);
            assert_eq!(parsed["schema"], 1);
            assert_eq!(parsed["ok"], false);
        }
    }
    #[test]
    fn internal_package_indices_never_leak_in_error_messages() {
        let value = package_error_json(
            &PackageImportError::UnknownPackage(super::super::PackageId(12345)).into(),
        );
        assert!(!value.to_string().contains("12345"));
        assert!(!value.to_string().contains("PackageId"));
    }
}

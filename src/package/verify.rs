//! Package verification owns presentation and discovery; the normal frontend
//! owns parsing, import identity, type checking and concurrency checks.
use std::{collections::BTreeMap, path::Path, process::Command, sync::Arc};

use super::{PackageError, PathSource, source::contained_path};
use crate::diagnostics::{Diagnostic, DiagnosticEmitter, Severity, source_map::SourceLookup};
use serde::Serialize;

const NOTICE: &str = "Format and code validation do not guarantee security or trust.";

#[derive(Debug, Serialize)]
pub struct Verification {
    pub schema: u32,
    pub kind: &'static str,
    pub ok: bool,
    pub package: Option<PackageSummary>,
    pub checks: BTreeMap<&'static str, bool>,
    pub modules: usize,
    pub dependencies: usize,
    pub error: Option<VerificationError>,
    #[cfg(test)]
    #[serde(skip)]
    source_loads: usize,
    pub notice: &'static str,
}
#[derive(Debug, Serialize)]
pub struct PackageSummary {
    name: String,
    version: String,
    manifest_version: u64,
    source: super::PackageSourceIdentity,
    revision: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct VerificationError {
    kind: String,
    reason: String,
}
impl Verification {
    pub fn human(&self) -> String {
        let mut text = String::new();
        if let Some(p) = &self.package {
            text.push_str(&format!(
                "Willow package: {} {}\nManifest: v{}\n",
                p.name, p.version, p.manifest_version
            ));
        }
        text.push_str(&format!(
            "Modules: {}\nDependencies: {}\nStatus: {}\n",
            self.modules,
            self.dependencies,
            if self.ok { "valid" } else { "invalid" }
        ));
        if let Some(e) = &self.error {
            text.push_str(&format!("{}: {}\n", e.kind, e.reason));
        }
        text.push_str(NOTICE);
        text.push('\n');
        text
    }
}
fn failure(kind: &str, reason: impl ToString) -> VerificationError {
    VerificationError {
        kind: kind.into(),
        reason: reason.to_string(),
    }
}
fn package_error(error: PackageError) -> VerificationError {
    if matches!(&error, PackageError::NotWillowPackage(_)) {
        return failure("not_willow_package", "missing_willow_manifest_marker");
    }
    let detail = error.machine_error();
    let kind = detail["kind"]
        .as_str()
        .unwrap_or("dependency_resolution_failed");
    failure(kind, error)
}

pub fn verify_package(path: &Path) -> Verification {
    let mut report = Verification {
        schema: 1,
        kind: "package.verify",
        ok: false,
        package: None,
        #[cfg(test)]
        source_loads: 0,
        checks: [
            "manifest",
            "dependencies",
            "source_layout",
            "parse",
            "type_check",
        ]
        .into_iter()
        .map(|key| (key, false))
        .collect(),
        modules: 0,
        dependencies: 0,
        error: None,
        notice: NOTICE,
    };
    match verify(path, &mut report) {
        Ok(()) => report.ok = true,
        Err(error) => report.error = Some(error),
    }
    report
}

fn verify(path: &Path, report: &mut Verification) -> Result<(), VerificationError> {
    let source = PathSource::open(path, true).map_err(package_error)?;
    report.package = Some(PackageSummary {
        name: source.manifest.project.name.clone(),
        version: source.manifest.project.version.clone(),
        manifest_version: 1,
        source: source.identity().source,
        revision: None,
    });
    report.checks.insert("manifest", true);
    check_tags(&source)?;
    let modules = discover(&source.root)?;
    report.modules = modules.len();
    report.checks.insert("source_layout", true);
    // Fresh resolution deliberately neither reads nor writes project.lock.
    let graph =
        super::solve::resolve_source(source, Default::default(), Default::default(), false, false)
            .map_err(package_error)?;
    report.dependencies = graph.packages.len() - 1;
    super::PackageImports::new(&graph)
        .map_err(|error| failure(error.machine_error()["kind"].as_str().unwrap(), &error))?;
    report.checks.insert("dependencies", true);
    let root = graph.get(graph.root).expect("root package").source_root();
    // One synthetic entry imports every source, with distinct local bindings.
    // Shared transitive modules are cached by the ordinary module loader.
    let mut entry = String::new();
    for (index, module) in modules.keys().enumerate() {
        use std::fmt::Write;
        writeln!(entry, "import {module} as verify_module_{index};").unwrap();
    }
    entry.push_str("fn main() {}\n");
    let _nodes = crate::parser::ast::NodeIdSession::enter();
    let _queries = crate::query_stats::Session::enter();
    let inputs = crate::compiler_db::inputs::CompilerInputs::native(
        crate::CompilerOptions::debug(),
        root.clone(),
    )
    .with_packages(Arc::new(graph));
    let mut diagnostics = VerificationDiagnostics::default();
    let map = crate::diagnostics::SourceMap::new("<package verify>", &entry);
    let result = crate::run_frontend_with_inputs(&entry, &root, &map, inputs, &mut diagnostics);
    report.checks.insert("parse", !diagnostics.parse_error);
    if diagnostics.layout_error {
        report.checks.insert("source_layout", false);
    }
    if let Err(error) = result {
        let package_error = error
            .downcast_ref::<super::PackageImportError>()
            .map(super::PackageImportError::machine_error);
        return Err(failure(
            if diagnostics.layout_error {
                "source_layout_invalid"
            } else if diagnostics.parse_error {
                "parse_error"
            } else if let Some(detail) = &package_error {
                detail["kind"].as_str().expect("typed package error kind")
            } else {
                "type_check_failed"
            },
            if diagnostics.errors.is_empty() {
                error.to_string()
            } else {
                diagnostics.errors.join("\n")
            },
        ));
    }
    #[cfg(test)]
    {
        report.source_loads = result.unwrap().module_graph.source_loads;
    }
    report.checks.insert("type_check", true);
    Ok(())
}

#[derive(Default)]
struct VerificationDiagnostics {
    parse_error: bool,
    layout_error: bool,
    errors: Vec<String>,
}
impl DiagnosticEmitter for VerificationDiagnostics {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
        if diagnostic.severity == Severity::Error {
            let code = diagnostic.code.as_str();
            self.layout_error |= code == "E2011";
            self.parse_error |= code.starts_with("E00") || code.starts_with("E01");
            let location = diagnostic
                .primary_span()
                .and_then(|span| {
                    sources
                        .get(span.file_id)
                        .map(|source| format!("{}:{}:{}: ", source.path, span.line, span.col))
                })
                .unwrap_or_default();
            self.errors
                .push(format!("{location}{code}: {}", diagnostic.message));
        }
        Ok(())
    }
}

/// Walk each directory once, refusing symlink cycles/aliases and source escapes.
/// BTreeMap both detects competing module spellings and stabilizes import order.
fn discover(root: &Path) -> Result<BTreeMap<String, std::path::PathBuf>, VerificationError> {
    let src = root.join("src");
    let mut stack = vec![src.clone()];
    let mut directories = std::collections::HashSet::new();
    let mut modules = BTreeMap::new();
    while let Some(dir) = stack.pop() {
        let canonical = contained_path(root, &dir).map_err(package_error)?;
        if !directories.insert(canonical) {
            return Err(failure("source_layout_invalid", "directory cycle or alias"));
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| failure("source_layout_invalid", e))? {
            let path = entry
                .map_err(|e| failure("source_layout_invalid", e))?
                .path();
            let canonical = contained_path(root, &path).map_err(package_error)?;
            if canonical.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "wi") {
                continue;
            }
            let relative = path
                .strip_prefix(&src)
                .expect("descendant")
                .with_extension("");
            let mut parts: Vec<_> = relative
                .iter()
                .map(|part| {
                    part.to_str()
                        .ok_or_else(|| failure("source_layout_invalid", "non-UTF8 module path"))
                })
                .collect::<Result<_, _>>()?;
            if parts.len() > 1 && parts.last() == Some(&"mod") {
                parts.pop();
            }
            let logical = parts.join("::");
            // Validate syntax before interpolating filesystem names into imports.
            let probe = format!("import {logical};");
            let tokens = crate::lexer::Lexer::new(&probe)
                .tokenize()
                .map_err(|_| failure("source_layout_invalid", &logical))?;
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            if !errors.is_empty()
                || program.imports.len() != 1
                || program.imports[0].path != logical
                || !program.items.is_empty()
                || logical == "std"
                || logical.starts_with("std::")
            {
                return Err(failure("source_layout_invalid", &logical));
            }
            if modules.insert(logical.clone(), canonical).is_some() {
                return Err(failure(
                    "source_layout_invalid",
                    format!("ambiguous module {logical}"),
                ));
            }
        }
    }
    Ok(modules)
}

fn check_tags(source: &PathSource) -> Result<(), VerificationError> {
    // A checkout can contain packages below its root, including worktrees.
    if !source
        .root
        .ancestors()
        .any(|path| path.join(".git").exists())
    {
        return Ok(());
    }
    // Read-only Git plumbing; no checkout, hooks, filters or repository code.
    let run = |args: &[&str]| -> Result<std::process::Output, VerificationError> {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .arg("-C")
            .arg(&source.root)
            .args(args)
            .output()
            .map_err(|e| failure("git_inspection_failed", e))
    };
    let head = run(&["rev-parse", "--verify", "--quiet", "HEAD"])?;
    // An initialized repository without a first commit has no tag to compare.
    if head.status.code() == Some(1) && head.stderr.is_empty() {
        return Ok(());
    }
    if !head.status.success() {
        return Err(failure(
            "git_inspection_failed",
            String::from_utf8_lossy(&head.stderr),
        ));
    }
    let output = run(&["tag", "--points-at", "HEAD"])?;
    if !output.status.success() {
        return Err(failure(
            "git_inspection_failed",
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let version =
        semver::Version::parse(&source.manifest.project.version).expect("validated version");
    for tag in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(tag_version) = super::git::tag_version(tag)
            && tag_version != version
        {
            return Err(package_error(PackageError::TagManifestVersionMismatch {
                tag: tag.into(),
                version: version.to_string(),
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_imports_load_each_source_once_at_increasing_sizes() {
        for n in [8, 32, 128] {
            for shape in ["chain", "fanout"] {
                let root = std::env::temp_dir().join(format!(
                    "willow-verify-scale-{}-{shape}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir_all(root.join("src")).unwrap();
                std::fs::write(
                    root.join("project.toml"),
                    "[project]\nname='scale'\nversion='1.0.0'\n[willow]\nmanifest-version=1",
                )
                .unwrap();
                for i in 0..n {
                    let import = if i + 1 == n {
                        String::new()
                    } else {
                        let target = if shape == "chain" { i + 1 } else { n - 1 };
                        format!("import m{target} as a; import m{target} as b;")
                    };
                    std::fs::write(
                        root.join(format!("src/m{i}.wi")),
                        format!("module m{i}; {import} pub fn f() -> i64 {{ return 1; }}"),
                    )
                    .unwrap();
                }
                let report = verify_package(&root);
                std::fs::remove_dir_all(root).unwrap();
                assert!(report.ok, "{report:?}");
                assert_eq!(report.modules, n);
                assert_eq!(report.source_loads, n);
                eprintln!(
                    "verify shape={shape} modules={n} explicit_imports={} source_loads={}",
                    2 * (n - 1),
                    report.source_loads
                );
            }
        }
    }
}

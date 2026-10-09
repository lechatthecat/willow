//! Read-only package maintenance. Uses the compiler's source graph, never text
//! searches, and shares remote metadata across equivalent direct dependencies.
use super::{CommandError, GitSource, PackageSource, PathSource, SystemGit, inspect_packages};
use crate::project::{DependencySource, GitSelector};
use anyhow::Result;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

#[derive(Debug, Serialize)]
pub struct TidyReport {
    pub schema: u32,
    pub kind: &'static str,
    pub ok: bool,
    pub unused: Vec<String>,
    pub used: Vec<String>,
    pub modules: usize,
    pub imports: usize,
    #[cfg(test)]
    #[serde(skip)]
    source_loads: usize,
}
impl TidyReport {
    pub fn human(&self) -> String {
        let mut text = format!(
            "Checked {} modules, {} imports (all root source modules).\n",
            self.modules, self.imports
        );
        for alias in &self.used {
            text.push_str(&format!("{alias}: used\n"));
        }
        for alias in &self.unused {
            text.push_str(&format!("{alias}: unused\n"));
        }
        text.push_str("Report only; project.toml and project.lock unchanged.\n");
        text
    }
}

pub fn tidy_packages(root: &Path) -> Result<TidyReport> {
    let packages = Arc::new(inspect_packages(root)?);
    let package = packages.get(packages.root).expect("root package");
    let src = package.source_root();
    let symbols = crate::semantic::ids::SymbolInterner::default();
    let _symbols = symbols.enter();
    let _nodes = crate::parser::ast::NodeIdSession::enter();
    let modules = super::discover_sources(&package.root, &src)?;
    let mut graph = crate::module::ModuleGraph::new(src.clone());
    graph.package_graph = Some(packages.clone());
    graph.project_mode = true;
    let (entry, _) =
        crate::parser::Parser::new(crate::lexer::Lexer::new("").tokenize().unwrap()).parse();
    let resolution =
        crate::module::resolver::resolve_imports_in_graph(&entry, &src, graph, &modules);
    let errors: Vec<_> = resolution
        .diagnostics
        .iter()
        .filter(|d| d.severity == crate::diagnostics::Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    if !errors.is_empty() || resolution.graph.has_syntax_errors {
        return Err(CommandError::new("tidy_analysis_failed", errors.join("\n")).into());
    }
    Ok(tidy_report(&packages, &resolution.graph))
}

fn tidy_report(packages: &super::PackageGraph, graph: &crate::module::ModuleGraph) -> TidyReport {
    let direct: HashSet<_> = packages
        .get(packages.root)
        .unwrap()
        .dependencies
        .iter()
        .map(|d| d.alias.as_str())
        .collect();
    let mut used = HashSet::new();
    let mut imports = 0;
    // Alias spelling matters: two aliases can name the same package. Inspect
    // root-owned ASTs only; transitive consumers cannot mark a root alias used.
    for file in &graph.files {
        if file.package != packages.root {
            continue;
        }
        for import in &file.program.imports {
            imports += 1;
            let first = import.path.split("::").next().unwrap();
            if direct.contains(first) {
                used.insert(first);
            }
        }
    }
    let mut report = TidyReport {
        schema: 1,
        kind: "package.tidy",
        ok: true,
        unused: Vec::new(),
        used: Vec::new(),
        modules: graph.files.len(),
        imports,
        #[cfg(test)]
        source_loads: graph.source_loads,
    };
    for alias in direct {
        if used.contains(alias) {
            report.used.push(alias.into());
        } else {
            report.unused.push(alias.into());
        }
    }
    report.used.sort_unstable();
    report.unused.sort_unstable();
    report
}

#[derive(Debug, Serialize)]
pub struct OutdatedReport {
    pub schema: u32,
    pub kind: &'static str,
    pub ok: bool,
    pub dependencies: Vec<OutdatedDependency>,
}
#[derive(Debug, Serialize)]
pub struct OutdatedDependency {
    pub alias: String,
    pub current: String,
    pub requirement: Option<String>,
    pub compatible: Option<String>,
    pub breaking: Option<String>,
    pub skipped: Option<&'static str>,
}
impl OutdatedReport {
    pub fn human(&self) -> String {
        let mut text = String::new();
        for d in &self.dependencies {
            text.push_str(&format!(
                "{} {} (requirement: {})",
                d.alias,
                d.current,
                d.requirement.as_deref().unwrap_or("none")
            ));
            if let Some(reason) = d.skipped {
                text.push_str(&format!(": skipped ({reason})"));
            } else {
                text.push_str(&format!(
                    ": compatible {}; breaking {}",
                    d.compatible.as_deref().unwrap_or("none"),
                    d.breaking.as_deref().unwrap_or("none")
                ));
            }
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("No direct dependencies.\n");
        }
        text
    }
}

pub fn outdated_packages(root: &Path, offline: bool) -> Result<OutdatedReport> {
    if offline {
        return Err(CommandError::new(
            "network_required",
            "outdated requires online Git metadata; --offline is unsupported",
        )
        .into());
    }
    let source = PathSource::open(root, false)?;
    let declared = source.manifest.dependencies.clone();
    let graph = super::commands::inspect_source(source)?;
    let direct: HashMap<_, _> = graph
        .get(graph.root)
        .unwrap()
        .dependencies
        .iter()
        .map(|d| (d.alias.as_str(), &graph.get(d.package).unwrap().identity))
        .collect();
    let mut sources = HashMap::new();
    let mut selections = HashMap::new();
    let mut dependencies = Vec::new();
    for (alias, dependency) in &declared {
        let current = &direct[alias.as_str()].version;
        let mut row = OutdatedDependency {
            alias: alias.clone(),
            current: current.clone(),
            requirement: None,
            compatible: None,
            breaking: None,
            skipped: None,
        };
        match dependency {
            DependencySource::Git {
                url,
                selector: GitSelector::Version(req),
            } => {
                row.requirement = Some(req.to_string());
                let key = (url.as_str().to_owned(), req.clone(), current.clone());
                if !selections.contains_key(&key) {
                    if !sources.contains_key(url.as_str()) {
                        let remote = GitSource::cached_mode(
                            url.clone(),
                            SystemGit,
                            false,
                            false,
                            None,
                            false,
                        )?;
                        let versions = remote.versions()?;
                        sources.insert(url.as_str().to_owned(), (remote, versions));
                    }
                    let (remote, versions) = &sources[url.as_str()];
                    let current = semver::Version::parse(current)?;
                    let (compatible, breaking) = select_updates(versions, req, &current);
                    // Validate selected tags against their manifests just as update does.
                    for version in [compatible, breaking].into_iter().flatten() {
                        remote.resolve(&GitSelector::Version(format!("={version}").parse()?))?;
                    }
                    selections.insert(
                        key.clone(),
                        (
                            compatible.map(ToString::to_string),
                            breaking.map(ToString::to_string),
                        ),
                    );
                }
                (row.compatible, row.breaking) = selections[&key].clone();
            }
            DependencySource::Git { .. } => row.skipped = Some("pinned_selector"),
            DependencySource::Path { .. } => row.skipped = Some("path_dependency"),
        }
        dependencies.push(row);
    }
    Ok(OutdatedReport {
        schema: 1,
        kind: "package.outdated",
        ok: true,
        dependencies,
    })
}

fn select_updates<'a>(
    versions: impl IntoIterator<Item = &'a semver::Version>,
    req: &semver::VersionReq,
    current: &semver::Version,
) -> (Option<&'a semver::Version>, Option<&'a semver::Version>) {
    let mut compatible = None;
    let mut breaking = None;
    for version in versions {
        if version <= current {
            continue;
        }
        let slot = if req.matches(version) {
            &mut compatible
        } else if version.pre.is_empty() {
            &mut breaking
        } else {
            continue;
        };
        if slot.is_none_or(|old| version > old) {
            *slot = Some(version);
        }
    }
    (compatible, breaking)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semver_classification_covers_requirement_boundaries() {
        for (req, current, versions, compatible, breaking) in [
            (
                "^1",
                "1.4.7",
                "1.4.6,1.4.7,1.6.2,2.0.0",
                Some("1.6.2"),
                Some("2.0.0"),
            ),
            ("~1.4", "1.4.7", "1.4.9,1.5.0", Some("1.4.9"), Some("1.5.0")),
            ("=1.4.7", "1.4.7", "1.4.8", None, Some("1.4.8")),
            ("^0.2", "0.2.1", "0.2.9,0.3.0", Some("0.2.9"), Some("0.3.0")),
            ("*", "1.0.0", "1.0.0,2.0.0", Some("2.0.0"), None),
            (
                ">=1, <3",
                "1.0.0",
                "2.0.0,3.0.0",
                Some("2.0.0"),
                Some("3.0.0"),
            ),
            ("^1", "1.0.0", "2.0.0-beta.1", None, None),
            (
                "^2.0.0-beta.1",
                "2.0.0-beta.1",
                "2.0.0-beta.2,2.0.0",
                Some("2.0.0"),
                None,
            ),
            ("^1", "1.0.0", "0.9.0,1.0.0", None, None),
            ("^1", "1.0.0", "", None, None),
        ] {
            let versions: Vec<semver::Version> = versions
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap())
                .collect();
            let actual =
                select_updates(&versions, &req.parse().unwrap(), &current.parse().unwrap());
            assert_eq!(
                (
                    actual.0.map(ToString::to_string),
                    actual.1.map(ToString::to_string)
                ),
                (compatible.map(str::to_owned), breaking.map(str::to_owned)),
                "{req}"
            );
        }
    }

    #[test]
    fn release_selection_visits_each_candidate_once() {
        for n in [8, 32, 128, 512] {
            let versions: Vec<_> = (0..n).map(|i| semver::Version::new(1, i, 0)).collect();
            let visits = std::cell::Cell::new(0);
            let (compatible, breaking) = select_updates(
                versions.iter().inspect(|_| visits.set(visits.get() + 1)),
                &"^1".parse().unwrap(),
                &"1.0.0".parse().unwrap(),
            );
            assert_eq!(visits.get(), n);
            assert_eq!(compatible.unwrap().minor, n - 1);
            assert!(breaking.is_none());
            eprintln!("outdated candidates={n} visits={}", visits.get());
        }
    }

    #[test]
    fn tidy_shared_modules_are_loaded_once_at_increasing_sizes() {
        for n in [8, 32, 128] {
            for shape in ["chain", "fanout"] {
                let root = std::env::temp_dir().join(format!(
                    "willow-tidy-scale-{}-{shape}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir_all(root.join("src")).unwrap();
                std::fs::write(
                    root.join("project.toml"),
                    "[project]\nname='scale'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n",
                )
                .unwrap();
                for i in 0..n {
                    let import = if i + 1 == n {
                        String::new()
                    } else {
                        let next = if shape == "chain" { i + 1 } else { n - 1 };
                        format!("import m{next} as a; import m{next} as b;")
                    };
                    std::fs::write(
                        root.join(format!("src/m{i}.wi")),
                        format!("module m{i}; {import} pub fn f() -> i64 {{ return 1; }}"),
                    )
                    .unwrap();
                }
                let report = tidy_packages(&root).unwrap();
                std::fs::remove_dir_all(root).unwrap();
                assert_eq!(report.source_loads, n);
                assert_eq!(report.modules, n);
                assert_eq!(report.imports, 2 * (n - 1));
                eprintln!(
                    "tidy shape={shape} modules={n} loads={} imports={}",
                    report.source_loads, report.imports
                );
            }
        }
    }
}

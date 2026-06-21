use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::Lexer;
use crate::module::{ModuleGraph, std_registry};
use crate::parser::{Parser, ast::Program};

/// A single-item import (`import math::add;`), binding a local name to a public
/// item of a module. The binding is validated and wired up later by the type
/// checker and backend.
#[derive(Debug, Clone)]
pub struct ItemImport {
    /// Local name introduced into scope (the alias, or the item name).
    pub local: String,
    /// Canonical module path used for validation and symbol mangling.
    pub canonical_module: String,
    /// The item's own name in that module (e.g. `add`).
    pub item: String,
    pub span: crate::diagnostics::Span,
}

#[derive(Debug)]
pub struct ImportResolution {
    pub graph: ModuleGraph,
    pub item_imports: Vec<ItemImport>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolve all imports reachable from `entry_program`, loading source files
/// from `src_root` (i.e. `import math;` → `src_root/math.wi`).
///
/// Returns the resolved modules in dependency order (dependencies before
/// dependents) together with the entry file's single-item imports.
pub fn resolve_imports(entry_program: &Program, src_root: &Path) -> ImportResolution {
    struct BoundImport {
        span: crate::diagnostics::Span,
        path: String,
        alias: Option<String>,
    }

    let mut graph = ModuleGraph::new(src_root.to_path_buf());
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut item_imports: Vec<ItemImport> = Vec::new();

    // Names each entry import introduces (module access name or item local),
    // for detecting import-vs-import collisions (duplicate aliases / items).
    let mut bound: HashMap<String, BoundImport> = HashMap::new();

    for import in &entry_program.imports {
        let item_count_before = item_imports.len();
        resolve_import(
            &import.path,
            import.alias.as_deref(),
            import.span,
            src_root,
            &mut graph,
            &mut errors,
            Some(&mut item_imports),
        );

        // Determine the local name this import introduced, then check for a
        // collision with an earlier import.
        let bound_name = if std_registry::is_std_path(&import.path) {
            std_import_bound_name(&import.path, import.alias.as_deref(), import.span)
        } else if item_imports.len() > item_count_before {
            item_imports.last().map(|i| i.local.clone())
        } else {
            Some(
                import
                    .alias
                    .clone()
                    .unwrap_or_else(|| module_access_name(&import.path).to_string()),
            )
        };
        if let Some(name) = bound_name {
            if let Some(prev) = bound.get(&name) {
                let identical = prev.path == import.path && prev.alias == import.alias;
                if identical {
                    errors.push(
                        Diagnostic::new(
                            Severity::Warning,
                            ErrorCode::W2002,
                            format!(
                                "duplicate import `{}`",
                                std_registry::display_path(&import.path)
                            ),
                        )
                        .with_label(Label::primary(import.span, "duplicate import"))
                        .with_label(Label::secondary(prev.span, "first imported here")),
                    );
                } else {
                    errors.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E2004,
                            format!("import name `{name}` is defined multiple times"),
                        )
                        .with_label(Label::primary(import.span, "redefined here"))
                        .with_label(Label::secondary(prev.span, "first imported here"))
                        .with_help("rename one of them with `import ... as <alias>;`"),
                    );
                }
            } else {
                bound.insert(
                    name,
                    BoundImport {
                        span: import.span,
                        path: import.path.clone(),
                        alias: import.alias.clone(),
                    },
                );
            }
        }
    }

    ImportResolution {
        graph,
        item_imports,
        diagnostics: errors,
    }
}

/// Classify an import as `std`, a module import, or a single-item import, then
/// dispatch to the right loader. Item imports load the *parent* module and (for
/// the entry file) record an [`ItemImport`] binding via `item_sink`.
#[allow(clippy::too_many_arguments)]
fn resolve_import(
    path: &str,
    alias: Option<&str>,
    span: crate::diagnostics::Span,
    src_root: &Path,
    graph: &mut ModuleGraph,
    errors: &mut Vec<Diagnostic>,
    item_sink: Option<&mut Vec<ItemImport>>,
) {
    // The reserved `std` namespace resolves against the built-in registry.
    if std_registry::is_std_path(path) {
        if graph.mark_import_seen(path)
            && let Err(diag) = std_registry::resolve_std_import(path, span)
        {
            errors.push(diag);
        }
        return;
    }

    // A path that names a module file directly is a module import.
    if find_module_file(src_root, path).is_some() {
        resolve_one(path, alias, span, src_root, graph, errors);
        return;
    }

    // Otherwise, treat the last segment as an item of the parent module
    // (`import math::add;` → item `add` of module `math`).
    if let Some((parent, item)) = path.rsplit_once("::")
        && !parent.is_empty()
        && find_module_file(src_root, parent).is_some()
    {
        resolve_one(parent, None, span, src_root, graph, errors);
        if let Some(sink) = item_sink {
            sink.push(ItemImport {
                local: alias.unwrap_or(item).to_string(),
                canonical_module: parent.to_string(),
                item: item.to_string(),
                span,
            });
        }
        return;
    }

    // Neither a module nor a known item — report the unresolved import.
    resolve_one(path, alias, span, src_root, graph, errors);
}

#[allow(clippy::too_many_arguments)]
fn resolve_one(
    path: &str,
    alias: Option<&str>,
    span: crate::diagnostics::Span,
    src_root: &Path,
    graph: &mut ModuleGraph,
    errors: &mut Vec<Diagnostic>,
) {
    // Already fully resolved — skip (also deduplicates repeated imports).
    // `std` and module-vs-item classification are handled by `resolve_import`.
    if graph.contains(path) {
        return;
    }

    let candidates = candidate_module_paths(src_root, path);
    let module_path = candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned();

    let Some(module_path) = module_path else {
        let tried = candidates
            .iter()
            .map(|candidate| format!("  - {}", candidate.display()))
            .collect::<Vec<_>>()
            .join("\n");
        errors.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0401,
                format!("unresolved import `{}`", path),
            )
            .with_label(Label::primary(span, "module not found"))
            .with_note(format!("tried to find module at:\n{}", tried))
            .with_help(format!(
                "create `{}` or check the import name",
                candidates[0].display()
            )),
        );
        return;
    };

    let source = match std::fs::read_to_string(&module_path) {
        Ok(s) => s,
        Err(e) => {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0401,
                    format!("cannot read module `{}`: {}", path, e),
                )
                .with_label(Label::primary(span, "failed to read")),
            );
            return;
        }
    };

    let module_id = graph.reserve_module_id(path);
    let tokens = match Lexer::with_file_id(&source, module_id.file_id()).tokenize() {
        Ok(t) => t,
        Err(errs) => {
            errors.extend(errs);
            return;
        }
    };

    let (program, parse_errs) = Parser::new(tokens).parse();
    if !parse_errs.is_empty() {
        errors.extend(parse_errs);
        return;
    }

    // An imported file's declared module identity must match the import path
    // that reached it (both canonical `::`-normalized). Files without a `module`
    // declaration keep deriving their identity from the path (backward compatible).
    if let Some(decl) = &program.module
        && decl.path != path
    {
        errors.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E2011,
                format!(
                    "module declaration `{}` does not match import path `{}`",
                    std_registry::display_path(&decl.path),
                    std_registry::display_path(path)
                ),
            )
            .with_label(Label::primary(decl.span, "declared module here"))
            .with_help(format!(
                "rename the module to `{}` or import it by its declared path",
                std_registry::display_path(path)
            )),
        );
    }

    if let Err(cycle) = graph.begin_visit(path) {
        errors.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0403, "import cycle detected")
                .with_label(Label::primary(span, "this import creates a cycle"))
                .with_note(format!("import cycle: {}", cycle.join(" -> ")))
                .with_help(
                    "remove one of the imports or move shared declarations into another module",
                ),
        );
        return;
    }

    // Recursively resolve this module's own imports first. Transitive item
    // imports are classified (so their files load) but not yet bound into the
    // importing module's scope — that is a later stage.
    for sub_import in &program.imports {
        let dependency = imported_module_path(src_root, &sub_import.path);
        resolve_import(
            &sub_import.path,
            sub_import.alias.as_deref(),
            sub_import.span,
            src_root,
            graph,
            errors,
            None,
        );
        if let Some(dependency) = dependency
            && graph.contains(&dependency)
        {
            graph.add_dependency(path, &dependency);
        }
    }

    graph.end_visit(path);

    let name = alias
        .map(str::to_string)
        .unwrap_or_else(|| module_access_name(path).to_string());
    graph.add_file(name, path.to_string(), module_path, source, program);
}

fn imported_module_path(src_root: &Path, path: &str) -> Option<String> {
    if std_registry::is_std_path(path) {
        return None;
    }
    if find_module_file(src_root, path).is_some() {
        return Some(path.to_string());
    }
    path.rsplit_once("::")
        .and_then(|(parent, _)| find_module_file(src_root, parent).map(|_| parent.to_string()))
}

/// The existing module source file for `path`, if any.
fn find_module_file(src_root: &Path, path: &str) -> Option<PathBuf> {
    candidate_module_paths(src_root, path)
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn candidate_module_paths(src_root: &Path, path: &str) -> Vec<PathBuf> {
    let path_buf = module_path_buf(path);
    vec![
        src_root.join(path_buf.with_extension("wi")),
        src_root.join(path_buf).join("mod.wi"),
    ]
}

fn module_path_buf(path: &str) -> PathBuf {
    path.split("::").collect()
}

fn module_access_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn std_import_bound_name(
    path: &str,
    alias: Option<&str>,
    span: crate::diagnostics::Span,
) -> Option<String> {
    if !std_registry::is_std_path(path) {
        return None;
    }
    if std_registry::resolve_std_import(path, span).is_err() {
        return None;
    }
    alias
        .map(str::to_string)
        .or_else(|| path.rsplit("::").next().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new(files: &[(&str, &str)]) -> Self {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "willow_module_graph_{}_{}",
                std::process::id(),
                id
            ));
            std::fs::create_dir_all(&root).unwrap();
            for (name, source) in files {
                let path = root.join(name);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(path, source).unwrap();
            }
            Self(root)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn parse(source: &str) -> Program {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, diagnostics) = Parser::new(tokens).parse();
        assert!(diagnostics.is_empty());
        program
    }

    #[test]
    fn graph_caches_files_in_dependency_first_order() {
        let project = TempProject::new(&[
            ("a.wi", "module a; import b; import c; pub fn a() {}"),
            ("b.wi", "module b; import c; pub fn b() {}"),
            ("c.wi", "module c; pub fn c() {}"),
        ]);
        let entry = parse("import a; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.is_empty());
        assert_eq!(
            resolution
                .graph
                .files
                .iter()
                .map(|file| file.canonical_path.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        assert_eq!(resolution.graph.dependencies("a"), ["b", "c"]);
        assert_eq!(resolution.graph.dependencies("b"), ["c"]);
        assert_eq!(
            resolution.graph.module_id("c"),
            Some(super::super::ModuleId(2))
        );
    }

    #[test]
    fn duplicate_import_reuses_cached_source_file() {
        let project =
            TempProject::new(&[("a.wi", "module a; pub fn value() -> i64 { return 1; }")]);
        let entry = parse("import a; import a; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert_eq!(resolution.graph.files.len(), 1);
        assert!(
            resolution
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == ErrorCode::W2002)
        );
    }

    #[test]
    fn graph_cycle_detection_reports_full_path() {
        let project = TempProject::new(&[
            ("a.wi", "module a; import b; pub fn a() {}"),
            ("b.wi", "module b; import a; pub fn b() {}"),
        ]);
        let entry = parse("import a; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        let cycle = resolution
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == ErrorCode::E0403)
            .expect("cycle diagnostic");
        assert!(cycle.notes.iter().any(|note| note.contains("a -> b -> a")));
    }
}

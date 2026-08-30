use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::Lexer;
use crate::module::source_file::SourceFile;
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

/// One `import module;` binding, as seen from a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBinding {
    /// The name this file calls the module by: its alias, or the last segment
    /// of the path.
    pub access: String,
    /// The name the module graph registered this module under, which is the
    /// FIRST importer's spelling. Equal to `access` unless another file got
    /// there first with a different alias.
    pub graph_name: String,
    /// Canonical `::`-separated module identity.
    pub canonical_path: String,
}

/// What one file's `import` lines bind, classified the same way
/// [`resolve_import`] classified them when it loaded the files.
#[derive(Debug, Clone, Default)]
pub struct UnitImports {
    pub modules: Vec<ModuleBinding>,
    pub items: Vec<ItemImport>,
}

/// Classify `program`'s imports against the modules the resolver loaded.
///
/// `import a::b;` is a MODULE import when `a::b` is itself a loaded module, and
/// an ITEM import of module `a` otherwise — the same module-first precedence
/// [`resolve_import`] applies against the file system, replayed here against
/// what it loaded. `std` paths bind neither and are skipped.
///
/// The back end needs the distinction for two things it cannot get from the
/// import path alone. An item import binds the ITEM, so the module it came from
/// is not a name this file can write, and letting it count as visible makes an
/// unrelated class of the same bare name ambiguous (willow-vtlr). And the local
/// name an item import does bind has to be wired to that module's mangled
/// symbol per file: two files can bind the same local name to different
/// modules' functions (willow-28h8).
pub fn classify_unit_imports(program: &Program, modules: &[SourceFile]) -> UnitImports {
    let find = |path: &str| modules.iter().find(|m| m.canonical_path == path);
    let mut out = UnitImports::default();
    for import in &program.imports {
        if std_registry::is_std_path(&import.path) {
            continue;
        }
        if let Some(dependency) = find(&import.path) {
            out.modules.push(ModuleBinding {
                access: import
                    .alias
                    .clone()
                    .unwrap_or_else(|| module_access_name(&import.path).to_string()),
                graph_name: dependency.name.clone(),
                canonical_path: dependency.canonical_path.clone(),
            });
            continue;
        }
        if let Some((parent, item)) = import.path.rsplit_once("::")
            && let Some(dependency) = find(parent)
        {
            out.items.push(ItemImport {
                local: import.alias.clone().unwrap_or_else(|| item.to_string()),
                canonical_module: dependency.canonical_path.clone(),
                item: item.to_string(),
                span: import.span,
            });
        }
    }
    out
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

    // A path that names a module file directly is a module import. This
    // module-first precedence also applies to paths expanded from grouped
    // syntax: if both `math.wi` and `math/add.wi` exist, `math::{add}` resolves
    // `math::add` as a child module, exactly like `import math::add;`.
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
        // Keep the partially parsed module in the graph long enough for the
        // diagnostic reporter to resolve its FileId to the imported file.
        // The front end clears files after an import failure, so this recovery
        // program is never desugared, type-checked, or emitted.
        let name = alias
            .map(str::to_string)
            .unwrap_or_else(|| module_access_name(path).to_string());
        graph.add_file(name, path.to_string(), module_path, source, program);
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

    // willow-vtlr / willow-28h8 — what one file's imports bind, as the back end
    // has to be told rather than guess from the path shape.

    /// The classification of `entry`'s imports against a project's modules.
    fn classify(files: &[(&str, &str)], entry_source: &str) -> (UnitImports, TempProject) {
        let project = TempProject::new(files);
        let entry = parse(entry_source);
        let resolution = resolve_imports(&entry, &project.0);
        assert!(
            resolution.diagnostics.is_empty(),
            "{:?}",
            resolution.diagnostics
        );
        let imports = classify_unit_imports(&entry, &resolution.graph.files);
        (imports, project)
    }

    #[test]
    fn classify_p1_a_module_import_binds_the_module() {
        let (imports, _project) = classify(
            &[("sales.wi", "module sales; pub fn v() -> i64 { return 1; }")],
            "import sales; fn main() {}",
        );
        assert!(imports.items.is_empty());
        assert_eq!(imports.modules.len(), 1);
        assert_eq!(imports.modules[0].access, "sales");
        assert_eq!(imports.modules[0].canonical_path, "sales");
    }

    #[test]
    fn classify_p2_a_child_module_import_binds_the_child_alone() {
        // The parent is a module of this build, and importing the child is
        // still not a way to name it.
        let (imports, _project) = classify(
            &[
                (
                    "pricing.wi",
                    "module pricing; pub fn v() -> i64 { return 1; }",
                ),
                (
                    "pricing/rules.wi",
                    "module pricing::rules; pub fn markup(n: i64) -> i64 { return n; }",
                ),
            ],
            "import pricing::rules; fn main() {}",
        );
        assert!(imports.items.is_empty());
        assert_eq!(
            imports
                .modules
                .iter()
                .map(|m| m.access.as_str())
                .collect::<Vec<_>>(),
            ["rules"]
        );
        assert_eq!(imports.modules[0].canonical_path, "pricing::rules");
    }

    #[test]
    fn classify_p3_an_item_import_binds_the_item_and_no_module() {
        let (imports, _project) = classify(
            &[(
                "calc.wi",
                "module calc; pub fn add(n: i64) -> i64 { return n; }",
            )],
            "import calc::add; fn main() {}",
        );
        assert!(imports.modules.is_empty(), "{:?}", imports.modules);
        assert_eq!(imports.items.len(), 1);
        assert_eq!(imports.items[0].local, "add");
        assert_eq!(imports.items[0].canonical_module, "calc");
        assert_eq!(imports.items[0].item, "add");
    }

    #[test]
    fn classify_p4_aliases_are_the_local_names() {
        let (imports, _project) = classify(
            &[
                ("sales.wi", "module sales; pub fn v() -> i64 { return 1; }"),
                (
                    "calc.wi",
                    "module calc; pub fn add(n: i64) -> i64 { return n; }",
                ),
            ],
            "import sales as market; import calc::add as plus; fn main() {}",
        );
        assert_eq!(imports.modules[0].access, "market");
        assert_eq!(imports.modules[0].canonical_path, "sales");
        assert_eq!(imports.items[0].local, "plus");
        assert_eq!(imports.items[0].item, "add");
    }

    #[test]
    fn classify_p5_a_std_import_binds_neither() {
        let (imports, _project) = classify(&[], "import std::collections::Array; fn main() {}");
        assert!(imports.modules.is_empty());
        assert!(imports.items.is_empty());
    }

    #[test]
    fn classify_p6_the_graph_name_is_the_first_importers_spelling() {
        // `sales` reaches the graph under the entry's alias, so a module that
        // imports it plainly has to be told both spellings: the back end's
        // module tables are keyed by the graph's.
        let project = TempProject::new(&[
            ("sales.wi", "module sales; pub fn v() -> i64 { return 1; }"),
            (
                "ledger.wi",
                "module ledger; import sales; pub fn t() -> i64 { return sales::v(); }",
            ),
        ]);
        let entry = parse("import sales as market; import ledger; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert!(
            resolution.diagnostics.is_empty(),
            "{:?}",
            resolution.diagnostics
        );
        let ledger = resolution
            .graph
            .files
            .iter()
            .find(|file| file.canonical_path == "ledger")
            .expect("ledger module");
        let imports = classify_unit_imports(&ledger.program, &resolution.graph.files);
        let binding = imports
            .modules
            .iter()
            .find(|m| m.canonical_path == "sales")
            .expect("sales binding");
        assert_eq!(binding.access, "sales");
        assert_eq!(binding.graph_name, "market");
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

    // Grouped-import resolver perspectives continue the parser P01-P17 list.

    #[test]
    fn grouped_import_p18_resolves_known_std_items() {
        let project = TempProject::new(&[]);
        let entry = parse("import std::collections::{Array, Map}; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.is_empty());
    }

    #[test]
    fn grouped_import_p19_reports_unknown_std_item() {
        let project = TempProject::new(&[]);
        let entry = parse("import std::collections::{Array, Missing}; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ErrorCode::E2006 && diagnostic.message.contains("Missing")
        }));
    }

    #[test]
    fn grouped_import_p20_warns_for_duplicate_across_group_and_ordinary_import() {
        let project = TempProject::new(&[]);
        let entry = parse(
            "import std::collections::{Array, Map}; \
             import std::collections::Array; \
             fn main() {}",
        );
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ErrorCode::W2002 && diagnostic.message.contains("Array")
        }));
    }

    #[test]
    fn grouped_import_p21_reports_alias_conflict() {
        let project = TempProject::new(&[]);
        let entry = parse(
            "import std::collections::{Array as Collection, Map as Collection}; fn main() {}",
        );
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ErrorCode::E2004 && diagnostic.message.contains("Collection")
        }));
    }

    #[test]
    fn grouped_import_p22_loads_user_module_once_and_binds_each_item() {
        let project = TempProject::new(&[(
            "math.wi",
            "module math; pub fn add() -> i64 { return 1; } \
             pub fn sub() -> i64 { return 2; }",
        )]);
        let entry = parse("import math::{add, sub as subtract}; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);
        assert!(resolution.diagnostics.is_empty());
        assert_eq!(resolution.graph.files.len(), 1);
        assert_eq!(
            resolution
                .item_imports
                .iter()
                .map(|item| (item.local.as_str(), item.item.as_str()))
                .collect::<Vec<_>>(),
            [("add", "add"), ("subtract", "sub")]
        );
    }

    #[test]
    fn grouped_import_prefers_child_module_when_module_and_item_names_collide() {
        let project = TempProject::new(&[
            ("math.wi", "module math; pub fn add() -> i64 { return 1; }"),
            (
                "math/add.wi",
                "module math::add; pub fn value() -> i64 { return 2; }",
            ),
        ]);
        let entry = parse("import math::{add}; fn main() {}");
        let resolution = resolve_imports(&entry, &project.0);

        assert!(resolution.diagnostics.is_empty());
        assert!(
            resolution.item_imports.is_empty(),
            "the colliding `add` name must bind the child module, not math::add()"
        );
        assert_eq!(
            resolution
                .graph
                .files
                .iter()
                .map(|module| module.canonical_path.as_str())
                .collect::<Vec<_>>(),
            ["math::add"]
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

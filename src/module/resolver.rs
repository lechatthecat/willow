use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::Lexer;
use crate::module::std_registry;
use crate::parser::{Parser, ast::Program};

/// One resolved (parsed) imported module.
#[derive(Debug)]
pub struct ResolvedModule {
    /// The name used to access the module (import alias or original path).
    pub name: String,
    /// Canonical import path used for symbol mangling, independent of aliases.
    pub canonical_path: String,
    pub path: PathBuf,
    pub source: String,
    pub program: Program,
}

/// A single-item import (`import math.add;`), binding a local name to a public
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
    pub modules: Vec<ResolvedModule>,
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

    let mut resolved: Vec<ResolvedModule> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: Vec<String> = Vec::new();
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
            &mut resolved,
            &mut visited,
            &mut visiting,
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
                            format!("duplicate import `{}`", std_registry::dotted(&import.path)),
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
        modules: resolved,
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
    resolved: &mut Vec<ResolvedModule>,
    visited: &mut HashSet<String>,
    visiting: &mut Vec<String>,
    errors: &mut Vec<Diagnostic>,
    item_sink: Option<&mut Vec<ItemImport>>,
) {
    // The reserved `std` namespace resolves against the built-in registry.
    if std_registry::is_std_path(path) {
        if !visited.contains(path) {
            if let Err(diag) = std_registry::resolve_std_import(path, span) {
                errors.push(diag);
            }
            visited.insert(path.to_string());
        }
        return;
    }

    // A path that names a module file directly is a module import.
    if find_module_file(src_root, path).is_some() {
        resolve_one(
            path, alias, span, src_root, resolved, visited, visiting, errors,
        );
        return;
    }

    // Otherwise, treat the last segment as an item of the parent module
    // (`import math.add;` → item `add` of module `math`).
    if let Some((parent, item)) = path.rsplit_once("::") {
        if !parent.is_empty() && find_module_file(src_root, parent).is_some() {
            resolve_one(
                parent, None, span, src_root, resolved, visited, visiting, errors,
            );
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
    }

    // Neither a module nor a known item — report the unresolved import.
    resolve_one(
        path, alias, span, src_root, resolved, visited, visiting, errors,
    );
}

#[allow(clippy::too_many_arguments)]
fn resolve_one(
    path: &str,
    alias: Option<&str>,
    span: crate::diagnostics::Span,
    src_root: &Path,
    resolved: &mut Vec<ResolvedModule>,
    visited: &mut HashSet<String>,
    visiting: &mut Vec<String>,
    errors: &mut Vec<Diagnostic>,
) {
    // Already fully resolved — skip (also deduplicates repeated imports).
    // `std` and module-vs-item classification are handled by `resolve_import`.
    if visited.contains(path) {
        return;
    }

    // Cycle detection.
    if let Some(cycle_start) = visiting.iter().position(|module| module == path) {
        let mut cycle = visiting[cycle_start..].to_vec();
        cycle.push(path.to_string());
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

    let tokens = match Lexer::new(&source).tokenize() {
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
    if let Some(decl) = &program.module {
        if decl.path != path {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2011,
                    format!(
                        "module declaration `{}` does not match import path `{}`",
                        std_registry::dotted(&decl.path),
                        std_registry::dotted(path)
                    ),
                )
                .with_label(Label::primary(decl.span, "declared module here"))
                .with_help(format!(
                    "rename the module to `{}` or import it by its declared path",
                    std_registry::dotted(path)
                )),
            );
        }
    }

    // Mark as currently being visited (for cycle detection of transitive imports).
    visiting.push(path.to_string());

    // Recursively resolve this module's own imports first. Transitive item
    // imports are classified (so their files load) but not yet bound into the
    // importing module's scope — that is a later stage.
    for sub_import in &program.imports {
        resolve_import(
            &sub_import.path,
            sub_import.alias.as_deref(),
            sub_import.span,
            src_root,
            resolved,
            visited,
            visiting,
            errors,
            None,
        );
    }

    visiting.pop();
    visited.insert(path.to_string());

    let name = alias
        .map(str::to_string)
        .unwrap_or_else(|| module_access_name(path).to_string());
    resolved.push(ResolvedModule {
        name,
        canonical_path: path.to_string(),
        path: module_path,
        source,
        program,
    });
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

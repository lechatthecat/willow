//! The reserved `std` standard-library namespace and its import resolver.
//!
//! `std` is a built-in namespace resolved before local modules and external
//! packages. Unlike local imports (which map to source files), `std` imports
//! are validated against this fixed registry of public modules and items.
//!
//! Stage 2 establishes the namespace and the item-import
//! resolver. The concrete types/functions behind some items (notably
//! `collections.Array` / `collections.Map`) are wired into the type system in
//! later stages; here an import of a known item simply resolves without error.

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::qualified_name::QualifiedName;
use crate::stdlib_schema::{STDLIB_SCHEMA, StdModuleSchema};

/// The reserved top-level standard-library namespace.
pub const STD_ROOT: &str = "std";

/// What a resolved `import std::...;` statement refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdImport {
    /// `import std::collections;` — the whole module namespace.
    Module { module: String },
    /// `import std::collections::Array;` — a single item from a module.
    Item { module: String, item: String },
}

/// Split an import path into segments. Import paths use `::` as the separator.
pub fn import_segments(path: &str) -> Vec<&str> {
    QualifiedName::new(path).segments().collect()
}

/// True if this import path targets the reserved `std` namespace.
pub fn is_std_path(path: &str) -> bool {
    import_segments(path).first() == Some(&STD_ROOT)
}

/// A standard module written in Willow and bundled into the compiler. These
/// use ordinary module binding and checking; only the source text is built in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStdModule {
    /// Canonical import path, e.g. `std::json`.
    pub path: &'static str,
    /// Display path of the bundled file, e.g. `<std>/json.wi`.
    pub file: &'static str,
    pub source: &'static str,
}

/// Every bundled source module. One row per module: the resolver, the
/// dependency index and source queries all read this table.
pub const SOURCE_STD_MODULES: &[SourceStdModule] = &[
    SourceStdModule {
        path: "std::json",
        file: "<std>/json.wi",
        source: include_str!("../../std/json.wi"),
    },
    SourceStdModule {
        path: "std::set",
        file: "<std>/set.wi",
        source: include_str!("../../std/set.wi"),
    },
    SourceStdModule {
        path: "std::sorted_map",
        file: "<std>/sorted_map.wi",
        source: include_str!("../../std/sorted_map.wi"),
    },
    SourceStdModule {
        path: "std::strings",
        file: "<std>/strings.wi",
        source: include_str!("../../std/strings.wi"),
    },
    SourceStdModule {
        path: "std::bytes",
        file: "<std>/bytes.wi",
        source: include_str!("../../std/bytes.wi"),
    },
];

/// The bundled source module an import path names (the module itself or one of
/// its items), with the item segment when present.
pub fn source_std_module(path: &str) -> Option<(&'static SourceStdModule, Option<&str>)> {
    let find = |module: &str| SOURCE_STD_MODULES.iter().find(|m| m.path == module);
    if let Some(module) = find(path) {
        return Some((module, None));
    }
    let (parent, item) = path.rsplit_once("::")?;
    find(parent).map(|module| (module, Some(item)))
}

/// The bundled source module whose display file is `file`.
pub fn source_std_file(file: &str) -> Option<&'static SourceStdModule> {
    SOURCE_STD_MODULES
        .iter()
        .find(|candidate| candidate.file == file)
}

/// Source-backed standard modules use ordinary module binding and checking.
pub fn is_source_std_path(path: &str) -> bool {
    source_std_module(path).is_some()
}

pub fn is_builtin_std_path(path: &str) -> bool {
    is_std_path(path) && !is_source_std_path(path)
}

/// Render an import path in canonical form for diagnostics.
pub fn display_path(path: &str) -> String {
    path.to_string()
}

fn find_module(name: &str) -> Option<&'static StdModuleSchema> {
    STDLIB_SCHEMA.iter().find(|m| m.name == name)
}

/// Builtin and bundled-source module names, in registry order.
fn module_names() -> impl Iterator<Item = &'static str> {
    STDLIB_SCHEMA.iter().map(|m| m.name).chain(
        SOURCE_STD_MODULES
            .iter()
            .filter_map(|m| m.path.strip_prefix("std::")),
    )
}

fn available_modules() -> String {
    module_names().collect::<Vec<_>>().join(", ")
}

/// Case-sensitive Levenshtein distance, used only for "did you mean" hints.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The closest candidate to `name` within an edit distance threshold, for a
/// "did you mean" suggestion.
fn nearest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let threshold = (name.len() / 2).max(2);
    candidates
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= threshold)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Resolve an `import std::...;` path against the registry. `path` must be a std
/// path (see [`is_std_path`]). Returns the resolved import or a source-aware
/// diagnostic for the unknown-root/module/item cases.
pub fn resolve_std_import(path: &str, span: Span) -> Result<StdImport, Diagnostic> {
    let segs = import_segments(path);
    debug_assert_eq!(segs.first(), Some(&STD_ROOT));
    match segs.as_slice() {
        [_] => Err(Diagnostic::new(
            Severity::Error,
            ErrorCode::E2005,
            "`std` is a reserved namespace and cannot be imported directly",
        )
        .with_label(Label::primary(span, "import a module, not the root"))
        .with_help(format!(
            "import a module such as `std::collections` (available: {})",
            available_modules()
        ))),
        [_, module] => match find_module(module) {
            Some(_) => Ok(StdImport::Module {
                module: (*module).to_string(),
            }),
            None => Err(unknown_module(module, span)),
        },
        [_, module, item] => {
            let Some(m) = find_module(module) else {
                return Err(unknown_module(module, span));
            };
            if m.items.iter().any(|candidate| candidate.name == *item) {
                Ok(StdImport::Item {
                    module: (*module).to_string(),
                    item: (*item).to_string(),
                })
            } else {
                let mut diag = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2006,
                    format!("no item `{}` in `std::{}`", item, module),
                )
                .with_label(Label::primary(span, "unknown standard library item"))
                .with_help(format!(
                    "available items: {}",
                    m.items
                        .iter()
                        .map(|candidate| candidate.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                if let Some(suggestion) = nearest(item, m.items.iter().map(|item| item.name)) {
                    diag = diag.with_note(format!("did you mean `{}`?", suggestion));
                }
                Err(diag)
            }
        }
        _ => Err(Diagnostic::new(
            Severity::Error,
            ErrorCode::E2007,
            format!("`{}` is not a valid std import path", display_path(path)),
        )
        .with_label(Label::primary(
            span,
            "std import paths are `std::module::item`",
        ))
        .with_help("import a single item, e.g. `import std::collections::Array;`")),
    }
}

fn unknown_module(module: &str, span: Span) -> Diagnostic {
    let mut diag = Diagnostic::new(
        Severity::Error,
        ErrorCode::E2007,
        format!("unknown std module `{}`", module),
    )
    .with_label(Label::primary(span, "no such module in `std`"))
    .with_help(format!("available std modules: {}", available_modules()));
    if let Some(suggestion) = nearest(module, module_names()) {
        diag = diag.with_note(format!("did you mean `std::{}`?", suggestion));
    }
    diag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span::new(0, 0, 0, 0)
    }

    #[test]
    fn is_std_path_detects_std_root() {
        assert!(is_std_path("std::collections::Array"));
        assert!(is_std_path("std"));
        assert!(!is_std_path("math"));
        assert!(!is_std_path("graph::a"));
        // A local module that merely starts with the letters "std" is not std.
        assert!(!is_std_path("studio::thing"));
    }

    #[test]
    fn display_path_renders_canonical_form() {
        assert_eq!(
            display_path("std::collections::Array"),
            "std::collections::Array"
        );
    }

    #[test]
    fn resolves_known_single_item() {
        assert_eq!(
            resolve_std_import("std::collections::Array", span()).unwrap(),
            StdImport::Item {
                module: "collections".to_string(),
                item: "Array".to_string(),
            }
        );
        assert!(resolve_std_import("std::io::println", span()).is_ok());
        assert!(resolve_std_import("std::option::Option", span()).is_ok());
        assert!(resolve_std_import("std::env::program_name", span()).is_ok());
    }

    #[test]
    fn resolves_known_module() {
        assert_eq!(
            resolve_std_import("std::collections", span()).unwrap(),
            StdImport::Module {
                module: "collections".to_string(),
            }
        );
    }

    #[test]
    fn registry_resolves_every_schema_entry() {
        for module in STDLIB_SCHEMA {
            assert!(
                resolve_std_import(&format!("std::{}", module.name), span()).is_ok(),
                "schema module std::{} was not resolvable",
                module.name
            );
            for item in module.items {
                assert!(
                    resolve_std_import(&format!("std::{}::{}", module.name, item.name), span())
                        .is_ok(),
                    "schema item std::{}::{} was not resolvable",
                    module.name,
                    item.name
                );
            }
        }
    }

    #[test]
    fn source_modules_resolve_by_path_item_and_file() {
        for module in SOURCE_STD_MODULES {
            assert_eq!(source_std_module(module.path), Some((module, None)));
            let item = format!("{}::Item", module.path);
            assert_eq!(source_std_module(&item), Some((module, Some("Item"))));
            assert_eq!(source_std_file(module.file), Some(module));
            assert!(is_source_std_path(module.path));
            assert!(!is_builtin_std_path(module.path));
            let name = module.path.strip_prefix("std::").expect("std path");
            assert!(
                find_module(name).is_none(),
                "{name} is both builtin and source"
            );
            assert!(module_names().any(|known| known == name));
        }
        // Only one item segment below a source module is an import path.
        assert_eq!(source_std_module("std::set::HashSet::new"), None);
        assert_eq!(source_std_module("std::sets"), None);
        assert_eq!(source_std_file("<std>/missing.wi"), None);
        assert!(is_builtin_std_path("std::path"));
        let err = resolve_std_import("std::sett", span()).unwrap_err();
        assert!(
            err.notes.iter().any(|n| n.contains("set")),
            "{:?}",
            err.notes
        );
    }

    #[test]
    fn bare_std_root_is_reserved_e2005() {
        let err = resolve_std_import("std", span()).unwrap_err();
        assert_eq!(err.code.as_str(), "E2005");
    }

    #[test]
    fn unknown_module_reports_e2007_with_suggestion() {
        // "collection" is one edit away from "collections".
        let err = resolve_std_import("std::collection::Array", span()).unwrap_err();
        assert_eq!(err.code.as_str(), "E2007");
        assert!(
            err.notes.iter().any(|n| n.contains("collections")),
            "expected a did-you-mean suggestion, got: {:?}",
            err.notes
        );
    }

    #[test]
    fn unknown_item_reports_e2006_with_suggestion() {
        // "Aray" is one edit away from "Array".
        let err = resolve_std_import("std::collections::Aray", span()).unwrap_err();
        assert_eq!(err.code.as_str(), "E2006");
        assert!(err.notes.iter().any(|n| n.contains("Array")));
    }

    #[test]
    fn unknown_item_without_close_match_still_e2006() {
        let err = resolve_std_import("std::collections::Vec", span()).unwrap_err();
        assert_eq!(err.code.as_str(), "E2006");
    }

    #[test]
    fn too_deep_path_reports_e2007() {
        let err = resolve_std_import("std::collections::Array::extra", span()).unwrap_err();
        assert_eq!(err.code.as_str(), "E2007");
    }
}

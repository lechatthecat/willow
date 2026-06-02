//! The reserved `std` standard-library namespace and its import resolver.
//!
//! `std` is a built-in namespace resolved before local modules and external
//! packages. Unlike local imports (which map to source files), `std` imports
//! are validated against this fixed registry of public modules and items.
//!
//! Stage 2 (willow-4bv.2) establishes the namespace and the item-import
//! resolver. The concrete types/functions behind some items (notably
//! `collections.Array` / `collections.Map`) are wired into the type system in
//! later stages; here an import of a known item simply resolves without error.

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};

/// The reserved top-level standard-library namespace.
pub const STD_ROOT: &str = "std";

/// A public std module and the item names it exports.
struct StdModule {
    name: &'static str,
    items: &'static [&'static str],
}

/// The initial public `std` surface (Stage 2). Item *availability* is recorded
/// here; the concrete types/functions are provided by the prelude, compiler
/// builtins, or later stages.
const STD_MODULES: &[StdModule] = &[
    StdModule {
        name: "collections",
        items: &["Array", "Map"],
    },
    StdModule {
        name: "option",
        items: &["Option"],
    },
    StdModule {
        name: "result",
        items: &["Result"],
    },
    StdModule {
        name: "io",
        items: &["println", "print", "eprintln"],
    },
    StdModule {
        name: "env",
        items: &["args", "arg", "args_len", "program_name"],
    },
];

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
    path.split("::").collect()
}

/// True if this import path targets the reserved `std` namespace.
pub fn is_std_path(path: &str) -> bool {
    import_segments(path).first() == Some(&STD_ROOT)
}

/// Render an import path in canonical form for diagnostics.
pub fn display_path(path: &str) -> String {
    path.to_string()
}

fn find_module(name: &str) -> Option<&'static StdModule> {
    STD_MODULES.iter().find(|m| m.name == name)
}

fn available_modules() -> String {
    STD_MODULES
        .iter()
        .map(|m| m.name)
        .collect::<Vec<_>>()
        .join(", ")
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
            if m.items.contains(item) {
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
                .with_help(format!("available items: {}", m.items.join(", ")));
                if let Some(suggestion) = nearest(item, m.items.iter().copied()) {
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
    if let Some(suggestion) = nearest(module, STD_MODULES.iter().map(|m| m.name)) {
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

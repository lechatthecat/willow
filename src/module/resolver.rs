use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::Lexer;
use crate::parser::{Parser, ast::Program};

/// One resolved (parsed) imported module.
#[derive(Debug)]
pub struct ResolvedModule {
    /// The name used to access the module (import alias or original path).
    pub name: String,
    pub path: PathBuf,
    pub source: String,
    pub program: Program,
}

/// Resolve all imports reachable from `entry_program`, loading source files
/// from `src_root` (i.e. `import math;` → `src_root/math.wi`).
///
/// Returns the resolved modules in dependency order (dependencies before dependents).
pub fn resolve_imports(
    entry_program: &Program,
    src_root: &Path,
) -> Result<Vec<ResolvedModule>, Vec<Diagnostic>> {
    let mut resolved: Vec<ResolvedModule> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: Vec<String> = Vec::new();
    let mut errors: Vec<Diagnostic> = Vec::new();

    for import in &entry_program.imports {
        resolve_one(
            &import.path,
            import.alias.as_deref(),
            import.span,
            src_root,
            &mut resolved,
            &mut visited,
            &mut visiting,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(errors)
    }
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
    // Already fully resolved — skip.
    if visited.contains(path) {
        return;
    }

    // Cycle detection.
    if visiting.contains(&path.to_string()) {
        errors.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0401,
                format!("import cycle detected involving module `{}`", path),
            )
            .with_label(Label::primary(span, "cyclic import here")),
        );
        return;
    }

    let module_path = src_root.join(format!("{}.wi", path));

    if !module_path.exists() {
        errors.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0401,
                format!(
                    "cannot find module `{}` (looked for `{}`)",
                    path,
                    module_path.display()
                ),
            )
            .with_label(Label::primary(span, "module not found")),
        );
        return;
    }

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

    // Mark as currently being visited (for cycle detection of transitive imports).
    visiting.push(path.to_string());

    // Recursively resolve this module's own imports first.
    for sub_import in &program.imports {
        resolve_one(
            &sub_import.path,
            sub_import.alias.as_deref(),
            sub_import.span,
            src_root,
            resolved,
            visited,
            visiting,
            errors,
        );
    }

    visiting.pop();
    visited.insert(path.to_string());

    let name = alias.unwrap_or(path).to_string();
    resolved.push(ResolvedModule {
        name,
        path: module_path,
        source,
        program,
    });
}

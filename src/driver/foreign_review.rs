//! Resolve the shared review before any semantic pass consumes external claims.
use crate::toolchain::rust_bridge_build::{self, BridgeOptions, review};
use crate::{
    compiler_db::inputs::CompilerInputs,
    parser::ast::{Item, Program},
};
use anyhow::Result;

/// `programs` pairs each program with its symbol namespace (empty for the
/// entry), so reviewed claim identities match the ones build and query bind.
pub(super) fn apply<'a>(
    programs: impl Iterator<Item = (&'a str, &'a mut Program)>,
    inputs: &mut CompilerInputs,
) -> Result<Vec<crate::diagnostics::Diagnostic>> {
    let mut declarations: Vec<_> = programs
        .flat_map(|(namespace, p)| p.items.iter_mut().map(move |item| (namespace, item)))
        .filter_map(|(namespace, item)| {
            if let Item::Function(f) = item {
                f.rust_bridge
                    .as_deref_mut()
                    .map(|symbol| (namespace, symbol))
            } else {
                None
            }
        })
        .collect();
    if !declarations.iter().any(|(_, s)| s.has_assumptions()) {
        return Ok(Vec::new());
    }
    let root = inputs
        .query_cache_root
        .as_deref()
        .unwrap_or(&inputs.project_root);
    let mut inspection_error = None;
    let current = if inputs.project_mode {
        let manifest = crate::project::ProjectManifest::load(&root.join("project.toml"))?;
        let mut options = BridgeOptions::new(inputs.options.target.build_mode)?;
        options.offline = inputs.options.offline;
        // A Git verification tree need not contain generated .willow locks.
        // Read-only resolution may reconstruct one; the fingerprint still must
        // match the human-reviewed exact resolution before any claim is active.
        options.locked = inputs.options.locked && root.join(".willow/rust/Cargo.lock").is_file();
        options.read_only = true;
        options.symbols = declarations
            .iter()
            .map(|(namespace, symbol)| {
                let mut symbol = (**symbol).clone();
                if !namespace.is_empty() {
                    symbol.willow_function = crate::qualified_name::QualifiedName::join(
                        namespace,
                        &symbol.willow_function,
                    );
                }
                symbol
            })
            .collect();
        // Metadata never compiles or executes the adapter. A missing toolchain or
        // unresolved dependency cannot turn an unchecked assertion into evidence.
        match rust_bridge_build::inspect_bridge(&manifest, root, &options) {
            Ok(build) => build
                .filter(|build| {
                    review::recorded(root).ok().flatten().as_deref()
                        == Some(&build.review_fingerprint)
                })
                .map(|build| build.review_fingerprint),
            Err(error) => {
                // Still fail closed, but keep the reason the review cannot be
                // current (for example an uncovered source layout).
                inspection_error = Some(format!("{error:#}"));
                None
            }
        }
    } else {
        None
    };
    inputs.capture_analysis |= current.is_some();
    let mut diagnostics = Vec::new();
    for (_, symbol) in &mut declarations {
        let valid = current.is_some();
        symbol.apply_review(valid);
        if !valid && symbol.has_assumptions() {
            use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
            let mut diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E2612,
                format!("rust_nonblocking_review_required: external assumptions for `{}` are inactive", symbol.willow_function))
                .with_help("Review the current sources and dependencies with `willow query rust-bridge --format json`; explicitly update rust-review.toml. No claim is effective until the review is current.");
            if let Some(error) = &inspection_error {
                diagnostic = diagnostic.with_note(error.clone());
            }
            if let Some(span) = symbol.assumption_span {
                diagnostic = diagnostic.with_label(Label::primary(span, "missing or stale review"));
            }
            diagnostics.push(diagnostic);
        }
    }
    Ok(diagnostics)
}

/// Emit successful contract dependencies using the same graph and vocabulary as queries.
pub(super) fn diagnostics(
    frontend: &crate::Frontend,
    snapshot: &crate::ai::Snapshot,
    entry: &crate::diagnostics::SourceMap,
) -> Vec<crate::diagnostics::Diagnostic> {
    use crate::diagnostics::{Diagnostic, ErrorCode, FileId, Label, Severity, Span};
    use std::collections::HashMap;
    let paths: HashMap<_, _> = std::iter::once((entry.path.as_str(), FileId::ENTRY))
        .chain(
            frontend
                .module_graph
                .files
                .iter()
                .filter_map(|f| f.path.to_str().map(|p| (p, FileId(f.id.0)))),
        )
        .collect();
    let span = |location: &crate::ai::Location| {
        paths
            .get(location.path.as_str())
            .map(|&file| Span::in_file(file, location.start, location.end, 0, 0))
    };
    let functions: HashMap<_, _> = snapshot
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id.as_str(), i))
        .collect();
    let analysis = crate::ai::assumptions::Analysis::new(snapshot);
    let mut result = Vec::new();
    for symbol in &snapshot.semantic.symbols {
        let Some(&i) = functions.get(symbol.id.as_str()) else {
            continue;
        };
        if symbol.details.effect_contracts.is_empty() {
            continue;
        }
        let evidence = analysis.output(i);
        for contract in &symbol.details.effect_contracts {
            let value = &evidence[contract];
            if value.is_null() {
                continue;
            }
            let Some(at) = symbol.location.as_ref().and_then(span) else {
                continue;
            };
            let Some(assumption) =
                serde_json::from_value::<crate::ai::Location>(value["location"].clone())
                    .ok()
                    .and_then(|l| span(&l))
            else {
                continue;
            };
            result.push(
                Diagnostic::new(
                    Severity::Warning,
                    ErrorCode::W2613,
                    format!(
                        "effect contract `{contract}` for `{}` relies on an assumption",
                        symbol.name
                    ),
                )
                .with_label(Label::primary(at, "contract depends on external review"))
                .with_label(Label::secondary(assumption, "assumption declared here"))
                .with_note("evidence: assumption; not compiler-proven"),
            );
        }
    }
    result
}

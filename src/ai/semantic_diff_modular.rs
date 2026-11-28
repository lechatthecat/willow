//! Contract snapshots use the normal typed module graph and resolved call IDs.
use super::*;
use crate::{
    diagnostics::{Diagnostic, DiagnosticEmitter, Severity, source_map::SourceLookup},
    module::UnitId,
    parser::ast::*,
};
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
struct Diagnostics(Vec<Diagnostic>);
impl DiagnosticEmitter for Diagnostics {
    fn emit(&mut self, diagnostic: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
        self.0.push(diagnostic.clone());
        Ok(())
    }
}
pub(super) fn capture(
    inputs: &tree::Tree,
    entry: &Path,
    cache: &mut ProofCache,
) -> Result<comparison::Snapshot> {
    let scratch = tree::Scratch::new()?;
    scratch.write(&tree::Tree::new(), inputs)?;
    let path = scratch.0.join(entry);
    let source = std::fs::read_to_string(&path)?;
    let root = path.parent().context("entry parent")?;
    let map = crate::diagnostics::SourceMap::new(path.to_str().context("entry UTF-8")?, &source);
    let symbols = crate::semantic::ids::SymbolInterner::new();
    let _symbols = symbols.enter();
    let mut options = crate::compiler_db::inputs::CompilerInputs::native(
        crate::CompilerOptions::debug(),
        root.to_owned(),
    )
    .resolve_project_analysis(
        inputs
            .contains_key("project.toml")
            .then_some(scratch.0.as_path()),
        crate::package::LockPublication::Preserve,
    )?;
    options.verification = true;
    options.capture_analysis = true;
    options.proof_cache = Some(std::mem::take(cache));
    if options.project_mode {
        let (project, source_root) = crate::driver::discovery_roots(&options, &scratch.0, root);
        options.check_modules = crate::package::discover_sources(&project, &source_root)?;
    }
    let mut diagnostics = Diagnostics::default();
    let frontend =
        crate::run_frontend_revision(&source, root, &map, options, &mut diagnostics, None, None)?;
    let artifacts = frontend
        .module_graph
        .artifacts
        .as_ref()
        .context("source artifacts")?;
    let mut program = Program {
        items: Vec::new(),
        imports: Vec::new(),
        type_uses: Vec::new(),
        instantiation_types: Vec::new(),
        module: None,
    };
    let mut targets = HashMap::new();
    let mut panic_evidence = HashMap::new();
    for (unit, declarations) in frontend
        .module_graph
        .files
        .iter()
        .map(|m| (m.id, &m.program))
        .chain(std::iter::once((UnitId::ENTRY, &frontend.program)))
    {
        let mut body = artifacts.hydrate(declarations, unit.file_id())?;
        let checked = frontend.db.checked_unit(unit, artifacts)?;
        let imports = body
            .imports
            .iter()
            .map(|i| {
                (
                    i.alias.clone().unwrap_or_else(|| {
                        crate::qualified_name::QualifiedName::new(&i.path)
                            .last_segment()
                            .to_owned()
                    }),
                    i.path.clone(),
                )
            })
            .collect();
        let world = frontend.db.effects.proofs.borrow();
        if let Some(capture) = frontend
            .db
            .effects
            .analysis
            .borrow()
            .as_ref()
            .and_then(|a| a.get(&unit))
        {
            panic_evidence.extend(
                capture
                    .semantic
                    .panic_evidence
                    .iter()
                    .map(|(id, evidence)| {
                        (
                            crate::semantic::ids::FunctionId::free(
                                world.name(unit, &id.to_string()),
                            ),
                            evidence.clone(),
                        )
                    }),
            );
        }
        for (&expr, target) in &checked.analysis_calls {
            if let Some(target) = target {
                let (owner, target) = frontend
                    .db
                    .effects
                    .external_target(unit, target, &imports)
                    .unwrap_or((unit, *target));
                if target.owner().is_none() {
                    targets.insert(expr, world.name(owner, &target.name()));
                }
            }
        }
        for item in &mut body.items {
            if let Item::Function(f) = item {
                f.name = world.name(unit, &f.name);
            }
        }
        program.items.append(&mut body.items);
    }
    let mut world = frontend.db.effects.proofs.borrow_mut();
    *cache = std::mem::take(&mut world.cache);
    let verification = verification::Verification {
        reports: std::mem::take(&mut world.reports),
        ..Default::default()
    };
    let errors = diagnostics
        .0
        .iter()
        .filter(|d| {
            d.severity == Severity::Error
                && !matches!(
                    d.code,
                    crate::diagnostics::ErrorCode::E2630
                        | crate::diagnostics::ErrorCode::E2631
                        | crate::diagnostics::ErrorCode::E2632
                )
        })
        .map(|d| d.message.clone())
        .collect();
    Ok(comparison::from_checked(
        program,
        targets,
        verification,
        errors,
        BTreeSet::new(),
        &panic_evidence,
    ))
}

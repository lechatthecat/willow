//! Session-owned modular summaries. Imported verified bodies are never expanded
//! into callers; only their proved contracts and contract dependencies are loaded.
use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use std::sync::Arc;

pub(super) struct Export {
    pub function: FunctionDecl,
    pub definitions: String,
    pub digest: String,
    pub dependencies: Vec<String>,
    pub error: Option<VerificationError>,
}
impl Export {
    pub(super) fn new(
        f: &FunctionDecl,
        summary: &Summary,
        digest: String,
        dependencies: Vec<String>,
    ) -> Self {
        let mut function = f.clone();
        function.body.stmts.clear();
        function.requires.clear();
        function.ensures.clear();
        let definitions = if has(f, EffectContractKind::Verified) {
            let params = f
                .params
                .iter()
                .map(|p| sort(&p.ty).expect("encoded parameter"))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "{}(declare-fun f{}_contract ({params}) {})\n",
                summary.contract_definitions,
                symbol(f),
                sort(&f.return_type).expect("encoded return")
            )
        } else {
            summary.definitions.clone()
        };
        Self {
            function,
            definitions,
            digest,
            dependencies,
            error: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct World {
    #[cfg(test)]
    pub import_visits: std::cell::Cell<usize>,
    exports: HashMap<String, Arc<Export>>,
    pub cache: ProofCache,
    pub reports: Vec<Report>,
    pub namespaces: HashMap<crate::module::UnitId, String>,
}
impl World {
    pub(crate) fn name(&self, unit: crate::module::UnitId, name: &str) -> String {
        match self.namespaces.get(&unit).filter(|n| !n.is_empty()) {
            Some(namespace) => format!("{namespace}::{name}"),
            None => name.to_owned(),
        }
    }
    pub(super) fn insert(&mut self, export: Export) {
        self.exports
            .insert(export.function.name.clone(), Arc::new(export));
    }
    pub(super) fn fail(&mut self, f: &FunctionDecl, message: String) {
        let mut function = f.clone();
        function.body.stmts.clear();
        function.requires.clear();
        function.ensures.clear();
        self.insert(Export {
            function,
            definitions: String::new(),
            digest: String::new(),
            dependencies: Vec::new(),
            error: Some(FailureKind::Unsupported.error(message)),
        });
    }
    pub(super) fn imports(
        &self,
        program: &Program,
        targets: Option<&HashMap<ExprId, String>>,
    ) -> Vec<Arc<Export>> {
        let local: BTreeSet<_> = program
            .items
            .iter()
            .filter_map(|i| {
                if let Item::Function(f) = i {
                    Some(f.name.as_str())
                } else {
                    None
                }
            })
            .collect();
        let mut roots = BTreeSet::new();
        for item in &program.items {
            if let Item::Function(f) = item
                && (has(f, EffectContractKind::Pure) || has(f, EffectContractKind::Verified))
            {
                for event in AstWalk::new(AstEvent::Block(&f.body)).chain(
                    f.requires
                        .iter()
                        .chain(&f.ensures)
                        .flat_map(|e| AstWalk::new(AstEvent::Expr(e))),
                ) {
                    if let AstEvent::Expr(expr) = event {
                        let name = targets
                            .and_then(|t| t.get(&expr.id()))
                            .map(String::as_str)
                            .or({
                                if let Expr::Call(c) = expr {
                                    Some(c.callee.as_str())
                                } else {
                                    None
                                }
                            });
                        if let Some(name) = name
                            && !local.contains(name)
                            && self.exports.contains_key(name)
                        {
                            roots.insert(name.to_owned());
                        }
                    }
                }
            }
        }
        // Pure-only providers compile summaries without a solver. Their direct
        // imported signatures suffice; defer transitive SMT closure loading to
        // a consumer that actually proves a verified function.
        let transitive = program
            .items
            .iter()
            .any(|i| matches!(i, Item::Function(f) if has(f, EffectContractKind::Verified)));
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        let mut stack: Vec<_> = roots.into_iter().rev().map(|n| (n, false)).collect();
        while let Some((name, exit)) = stack.pop() {
            let Some(export) = self.exports.get(&name) else {
                continue;
            };
            if exit {
                #[cfg(test)]
                self.import_visits.set(self.import_visits.get() + 1);
                result.push(Arc::clone(export));
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            stack.push((name, true));
            if !transitive {
                continue;
            }
            stack.extend(
                export
                    .dependencies
                    .iter()
                    .rev()
                    .cloned()
                    .map(|n| (n, false)),
            );
        }
        result
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "unit identity, checked call tables and immutable proof inputs stay borrowed from one checker"
    )]
    pub(crate) fn check(
        &mut self,
        unit: crate::module::UnitId,
        program: &Program,
        previous: &[Diagnostic],
        data_calls: &crate::semantic::intrinsics::BuiltinCalls,
        targets: &HashMap<ExprId, String>,
        writes: &crate::semantic::field_writes::FieldWrites,
        options: &Options,
    ) -> (Vec<Diagnostic>, Verification) {
        if !program.items.iter().any(|i| matches!(i, Item::Function(f) if has(f, EffectContractKind::Pure) || has(f, EffectContractKind::Verified))) {
            return super::check(program, previous, data_calls, writes, options, &mut self.cache);
        }
        let mut projected = program.clone();
        let mut originals = HashMap::new();
        for item in &mut projected.items {
            if let Item::Function(f) = item {
                let qualified = self.name(unit, &f.name);
                originals.insert(qualified.clone(), f.name.clone());
                f.name = qualified;
            }
        }
        let data = DataTypes {
            targets: Some(targets),
            calls: Some(data_calls),
            ..DataTypes::new(&projected, writes)
        };
        let mut cache = std::mem::take(&mut self.cache);
        let (diagnostics, mut verification) =
            check_with_data(&projected, previous, options, &mut cache, &data, Some(self));
        self.cache = cache;
        self.reports.extend(verification.reports.iter().cloned());
        for report in &mut verification.reports {
            if let Some(name) = originals.get(&report.function) {
                report.function = name.clone();
            }
        }
        (diagnostics, verification)
    }
}

#[cfg(test)]
#[path = "modular_tests.rs"]
mod tests;

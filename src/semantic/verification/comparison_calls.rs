//! Load each contract dependency once per isolated revision in a comparison.
use super::*;

// One table spans both revisions (their function indices are disjoint).
// Only full semantic identity, including transitive dependencies, permits
// coupling an abstract result. Equal contracts alone are insufficient.
pub(super) struct Declarations {
    functions: BTreeSet<usize>,
    unchanged: std::collections::HashSet<String>,
    shared: std::collections::HashSet<String>,
}
impl Declarations {
    pub(super) fn new(a: &Snapshot, b: &Snapshot) -> Self {
        // Both semantic maps are ordered. Merge once instead of looking up
        // every old identity in the new tree for each helper/caller.
        let mut unchanged = std::collections::HashSet::new();
        let mut newer = b.semantic_identities.iter();
        let mut current = newer.next();
        for (name, identity) in &a.semantic_identities {
            while current.is_some_and(|(other, _)| other < name) {
                current = newer.next();
            }
            if current == Some((name, identity)) {
                unchanged.insert(name.clone());
            }
        }
        Self {
            functions: BTreeSet::new(),
            unchanged,
            shared: std::collections::HashSet::new(),
        }
    }

    fn result_function(&mut self, f: &FunctionDecl, namespace: &str) -> VerificationResult<String> {
        let sorts = f
            .params
            .iter()
            .map(|p| sort(&p.ty))
            .collect::<Result<Vec<_>, _>>()?;
        let result = sort(&f.return_type)?;
        let name = format!("f{namespace}{}_contract", symbol(f));
        if !self.unchanged.contains(&f.name) {
            return Ok(format!(
                "(declare-fun {name} ({}) {result})\n",
                sorts.join(" ")
            ));
        }
        let shared = format!("fsame{}_contract", symbol(f));
        let mut definitions = String::new();
        if self.shared.insert(f.name.clone()) {
            definitions += &format!("(declare-fun {shared} ({}) {result})\n", sorts.join(" "));
        }
        let args: Vec<_> = (0..sorts.len()).map(|i| format!("p{i}")).collect();
        let params = args
            .iter()
            .zip(sorts)
            .map(|(arg, ty)| format!("({arg} {ty})"))
            .collect::<Vec<_>>()
            .join(" ");
        definitions += &format!(
            "(define-fun {name} ({params}) {result} {})\n",
            application(&shared, &args)
        );
        Ok(definitions)
    }
}

fn contract_edges(
    f: &FunctionDecl,
    names: &HashMap<&str, usize>,
    data: &DataTypes,
) -> BTreeSet<usize> {
    f.requires
        .iter()
        .chain(&f.ensures)
        .flat_map(|e| AstWalk::new(AstEvent::Expr(e)))
        .filter_map(|e| {
            let AstEvent::Expr(e) = e else {
                return None;
            };
            if !matches!(e, Expr::Call(_) | Expr::StaticCall(_)) {
                return None;
            }
            let name = data
                .targets
                .and_then(|t| t.get(&e.id()))
                .map(String::as_str)
                .or({
                    if let Expr::Call(c) = e {
                        Some(c.callee.as_str())
                    } else {
                        None
                    }
                })?;
            names.get(name).copied()
        })
        .collect()
}
pub(super) fn declare(
    root: &FunctionDecl,
    functions: &[&FunctionDecl],
    names: &HashMap<&str, usize>,
    data: &DataTypes,
    reports: &BTreeMap<&str, &Report>,
    emitted: &mut Declarations,
    solver: &mut solver::Solver,
) -> VerificationResult<()> {
    let mut stack: Vec<_> = contract_edges(root, names, data)
        .into_iter()
        .rev()
        .map(|i| (i, false))
        .collect();
    let mut visiting = BTreeSet::new();
    while let Some((id, exit)) = stack.pop() {
        if emitted.functions.contains(&id) {
            continue;
        }
        let f = functions[id];
        if exit {
            let summary = encode(id, functions, names, data)?;
            if has(f, EffectContractKind::Verified) {
                if reports
                    .get(f.name.as_str())
                    .is_none_or(|r| r.status != Status::Proven)
                {
                    return Err(
                        FailureKind::Unsupported.error("comparison dependency is not proven")
                    );
                }
                solver.exchange(&summary.contract_definitions)?;
                solver.exchange(&emitted.result_function(f, data.symbol_namespace)?)?;
            } else {
                if !has(f, EffectContractKind::Pure) {
                    return Err(
                        FailureKind::Unsupported.error("comparison dependency is not @pure")
                    );
                }
                solver.exchange(&summary.definitions)?;
            }
            visiting.remove(&id);
            emitted.functions.insert(id);
            continue;
        }
        if !visiting.insert(id) {
            return Err(FailureKind::Unsupported.error("recursive comparison dependency"));
        }
        stack.push((id, true));
        let deps = if has(f, EffectContractKind::Verified) {
            contract_edges(f, names, data)
        } else {
            encode::resolved_edges(f, names, data.targets)
        };
        stack.extend(deps.into_iter().rev().map(|i| (i, false)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::verification::tests::{options, parse};

    #[test]
    fn contract_comparison_opaque_helper_declarations_are_shared_across_callers() {
        for n in [8, 16, 32] {
            let mut source = "@pure @verified fn helper(x:i64)->bool {return x>=0;}".to_owned();
            for i in 0..n {
                source += &format!(
                    "@pure @verified fn caller{i}(x:i64)->i64 requires helper(x) ensures result==x {{return x;}}"
                );
            }
            let mut cache = ProofCache::default();
            let a = capture(parse(&source), &options(), &mut cache);
            let b = capture(parse(&source), &options(), &mut cache);
            let functions: Vec<_> = a
                .program
                .items
                .iter()
                .chain(&b.program.items)
                .filter_map(|item| {
                    if let Item::Function(f) = item {
                        Some(f)
                    } else {
                        None
                    }
                })
                .collect();
            let mut declarations = Declarations::new(&a, &b);
            let mut solver = solver::Solver::new(&options().solver, options().timeout_ms).unwrap();
            solver.exchange(PRELUDE).unwrap();
            for (snapshot, namespace, offset) in [(&a, "old", 0), (&b, "new", n + 1)] {
                let names: HashMap<_, _> = functions[offset..offset + n + 1]
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (f.name.as_str(), offset + i))
                    .collect();
                let data = DataTypes {
                    symbol_namespace: namespace,
                    ..Default::default()
                };
                let reports = snapshot
                    .verification
                    .reports
                    .iter()
                    .map(|r| (r.function.as_str(), r))
                    .collect();
                for f in &functions[offset..offset + n + 1] {
                    declare(
                        f,
                        &functions,
                        &names,
                        &data,
                        &reports,
                        &mut declarations,
                        &mut solver,
                    )
                    .unwrap();
                }
            }
            assert_eq!(declarations.functions.len(), 2);
            assert_eq!(declarations.shared.len(), 1);
            println!("opaque_helper callers={n} revision_helpers=2 shared_returns=1");
        }
    }
}

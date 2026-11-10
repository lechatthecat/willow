//! Declaration contracts consume the same solved effects as lock checking.
use super::*;
use crate::compiler_db::effects::EffectWitness;
use crate::semantic::call_graph::CallGraph;
use crate::semantic::effects::{EffectFacts, RuntimeEffects};
use std::collections::VecDeque;

#[cfg(test)]
thread_local! {
    static OBLIGATION_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CONTRACT_GRAPH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PATH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A shared next-hop forest per forbidden bit. Each edge is examined once;
/// rendering a diagnostic then costs only the length of its emitted chain.
fn witness_paths(
    graph: &CallGraph,
    facts: &EffectFacts<EffectWitness>,
    asynchronous: &HashMap<FunctionId, bool>,
    bit: RuntimeEffects,
) -> HashMap<FunctionId, (FunctionId, Option<Span>)> {
    let owners: HashMap<_, _> = facts
        .iter()
        .filter_map(|(&id, summary)| {
            #[cfg(test)]
            PATH_WORK.with(|n| n.set(n.get() + 1));
            match summary.witness(bit)? {
                EffectWitness::Lock(w) => Some((id, w.owner)),
                EffectWitness::External(owner) => Some((id, *owner)),
                _ => None,
            }
        })
        .collect();
    let mut reverse: HashMap<FunctionId, Vec<(FunctionId, Option<Span>)>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    for (caller, sites) in graph.iter() {
        #[cfg(test)]
        PATH_WORK.with(|n| n.set(n.get() + 1));
        if owners.get(caller) == Some(caller) {
            visited.insert(*caller);
            queue.push_back(*caller);
        }
        for callee in &sites.targets {
            #[cfg(test)]
            PATH_WORK.with(|n| n.set(n.get() + 1));
            if (bit == RuntimeEffects::MAY_IO || asynchronous.get(callee) != Some(&true))
                && owners.contains_key(caller)
                && owners.get(caller) == owners.get(callee)
            {
                reverse
                    .entry(*callee)
                    .or_default()
                    .push((*caller, sites.call_spans.get(callee).copied()));
            }
        }
    }
    let mut next = HashMap::new();
    while let Some(callee) = queue.pop_front() {
        if let Some(callers) = reverse.get(&callee) {
            for &(caller, span) in callers {
                #[cfg(test)]
                PATH_WORK.with(|n| n.set(n.get() + 1));
                if visited.insert(caller) {
                    next.insert(caller, (callee, span));
                    queue.push_back(caller);
                }
            }
        }
    }
    next
}

impl TypeChecker {
    /// Contract proofs are deliberately separate from the runtime/lock lattice:
    /// an unknown callback must fail a declaration proof even outside a lock.
    /// Arity hub nodes reuse the existing address-taken analysis without a
    /// call-sites × candidate-values expansion.
    fn solve_contracts(
        &self,
        program: &Program,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
        roots: impl Iterator<Item = FunctionId>,
    ) -> (CallGraph, EffectFacts<EffectWitness>) {
        const CONTRACT_EFFECTS: RuntimeEffects = RuntimeEffects::MAY_BLOCK
            .union(RuntimeEffects::MAY_SUSPEND)
            .union(RuntimeEffects::MAY_IO);
        use crate::compiler_db::effects::LOCK_EFFECT_WAIT;
        use crate::semantic::{call_graph::CallSites, effects::EffectProblem};
        let mut graph = graph.clone();
        for call in &self.indirect_lock.calls {
            #[cfg(test)]
            CONTRACT_GRAPH_WORK.with(|n| n.set(n.get() + 1));
            graph.merge(
                call.caller,
                CallSites {
                    targets: [FunctionId::free(format!(
                        "$contract$arity{}",
                        call.bit.trailing_zeros()
                    ))]
                    .into(),
                    // The arity set is conservative, not a proof that an arbitrary
                    // parameter cannot receive an unseen value.
                    has_unknown: true,
                    ..Default::default()
                },
            );
        }
        for value in &self.indirect_lock.values {
            #[cfg(test)]
            CONTRACT_GRAPH_WORK.with(|n| n.set(n.get() + 1));
            graph.merge(
                FunctionId::free(format!("$contract$arity{}", value.bit.trailing_zeros())),
                CallSites {
                    has_unknown: value.target.is_none(),
                    ..Default::default()
                },
            );
        }
        let mut problem = EffectProblem::new()
            .external_callee(CONTRACT_EFFECTS)
            .unknown_callee(CONTRACT_EFFECTS)
            .missing_body(CONTRACT_EFFECTS)
            .default_transmit(CONTRACT_EFFECTS);
        // A matching arity identifies a possible target, not an exact dynamic
        // target. External witnesses sort after proven Lock witnesses, so an
        // uncertain candidate cannot hide an independently proven violation.
        for value in &self.indirect_lock.values {
            let Some(target) = value.target else {
                continue;
            };
            let target = self.effect_index.local_target(target);
            if self.lock_callee_is_async(&target) == Some(true) {
                continue;
            }
            let Some(summary) = facts.get(&target) else {
                continue;
            };
            let hub = FunctionId::free(format!("$contract$arity{}", value.bit.trailing_zeros()));
            for bit in [
                RuntimeEffects::MAY_BLOCK,
                RuntimeEffects::MAY_SUSPEND,
                RuntimeEffects::MAY_IO,
            ] {
                if summary.intersects(bit) {
                    problem = problem.seed(hub, bit, Some(EffectWitness::External(hub)));
                }
            }
        }
        let mut nodes: HashSet<_> = graph.ids().copied().chain(roots).collect();
        for (_, sites) in graph.iter() {
            nodes.extend(&sites.targets);
        }
        // Lambdas and implicit constructors can have empty bodies without a
        // call-graph entry. Their checked summaries still establish purity.
        nodes.extend(
            facts
                .iter()
                .filter_map(|(&id, _)| (id.is_lambda()).then_some(id)),
        );
        let mut normalized = CallGraph::default();
        let mut abi_effects = HashMap::new();
        let builtin_aliases = self
            .imported_std_modules
            .iter()
            .map(|(alias, module)| (alias.clone(), module.module.clone()))
            .collect();
        for (&id, sites) in graph.iter() {
            let mut sites = sites.clone();
            if sites.has_unknown
                && !sites.targets.is_empty()
                && sites.unsupported_initialization.len() == 1
                && sites.unsupported_initialization.contains(
                    &crate::semantic::call_graph::UnsupportedInitialization::InjectedDefault,
                )
            {
                sites.has_unknown = false;
            }
            // A dependency is checked before its consumers. Open dispatch in
            // that dependency is not a closed set: a consumer may introduce a
            // new override. Do not export a proof based on today's subset.
            if self
                .effect_queries
                .as_ref()
                .is_some_and(|(_, unit)| *unit != crate::module::UnitId::ENTRY)
                && sites.virtual_calls.iter().any(|target| {
                    let Some(owner) = target.owner_type().map(|ty| ty.to_string()) else {
                        return true;
                    };
                    if self.symbols.lookup_interface(&owner).is_some() {
                        return true;
                    }
                    self.symbols.lookup_class(&owner).is_none_or(|class| {
                        class.is_open
                            && self
                                .resolved_method_class(&owner, &target.name())
                                .and_then(|declaring| self.symbols.lookup_class(&declaring))
                                .and_then(|class| class.methods.get(target.name().as_ref()))
                                .is_none_or(|method| method.is_open)
                    })
                })
            {
                sites.has_unknown = true;
            }
            normalized.merge(id, sites);
        }
        #[cfg(test)]
        CONTRACT_GRAPH_WORK.with(|n| {
            n.set(
                n.get()
                    + nodes.len()
                    + normalized
                        .iter()
                        .map(|(_, sites)| sites.targets.len())
                        .sum::<usize>(),
            )
        });
        let foreign: HashMap<_, _> = program
            .items
            .iter()
            .filter_map(|item| {
                let Item::Function(f) = item else {
                    return None;
                };
                f.rust_bridge.as_ref()?;
                Some((
                    FunctionId::free(&f.name),
                    f.effect_contracts
                        .iter()
                        .any(|c| c.kind == EffectContractKind::NoIo),
                ))
            })
            .collect();
        for id in nodes {
            if let Some(no_io) = foreign.get(&id) {
                problem = problem.seed(
                    id,
                    if *no_io {
                        LOCK_EFFECT_WAIT
                    } else {
                        CONTRACT_EFFECTS
                    },
                    None,
                );
                continue;
            }
            let local = graph.get(&id).is_some()
                || id.is_lambda()
                || (id.name().as_ref() == "init"
                    && id.owner().is_some_and(|owner| {
                        self.symbols
                            .lookup_class(&owner)
                            .is_some_and(|_| id.namespace().is_none())
                    }));
            let summary = if local {
                facts.get(&id).cloned()
            } else {
                self.effect_queries.as_ref().and_then(|(queries, unit)| {
                    let (owner, target) =
                        queries.external_target(*unit, &id, &self.effect_index.imports)?;
                    queries
                        .contract_facts
                        .borrow()
                        .get(&owner)?
                        .get(&target)
                        .cloned()
                })
            };
            if !local
                && let Some(effects) = crate::semantic::intrinsics::builtin_target_effects(&id)
                    .or_else(|| {
                        crate::semantic::intrinsics::builtin_static_effects(&id, &builtin_aliases)
                    })
            {
                abi_effects.insert(id, effects);
                problem = problem.seed(id, effects.intersection(CONTRACT_EFFECTS), None);
                continue;
            }
            if let Some(summary) = summary {
                problem = problem.seed(id, RuntimeEffects::NONE, None);
                for bit in [
                    RuntimeEffects::MAY_BLOCK,
                    RuntimeEffects::MAY_SUSPEND,
                    RuntimeEffects::MAY_IO,
                ] {
                    if summary.intersects(bit) {
                        let witness = summary.witness(bit).cloned().filter(|w| {
                            w.lock()
                                .is_some_and(|c| !c.is_conservative && c.operation != "extern rust")
                        });
                        // A solved runtime bound may include unavailable imported
                        // evidence. Re-resolve those edges against current proofs;
                        // only proven local waits are valid additional seeds.
                        if !local || witness.is_some() {
                            problem = problem.seed(id, bit, witness);
                        }
                    }
                }
                if summary
                    .witness(LOCK_EFFECT_WAIT)
                    .and_then(EffectWitness::lock)
                    .is_some_and(|c| c.operation == "extern rust")
                {
                    problem = problem.seed(id, LOCK_EFFECT_WAIT, None);
                }
            } else if id.name().starts_with("$contract$arity") {
                problem = problem.body(id);
            } else {
                problem = problem.seed(id, CONTRACT_EFFECTS, None);
            }
            // Interface union nodes with no implementation do not prove a
            // signature merely because an empty runtime fact was allocated.
            if let Some(owner) = id.owner()
                && self.symbols.lookup_interface(&owner).is_some()
                && !id.name().starts_with("$default$")
                && graph.get(&id).is_none_or(|sites| sites.targets.is_empty())
            {
                problem = problem.seed(id, CONTRACT_EFFECTS, None);
            }
            if self.lock_callee_is_async(&id) == Some(true) {
                problem = problem.transmit(id, RuntimeEffects::MAY_IO);
            }
        }
        // ABI effects are proved at each actual source call, not at a shared
        // synthetic leaf whose representative span might belong to another caller.
        for (&caller, sites) in normalized.iter() {
            for (target, span) in &sites.call_spans {
                let Some(effects) = abi_effects
                    .get(target)
                    .filter(|e| **e != RuntimeEffects::ALL)
                else {
                    continue;
                };
                for (bit, kind) in [
                    (RuntimeEffects::MAY_BLOCK, LockEffectKind::Block),
                    (RuntimeEffects::MAY_SUSPEND, LockEffectKind::Suspend),
                    (RuntimeEffects::MAY_IO, LockEffectKind::Io),
                ] {
                    if effects.intersects(bit) {
                        problem = problem.seed(
                            caller,
                            bit,
                            Some(EffectWitness::Lock(
                                crate::compiler_db::effects::LockEffectWitness {
                                    owner: caller,
                                    cause: LockEffectCause {
                                        is_conservative: false,
                                        span: *span,
                                        operation: target.to_string().into(),
                                        kind,
                                        lock: None,
                                    },
                                },
                            )),
                        );
                    }
                }
            }
        }
        let facts = problem.solve_reachable(&normalized);
        (normalized, facts)
    }

    /// Propagate each declaration contract kind once per (type, method).
    /// In particular, a deep chain never rescans all its ancestors per method.
    fn contract_obligations(
        &mut self,
        program: &Program,
        templates: bool,
    ) -> Vec<(FunctionId, EffectContract)> {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut obligations: HashMap<String, HashMap<String, [Option<EffectContract>; 6]>> =
            HashMap::new();
        let mut queue = VecDeque::new();
        let mut types = HashSet::new();
        let canonical = |name: &str| {
            self.symbols
                .lookup_class(name)
                .map(|c| c.name.clone())
                .or_else(|| self.symbols.lookup_interface(name).map(|i| i.name.clone()))
                .unwrap_or_else(|| name.to_string())
        };
        // Only local declarations and their ancestors can impose an obligation
        // in this unit. Do not rescan unrelated imported type families.
        let mut pending: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) if c.type_params.is_empty() != templates => Some(canonical(&c.name)),
                Item::Interface(i) if i.type_params.is_empty() != templates => {
                    Some(canonical(&i.name))
                }
                _ => None,
            })
            .collect();
        while let Some(name) = pending.pop() {
            if !types.insert(name.clone()) {
                continue;
            }
            if let Some(class) = self.symbols.lookup_class(&name) {
                for parent in class.base_class.iter().map(String::as_str).chain(
                    class.implements.iter().filter_map(|ty| match ty {
                        Type::Named(name) | Type::Generic(name, _) => Some(name.as_str()),
                        _ => None,
                    }),
                ) {
                    let parent = canonical(parent);
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(name.clone());
                    pending.push(parent);
                }
                for (method, info) in &class.methods {
                    if !info.is_static {
                        for contract in &info.effect_contracts {
                            queue.push_back((name.clone(), method.clone(), contract.clone()));
                        }
                    }
                }
            } else if let Some(interface) = self.symbols.lookup_interface(&name) {
                for parent in &interface.extends {
                    let parent = canonical(parent);
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(name.clone());
                    pending.push(parent);
                }
                for (method, info) in &interface.methods {
                    for contract in &info.effect_contracts {
                        queue.push_back((name.clone(), method.clone(), contract.clone()));
                    }
                }
            }
        }
        // Stable declaration locations make inherited diagnostic labels stable
        // even when several ancestors impose the same contract.
        queue.make_contiguous().sort_by_key(|(owner, method, c)| {
            (c.span.file_id, c.span.start, owner.clone(), method.clone())
        });
        while let Some((owner, method, contract)) = queue.pop_front() {
            #[cfg(test)]
            OBLIGATION_WORK.with(|n| n.set(n.get() + 1));
            let index = match contract.kind {
                EffectContractKind::Pure => 3,
                EffectContractKind::Verified => 4,
                EffectContractKind::NoWait => 0,
                EffectContractKind::NoBlock => 1,
                EffectContractKind::NoSuspend => 2,
                EffectContractKind::NoIo => 5,
            };
            let entry = obligations
                .entry(owner.clone())
                .or_default()
                .entry(method.clone())
                .or_default();
            if entry[index].is_some() {
                continue;
            }
            entry[index] = Some(contract.clone());
            for child in children.get(&owner).into_iter().flatten() {
                queue.push_back((child.clone(), method.clone(), contract.clone()));
            }
        }
        let mut declarations = Vec::new();
        let mut diagnostics = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) if f.type_params.is_empty() != templates => {
                    declarations.extend(
                        f.effect_contracts
                            .iter()
                            .cloned()
                            .map(|c| (FunctionId::free(&f.name), c)),
                    );
                }
                Item::Class(class) if class.type_params.is_empty() != templates => {
                    let owner = canonical(&class.name);
                    for method in &class.methods {
                        let id = FunctionId::method(TypeId::local(&class.name), &method.name);
                        declarations
                            .extend(method.effect_contracts.iter().cloned().map(|c| (id, c)));
                    }
                    // Include inherited implementations, even if the class adds
                    // an interface without declaring a new method body.
                    let explicit_methods: HashMap<_, _> =
                        class.methods.iter().map(|m| (m.name.as_str(), m)).collect();
                    let mut required: Vec<_> = obligations
                        .get(&owner)
                        .into_iter()
                        .flat_map(|m| m.iter())
                        .collect();
                    required.sort_by_key(|(method, _)| *method);
                    for (method, contracts) in required {
                        let Some(declaring) = self.resolved_method_class(&class.name, method)
                        else {
                            continue;
                        };
                        let id = self.effect_index.local_target(FunctionId::method(
                            TypeId::from_source_name(&declaring),
                            method,
                        ));
                        let explicit = explicit_methods.get(method.as_str()).copied();
                        for contract in contracts.iter().flatten() {
                            if let Some(m) = explicit
                                && !m.effect_contracts.is_empty()
                                && !m
                                    .effect_contracts
                                    .iter()
                                    .fold(RuntimeEffects::NONE, |bits, c| {
                                        bits.union(c.kind.forbidden())
                                    })
                                    .contains(contract.kind.forbidden())
                            {
                                diagnostics.push(Diagnostic::new(Severity::Error, ErrorCode::E2611, format!("override `{id}` weakens inherited effect contract `{}`", contract.kind.name()))
                                        .with_label(Label::primary(m.span, "override must preserve all inherited prohibitions"))
                                        .with_label(Label::secondary(contract.span, "inherited contract declared here")));
                            }
                            declarations.push((id, contract.clone()));
                        }
                    }
                }
                Item::Interface(interface) if interface.type_params.is_empty() != templates => {
                    let owner = canonical(&interface.name);
                    for method in &interface.methods {
                        let inherited = obligations.get(&owner).and_then(|m| m.get(&method.name));
                        for contract in inherited.into_iter().flatten().flatten() {
                            if !method.effect_contracts.is_empty()
                                && !method
                                    .effect_contracts
                                    .iter()
                                    .fold(RuntimeEffects::NONE, |bits, c| {
                                        bits.union(c.kind.forbidden())
                                    })
                                    .contains(contract.kind.forbidden())
                            {
                                diagnostics.push(Diagnostic::new(Severity::Error, ErrorCode::E2611, format!("interface method `{}::{}` weakens an inherited effect contract", interface.name, method.name))
                                    .with_label(Label::primary(method.span, "weakened contract"))
                                    .with_label(Label::secondary(contract.span, "inherited contract declared here")));
                            }
                            if method.default_body.is_some() {
                                declarations.push((
                                    interface_default_effect_id(&interface.name, &method.name),
                                    contract.clone(),
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for diagnostic in diagnostics {
            self.push(diagnostic);
        }
        declarations.sort_by_key(|(id, c)| (*id, c.kind.name(), c.span.file_id, c.span.start));
        declarations.dedup_by_key(|(id, c)| (*id, c.kind.name()));
        declarations
    }

    pub(super) fn report_effect_contracts(
        &mut self,
        program: &Program,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
        templates: bool,
    ) {
        let mut declarations = self.contract_obligations(program, templates);
        self.report_purity(program, graph, &declarations, templates);
        declarations.retain(|(_, c)| {
            !matches!(
                c.kind,
                EffectContractKind::Pure | EffectContractKind::Verified
            )
        });
        let (graph, contract_facts) = self.solve_contracts(
            program,
            graph,
            facts,
            declarations.iter().map(|(id, _)| *id),
        );
        let contract_facts = std::sync::Arc::new(contract_facts);
        let facts = &contract_facts;
        let graph = &graph;
        if !templates && let Some((queries, unit)) = &self.effect_queries {
            queries
                .contract_facts
                .borrow_mut()
                .insert(*unit, std::sync::Arc::clone(&contract_facts));
        }
        if declarations.is_empty() {
            return;
        }
        let summaries: HashMap<_, _> = facts.iter().map(|(&id, summary)| (id, summary)).collect();
        let bits = [
            RuntimeEffects::MAY_BLOCK,
            RuntimeEffects::MAY_SUSPEND,
            RuntimeEffects::MAY_IO,
        ];
        let paths = bits.map(|bit| witness_paths(graph, facts, &self.effect_index.callables, bit));
        for (id, contract) in declarations {
            let Some(summary) = summaries.get(&id) else {
                self.push(Diagnostic::new(Severity::Error, ErrorCode::E2612,
                        format!("effect contract `{}` cannot be proven for `{id}`: missing verified summary", contract.kind.name()))
                        .with_label(Label::primary(contract.span, "effect contract declared here")));
                continue;
            };
            // Prefer a proven violation over uncertainty in another bit.
            let matching = |bit: &RuntimeEffects| {
                contract.kind.forbidden().intersects(*bit) && summary.intersects(*bit)
            };
            let Some(index) = bits
                .iter()
                .position(|bit| {
                    matching(bit)
                        && summary
                            .witness(*bit)
                            .and_then(EffectWitness::lock)
                            .is_some_and(|c| !c.is_conservative)
                })
                .or_else(|| bits.iter().position(matching))
            else {
                continue;
            };
            let bit = bits[index];
            let cause = summary.witness(bit).and_then(EffectWitness::lock);
            let proven = cause.is_some_and(|c| !c.is_conservative);
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                if proven {
                    ErrorCode::E2610
                } else {
                    ErrorCode::E2612
                },
                format!(
                    "effect contract `{}` {} `{id}`",
                    contract.kind.name(),
                    if proven {
                        "violated by"
                    } else {
                        "cannot be proven for"
                    }
                ),
            )
            .with_label(Label::secondary(
                contract.span,
                "effect contract declared here",
            ));
            let mut chain = vec![id.to_string()];
            let mut cursor = id;
            while let Some(&(callee, span)) = paths[index].get(&cursor) {
                if let Some(span) = span {
                    diagnostic = diagnostic.with_label(Label::secondary(
                        span,
                        format!("`{cursor}` calls `{callee}`"),
                    ));
                }
                chain.push(callee.to_string());
                cursor = callee;
            }
            let certainty = match cause {
                Some(cause) => {
                    diagnostic = diagnostic.with_label(Label::primary(
                        cause.span,
                        format!(
                            "{} operation `{}`",
                            if cause.is_conservative {
                                "possible"
                            } else {
                                "root"
                            },
                            cause.operation
                        ),
                    ));
                    chain.push(cause.operation.to_string());
                    if cause.is_conservative {
                        "conservative-bound"
                    } else {
                        "compiler-proven"
                    }
                }
                None => {
                    diagnostic = diagnostic.with_label(Label::primary(
                        contract.span,
                        "effect has no available source witness",
                    ));
                    "conservative-bound"
                }
            };
            diagnostic = diagnostic.with_note(format!(
                "certainty: {certainty}; call chain: {}",
                chain.join(" -> ")
            ));
            self.push(diagnostic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::check::check_source;
    use super::*;

    #[test]
    fn io_contract_graph_work_is_bounded() {
        for shape in ["chain", "fanout", "cycle"] {
            for n in [8, 32, 128, 512] {
                let mut source = String::from("fn leaf() { println(1); } ");
                for i in 0..n {
                    let next = if shape == "fanout" || i == 0 {
                        "leaf".to_owned()
                    } else {
                        format!("node_{}", i - 1)
                    };
                    source.push_str(&format!(
                        "fn node_{i}() {{ {next}(); {} }} ",
                        if shape == "cycle" && i == 0 {
                            format!("node_{}();", n - 1)
                        } else {
                            String::new()
                        }
                    ));
                }
                source.push_str(&format!(
                    "@effects(no_io) fn guard() {{ node_{}(); }}",
                    n - 1
                ));
                CONTRACT_GRAPH_WORK.with(|v| v.set(0));
                PATH_WORK.with(|v| v.set(0));
                crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.set(0));
                let errors = check_source(&source);
                assert_eq!(
                    errors.iter().filter(|d| d.code == ErrorCode::E2610).count(),
                    1,
                    "{shape} {n}: {errors:?}"
                );
                let graph = CONTRACT_GRAPH_WORK.with(|v| v.get());
                let paths = PATH_WORK.with(|v| v.get());
                let visits = crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.get());
                assert!(graph <= 12 * (n + 4), "{shape} {n} graph={graph}");
                assert!(paths <= 30 * (n + 4), "{shape} {n} paths={paths}");
                assert!(visits <= 12 * (n + 4), "{shape} {n} edges={visits}");
                println!(
                    "io-contract shape={shape} nodes={n} graph={graph} paths={paths} edge_visits={visits}"
                );
            }
        }
    }

    #[test]
    fn io_contract_std_resource_catalog_is_complete() {
        for module in crate::stdlib_schema::STDLIB_SCHEMA {
            match module.name {
                "fs" | "net" | "env" | "time" => {}
                // Console output is syntax, tested through its overload ABI inventory.
                "io" => {
                    assert_eq!(
                        module
                            .items
                            .iter()
                            .map(|item| item.name)
                            .collect::<Vec<_>>(),
                        ["println", "print", "eprintln"]
                    );
                    continue;
                }
                // Pure value modules, or callback-driven effects classified at call sites.
                "text" | "collections" | "option" | "result" | "parallel" => continue,
                other => panic!("classify external access for new standard module {other}"),
            }
            for item in module.items {
                if !matches!(
                    item.kind,
                    crate::stdlib_schema::StdItemKind::Function { .. }
                ) {
                    continue;
                }
                let builtin =
                    crate::semantic::intrinsics::namespace_builtin(module.name, item.name)
                        .unwrap_or_else(|| panic!("unclassified {}::{}", module.name, item.name));
                assert!(
                    willow_abi::runtime_symbol(builtin.runtime)
                        .unwrap()
                        .effects()
                        .contains(RuntimeEffects::MAY_IO),
                    "{}::{}",
                    module.name,
                    item.name
                );
            }
        }
    }

    #[test]
    fn io_contract_permission_matrix() {
        let cases = [
            (
                "defer",
                "@effects(no_io) fn f() { defer println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "synchronization",
                "@effects(no_io) fn f(c:BlockingCell<i64>) { c.get(); }",
                None,
            ),
            ("empty", "@effects(no_io) fn f() {}", None),
            (
                "arithmetic",
                "@effects(no_io) fn f(x:i64)->i64 { return x+1; }",
                None,
            ),
            (
                "allocation",
                "@effects(no_io) fn f()->String { return 3.toString(); }",
                None,
            ),
            (
                "print",
                "@effects(no_io) fn f() { print(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "println",
                "@effects(no_io) fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "transitive",
                "fn leaf() { println(1); } fn relay() { leaf(); } @effects(no_io) fn f() { relay(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "recursive",
                "fn leaf(x:bool) { if x { leaf(false); } else { println(1); } } @effects(no_io) fn f() { leaf(true); }",
                Some(ErrorCode::E2610),
            ),
            (
                "pure helper",
                "fn leaf() {} @effects(no_io) fn f() { leaf(); }",
                None,
            ),
            (
                "unknown foreign",
                "extern rust { fn foreign(); } @effects(no_io) fn f() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "declared foreign",
                "extern rust { @effects(no_io) fn foreign(); } @effects(no_io) fn f() { foreign(); }",
                None,
            ),
            (
                "foreign relay",
                "extern rust { fn foreign(); } fn relay() { foreign(); } @effects(no_io) fn f() { relay(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "declared relay",
                "extern rust { @effects(no_io) fn foreign(); } fn relay() { foreign(); } @effects(no_io) fn f() { relay(); }",
                None,
            ),
            (
                "unused foreign",
                "extern rust { fn foreign(); } @effects(no_io) fn f() {}",
                None,
            ),
            (
                "callback unknown",
                "@effects(no_io) fn f(cb:fn()) { cb(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "constructor",
                "class C { pub init(self) { println(1); } } @effects(no_io) fn f() { let c = new C(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "method",
                "class C { pub fn out(self) { println(1); } } @effects(no_io) fn f(c:C) { c.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "override",
                "open class B { pub open fn out(self) {} } class C extends B { pub override fn out(self) { println(1); } } @effects(no_io) fn f(b:B) { b.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "interface",
                "interface I { fn out(self); } class C implements I { pub fn out(self) { println(1); } } @effects(no_io) fn f(i:I) { i.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "inherited obligation",
                "open class B { @effects(no_io) pub open fn out(self) {} } class C extends B { pub override fn out(self) { println(1); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "combined",
                "@effects(no_io,no_block) fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "async body",
                "@effects(no_io) async fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "async callee",
                "async fn leaf() { println(1); } @effects(no_io) async fn f() { await leaf(); }",
                Some(ErrorCode::E2610),
            ),
        ];
        for (label, source, expected) in cases {
            let errors: Vec<_> = check_source(source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if let Some(code) = expected {
                assert!(errors.iter().any(|d| d.code == code), "{label}: {errors:?}");
                if code == ErrorCode::E2610 {
                    assert!(
                        errors.iter().any(|d| d.code == code
                            && d.labels
                                .iter()
                                .any(|label| label.message.starts_with("root operation"))),
                        "{label}: missing source witness: {errors:?}"
                    );
                }
                assert!(
                    errors.iter().all(|d| matches!(
                        d.code,
                        ErrorCode::E2610 | ErrorCode::E2611 | ErrorCode::E2612
                    )),
                    "{label}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{label}: {errors:?}");
            }
        }
    }

    #[test]
    fn effect_contract_proven_wait_dominates_uncertain_value() {
        let errors = check_source(
            "fn wait() { println(1); } @effects(no_wait) fn guard() { let f = wait; f(); println(2); }",
        );
        assert!(
            errors.iter().any(|d| d.code == ErrorCode::E2610),
            "{errors:?}"
        );
        assert!(
            !errors.iter().any(|d| d.code == ErrorCode::E2612),
            "{errors:?}"
        );
        let errors = check_source(
            "async fn guard() { let m = Mutex::new(0); lock m as value { println(1); } }",
        );
        assert!(
            errors.iter().all(|d| d.severity != Severity::Error),
            "{errors:?}"
        );
    }

    #[test]
    fn effect_contract_obligation_and_arity_work_bounds() {
        for n in [8, 32, 128] {
            for shape in ["chain", "fanout"] {
                let mut source =
                    String::from("open class C0 { @effects(no_wait) pub open fn f(self) {} }\n");
                for i in 1..n {
                    let parent = if shape == "chain" { i - 1 } else { 0 };
                    source.push_str(&format!("open class C{i} extends C{parent} {{ @effects(no_wait) pub open override fn f(self) {{}} }}\n"));
                }
                OBLIGATION_WORK.with(|v| v.set(0));
                let errors = check_source(&source);
                assert!(
                    errors.iter().all(|d| d.severity != Severity::Error),
                    "{errors:?}"
                );
                let work = OBLIGATION_WORK.with(|v| v.get());
                assert_eq!(work, 2 * n - 1, "{shape} {n}");
                println!("contract-obligations shape={shape} classes={n} visits={work}");
            }
            let mut source = String::from("fn wait(c: BlockingCell<i64>) { c.get(); }\n");
            for i in 0..n {
                source.push_str(&format!(
                    "fn relay{i}(c: BlockingCell<i64>) {{ let f = wait; f(c); }}\n"
                ));
            }
            CONTRACT_GRAPH_WORK.with(|v| v.set(0));
            let errors = check_source(&source);
            assert!(
                errors.iter().all(|d| d.severity != Severity::Error),
                "{errors:?}"
            );
            let work = CONTRACT_GRAPH_WORK.with(|v| v.get());
            assert!(work <= 5 * n + 10, "{n}: {work}");
            println!("contract-arity callers={n} values={n} visits={work}");
        }
    }

    #[test]
    fn effect_contract_polymorphic_perspectives() {
        let cases = [
            (
                "trusted blocking ABI",
                "@effects(no_wait) fn guard() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "printing does not suspend",
                "@effects(no_suspend) fn guard() { print(1); }",
                None,
            ),
            (
                "blocking ABI through helper",
                "fn helper() { print(1); } @effects(no_block) fn guard() { helper(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unannotated interface implementation waits",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>); } class C implements I { pub fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "unannotated interface implementation pure",
                "interface I { @effects(no_wait) fn f(self); } class C implements I { pub fn f(self) {} }",
                None,
            ),
            (
                "override weakens",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_suspend) pub override fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "override strengthens",
                "open class B { @effects(no_suspend) pub open fn f(self) {} } class C extends B { @effects(no_wait) pub override fn f(self) {} }",
                None,
            ),
            (
                "override preserves",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_wait) pub override fn f(self) {} }",
                None,
            ),
            (
                "omitted annotation inherits",
                "open class B { @effects(no_wait) pub open fn f(self, c: BlockingCell<i64>) {} } class C extends B { pub override fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "omitted annotation pure",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { pub override fn f(self) {} }",
                None,
            ),
            (
                "combined prohibitions preserve",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_suspend, no_block) pub override fn f(self) {} }",
                None,
            ),
            (
                "interface weakening",
                "interface I { @effects(no_wait) fn f(self); } class C implements I { @effects(no_block) pub fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "deep inherited implementation",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>); } open class B { pub fn f(self, c: BlockingCell<i64>) { c.get(); } } class C extends B implements I {}",
                Some(ErrorCode::E2610),
            ),
            (
                "virtual mixture",
                "open class B { pub open fn f(self, c: BlockingCell<i64>) {} } class C extends B { pub override fn f(self, c: BlockingCell<i64>) { c.get(); } } @effects(no_wait) fn guard(b: B, c: BlockingCell<i64>) { b.f(c); }",
                Some(ErrorCode::E2610),
            ),
            (
                "virtual all pure",
                "open class B { pub open fn f(self) {} } class C extends B { pub override fn f(self) {} } @effects(no_wait) fn guard(b: B) { b.f(); }",
                None,
            ),
            (
                "interface mixture",
                "interface I { fn f(self, c: BlockingCell<i64>); } class A implements I { pub fn f(self, c: BlockingCell<i64>) {} } class B implements I { pub fn f(self, c: BlockingCell<i64>) { c.get(); } } @effects(no_wait) fn guard(i: I, c: BlockingCell<i64>) { i.f(c); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unknown interface target",
                "interface I { fn f(self); } @effects(no_wait) fn guard(i: I) { i.f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown function parameter",
                "@effects(no_wait) fn guard(f: fn()) { f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown closure parameter",
                "@effects(no_wait) fn guard(f: closure()) { f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown helper callback",
                "fn relay(f: fn()) { f(); } @effects(no_wait) fn guard(f: fn()) { relay(f); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting named function value",
                "fn wait(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = wait; f(c); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting value through argument",
                "fn wait(c: BlockingCell<i64>) { c.get(); } fn relay(f: fn(BlockingCell<i64>), c: BlockingCell<i64>) { f(c); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { relay(wait, c); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting captured closure",
                "@effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = || { c.get(); }; f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "closure captures waiting value",
                "fn wait(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = wait; let g = || { f(c); }; g(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unverified ffi",
                "extern rust { fn foreign(); } @effects(no_wait) fn guard() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "ffi cannot prove no suspend",
                "extern rust { fn foreign(); } @effects(no_suspend) fn guard() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "ffi transitive",
                "extern rust { fn foreign(); } fn relay() { foreign(); } @effects(no_block) fn guard() { relay(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "trusted arithmetic",
                "@effects(no_wait) fn guard(n: i64) -> i64 { return n + 1; }",
                None,
            ),
            (
                "async creation",
                "async fn wait() { await sleep(1); } @effects(no_wait) fn guard() { wait(); }",
                None,
            ),
            (
                "unannotated ffi unaffected",
                "extern rust { fn foreign(); } fn guard() { foreign(); }",
                None,
            ),
        ];
        for (label, source, expected) in cases {
            let errors = check_source(source);
            let codes: Vec<_> = errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(|d| d.code)
                .collect();
            if let Some(code) = expected {
                assert!(codes.contains(&code), "{label}: {errors:?}");
                assert!(codes.iter().all(|c| [ErrorCode::E2610, ErrorCode::E2611, ErrorCode::E2612].contains(c)), "{label}: {errors:?}");
                if code == ErrorCode::E2612 {
                    assert!(
                        !codes.contains(&ErrorCode::E2610),
                        "{label}: uncertainty was reported as proof: {errors:?}"
                    );
                }
            } else {
                assert!(codes.is_empty(), "{label}: {errors:?}");
            }
        }
    }

    #[test]
    fn effect_contract_generic_default_check_count_is_linear() {
        use super::super::check_decls::DEFAULT_BODY_CHECKS;
        for count in [8, 32, 128] {
            let source: String = (0..count).map(|i| format!(
                "interface I{i}<T> {{ fn leaf(self, c: BlockingCell<i64>) {{ c.get(); }} fn middle(self, c: BlockingCell<i64>) {{ self.leaf(c); }} @effects(no_wait) fn guarded(self, c: BlockingCell<i64>) {{ self.middle(c); }} }}\n"
            )).collect();
            DEFAULT_BODY_CHECKS.with(|checks| checks.set(0));
            let errors = check_source(&source);
            assert_eq!(
                errors.iter().filter(|e| e.code == ErrorCode::E2610).count(),
                count,
                "{errors:?}"
            );
            assert!(
                errors
                    .iter()
                    .all(|e| e.severity != Severity::Error || e.code == ErrorCode::E2610),
                "{errors:?}"
            );
            let checks = DEFAULT_BODY_CHECKS.with(|checks| checks.get());
            assert_eq!(checks, 3 * count);
            println!(
                "generic-default-checks interfaces={count} bodies={} checks={checks}",
                3 * count
            );
        }
    }

    #[test]
    fn effect_contract_unused_generic_interface_defaults() {
        let cases = [
            (
                "interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_block) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_suspend) fn f(self, c: Channel<i64>) { c.recv(); } }",
                Some("Channel.recv"),
            ),
            (
                "fn leaf(c: BlockingCell<i64>) { c.get(); } fn middle(c: BlockingCell<i64>) { leaf(c); } interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { middle(c); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { fn helper(self, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f(self, c: BlockingCell<i64>) { self.helper(c); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_wait) fn f(self, value: T) -> T { return value; } }",
                None,
            ),
            (
                "interface I<T> { @effects(no_wait) fn f(self, value: T) -> T; }",
                None,
            ),
            (
                "interface I<T> { fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                None,
            ),
        ];
        for (source, root) in cases {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2610),
                "{source}: {errors:?}"
            );
            let contract_errors: Vec<_> = errors
                .iter()
                .filter(|d| d.code == ErrorCode::E2610)
                .collect();
            assert_eq!(
                contract_errors.len(),
                usize::from(root.is_some()),
                "{source}: {errors:?}"
            );
            if let Some(root) = root {
                assert!(
                    contract_errors[0]
                        .labels
                        .iter()
                        .any(|l| l.message == format!("root operation `{root}`")),
                    "{errors:?}"
                );
            }
        }
    }

    #[test]
    fn effect_contract_mixed_direct_and_transitive_wait_bits() {
        for reverse in [false, true] {
            for (contract, root) in [("no_suspend", "await"), ("no_block", "BlockingCell.get")] {
                let body = if reverse {
                    "await sleep(1); c.get();"
                } else {
                    "c.get(); await sleep(1);"
                };
                let source =
                    format!("@effects({contract}) async fn f(c: BlockingCell<i64>) {{ {body} }}");
                let errors = check_source(&source);
                let error = errors
                    .iter()
                    .find(|d| d.code == ErrorCode::E2610)
                    .expect("mixed async body must violate either contract");
                assert!(
                    error
                        .labels
                        .iter()
                        .any(|l| l.message == format!("root operation `{root}`")),
                    "{error:?}"
                );
                let body = if reverse {
                    "ch.recv(); c.get();"
                } else {
                    "c.get(); ch.recv();"
                };
                let root = if contract == "no_suspend" {
                    "Channel.recv"
                } else {
                    root
                };
                for transitive in [false, true] {
                    let source = if transitive {
                        format!(
                            "fn leaf(c: BlockingCell<i64>, ch: Channel<i64>) {{ {body} }} fn middle(c: BlockingCell<i64>, ch: Channel<i64>) {{ leaf(c, ch); }} @effects({contract}) fn f(c: BlockingCell<i64>, ch: Channel<i64>) {{ middle(c, ch); }}"
                        )
                    } else {
                        format!(
                            "@effects({contract}) fn f(c: BlockingCell<i64>, ch: Channel<i64>) {{ {body} }}"
                        )
                    };
                    let errors = check_source(&source);
                    let error = errors
                        .iter()
                        .find(|d| d.code == ErrorCode::E2610)
                        .expect("mixed sync body must violate either contract");
                    assert!(
                        error
                            .labels
                            .iter()
                            .any(|l| l.message == format!("root operation `{root}`")),
                        "{error:?}"
                    );
                    if transitive {
                        assert!(
                            error
                                .notes
                                .iter()
                                .any(|n| n.contains("f -> middle -> leaf")),
                            "{error:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn effect_contract_perspectives() {
        let cases = [
            (
                "pure method",
                "class C { @effects(no_wait) pub fn f(self) -> i64 { return 1; } }",
                false,
            ),
            (
                "async method body",
                "class C { @effects(no_wait) pub async fn f(self) { await sleep(1); } }",
                true,
            ),
            (
                "async method creation",
                "class C { pub async fn f(self) { await sleep(1); } } @effects(no_wait) fn task(c: C) { c.f(); }",
                false,
            ),
            (
                "pure interface default",
                "interface I { @effects(no_wait) fn f(self) -> i64 { return 1; } }",
                false,
            ),
            (
                "generic helper wait",
                "fn a<T>(x: T, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { a(x, c); }",
                true,
            ),
            (
                "pure",
                "@effects(no_wait) fn f() -> i64 { return 1; }",
                false,
            ),
            (
                "allocation",
                "@effects(no_wait) fn f() { let a = [1, 2]; }",
                false,
            ),
            (
                "await",
                "@effects(no_wait) async fn f() { await sleep(1); }",
                true,
            ),
            (
                "suspend",
                "@effects(no_suspend) async fn f() { await sleep(1); }",
                true,
            ),
            (
                "block allows await",
                "@effects(no_block) async fn f() { await sleep(1); }",
                false,
            ),
            (
                "channel",
                "@effects(no_wait) fn f(c: Channel<i64>) { c.recv(); }",
                true,
            ),
            (
                "channel suspend",
                "@effects(no_suspend) fn f(c: Channel<i64>) { c.recv(); }",
                true,
            ),
            (
                "cell",
                "@effects(no_wait) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "block cell",
                "@effects(no_block) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "suspend allows cell",
                "@effects(no_suspend) fn f(c: BlockingCell<i64>) { c.get(); }",
                false,
            ),
            (
                "task creation",
                "async fn child() { await sleep(1); } @effects(no_wait) fn f() { child(); }",
                false,
            ),
            (
                "async body",
                "@effects(no_wait) async fn child() { await sleep(1); } fn f() { child(); }",
                true,
            ),
            (
                "two helpers",
                "fn a(c: BlockingCell<i64>) { c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }",
                true,
            ),
            (
                "recursive wait",
                "fn a(c: BlockingCell<i64>) { b(c); c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }",
                true,
            ),
            (
                "recursive pure",
                "fn a() { b(); } fn b() { a(); } @effects(no_wait) fn f() { a(); }",
                false,
            ),
            (
                "class method",
                "class C { @effects(no_wait) pub fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "static method",
                "class C { @effects(no_wait) pub static fn f(c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "interface signature",
                "interface I { @effects(no_wait) fn f(self); }",
                false,
            ),
            (
                "interface default",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "multiple contracts",
                "@effects(no_block, no_suspend) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "repeated attributes",
                "@effects(no_suspend) @effects(no_block) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "unannotated",
                "fn f(c: BlockingCell<i64>) { c.get(); }",
                false,
            ),
            (
                "generic pure",
                "@effects(no_wait) fn f<T>(x: T) -> T { return x; }",
                false,
            ),
            (
                "generic wait",
                "@effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
        ];
        for (label, source, violation) in cases {
            let diagnostics = check_source(source);
            assert_eq!(
                diagnostics.iter().any(|d| d.code == ErrorCode::E2610),
                violation,
                "{label}: {diagnostics:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2610),
                "{label}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn effect_contract_chain_is_deterministic_and_located() {
        let source = "fn a(c: BlockingCell<i64>) { c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }";
        let first = check_source(source);
        let diagnostic = first.iter().find(|d| d.code == ErrorCode::E2610).unwrap();
        assert!(
            diagnostic
                .notes
                .iter()
                .any(|n| n.contains("compiler-proven")
                    && n.contains("f -> b -> a -> BlockingCell.get")),
            "{diagnostic:?}"
        );
        assert_eq!(diagnostic.labels.len(), 4);
        for _ in 0..8 {
            assert_eq!(format!("{first:?}"), format!("{:?}", check_source(source)));
        }
    }

    #[test]
    fn effect_contract_declaration_storage_roundtrips() {
        let source = "@effects(no_wait) pub fn f<T>(x: T) -> T { return x; } class C { @effects(no_block) pub fn m(self) {} } interface I { @effects(no_suspend) fn m(self); }";
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let decoded: Program =
            serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
        let contracts: Vec<_> = decoded
            .items
            .iter()
            .map(|item| match item {
                Item::Function(f) => &f.effect_contracts,
                Item::Class(c) => &c.methods[0].effect_contracts,
                Item::Interface(i) => &i.methods[0].effect_contracts,
                _ => unreachable!(),
            })
            .collect();
        for (contracts, expected) in contracts.iter().zip([
            EffectContractKind::NoWait,
            EffectContractKind::NoBlock,
            EffectContractKind::NoSuspend,
        ]) {
            assert_eq!(contracts.len(), 1);
            assert_eq!(contracts[0].kind, expected);
            assert!(
                source[contracts[0].span.start..contracts[0].span.end].contains(expected.name())
            );
        }
    }

    #[test]
    fn effect_contract_invalid_syntax_is_rejected() {
        for source in [
            "@effects(unknown) fn f() {}",
            "@other(no_wait) fn f() {}",
            "@effects() fn f() {}",
            "@effects(no_wait) class C {}",
            "class C { @effects(no_wait) x: i64; }",
            "fn f(x: @effects(no_wait) fn()) {}",
        ] {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (_, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(!errors.is_empty(), "{source}");
        }
    }
    #[test]
    fn effect_contract_path_work_scales_with_nodes_and_edges() {
        use crate::compiler_db::effects::{LockEffectCause, LockEffectKind, LockEffectWitness};
        use crate::semantic::{call_graph::CallSites, effects::EffectProblem};
        for shape in ["chain", "fanout", "cycle", "many-roots"] {
            for n in [32, 128, 512, 2048] {
                let ids: Vec<_> = (0..n)
                    .map(|i| FunctionId::free(format!("f{i:05}")))
                    .collect();
                let mut graph = CallGraph::default();
                let mut problem = EffectProblem::new();
                for i in 0..n {
                    problem = problem.body(ids[i]);
                    let target = match shape {
                        "fanout" => (i > 0).then_some(ids[0]),
                        "cycle" => Some(ids[(i + 1) % n]),
                        _ => (i > 0).then(|| ids[i - 1]),
                    };
                    graph.merge(
                        ids[i],
                        CallSites {
                            targets: target.into_iter().collect(),
                            ..Default::default()
                        },
                    );
                    if i == 0 || shape == "many-roots" {
                        problem = problem.seed(
                            ids[i],
                            RuntimeEffects::MAY_BLOCK,
                            Some(EffectWitness::Lock(LockEffectWitness {
                                owner: ids[i],
                                cause: LockEffectCause {
                                    is_conservative: false,
                                    span: Span::dummy(),
                                    operation: "BlockingCell.get".into(),
                                    kind: LockEffectKind::Block,
                                    lock: None,
                                },
                            })),
                        );
                    }
                }
                let facts = problem.solve(&graph);
                PATH_WORK.with(|count| count.set(0));
                let paths =
                    witness_paths(&graph, &facts, &HashMap::new(), RuntimeEffects::MAY_BLOCK);
                let work = PATH_WORK.with(|count| count.get());
                let edges = graph.iter().map(|(_, s)| s.targets.len()).sum::<usize>();
                assert!(work <= 2 * n + 2 * edges, "{shape}: {n} {edges} {work}");
                assert_eq!(paths.len(), n - 1);
                println!(
                    "effect-path-work shape={shape} nodes={n} edges={edges} visits={work} next_hops={}",
                    paths.len()
                );
            }
        }
    }
}

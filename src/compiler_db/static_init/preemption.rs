//! Closed-build preemption proof. Unknown dispatch and callback obligations
//! survive module checking until all implementing classes and function values
//! are available. This graph is separate from panic/lock effect equations.
use super::*;
use crate::compiler_db::effects::UnitEffects;
use crate::semantic::call_graph::UnsupportedInitialization;
use crate::semantic::indirect_lock::IndirectInputs;

pub(super) struct Inputs {
    pub(super) effects: std::sync::Arc<UnitEffects>,
    leaves: HashSet<FunctionId>,
    tasks: Vec<(FunctionId, Span)>,
    foreign: HashSet<FunctionId>,
    indirect: IndirectInputs,
    std_aliases: HashMap<String, String>,
}

impl StaticInitQueries {
    pub(crate) fn record_preemption(
        &self,
        unit: UnitId,
        program: &Program,
        effects: &std::sync::Arc<UnitEffects>,
        indirect: &IndirectInputs,
    ) {
        let mut leaves = HashSet::new();
        let mut tasks = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) if f.is_async => {
                    tasks.push((FunctionId::free_from_source_name(&f.name), f.span));
                }
                Item::Class(c) => {
                    let owner = TypeId::local(&c.name);
                    if c.constructors.is_empty() {
                        leaves.insert(FunctionId::method(owner, "init"));
                    }
                    for m in &c.methods {
                        if m.is_async {
                            tasks.push((FunctionId::method(owner, &m.name), m.span));
                        }
                    }
                }
                Item::Enum(e) => {
                    for v in &e.variants {
                        leaves.insert(FunctionId::method(TypeId::local(&e.name), &v.name));
                    }
                }
                _ => {}
            }
        }
        let std_aliases = program
            .imports
            .iter()
            .filter_map(|i| {
                i.path.strip_prefix("std::").map(|path| {
                    (
                        i.alias
                            .clone()
                            .unwrap_or_else(|| path.rsplit("::").next().unwrap().into()),
                        path.into(),
                    )
                })
            })
            .collect();
        self.units
            .borrow_mut()
            .get_mut(&unit)
            .expect("recorded call graph")
            .preemption = Some(Inputs {
            effects: std::sync::Arc::clone(effects),
            leaves,
            tasks,
            foreign: program
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Function(f) if f.rust_bridge.is_some() => {
                        Some(FunctionId::free_from_source_name(&f.name))
                    }
                    _ => None,
                })
                .collect(),
            indirect: indirect.clone(),
            std_aliases,
        });
    }

    /// Resolve deferred module obligations using shared dispatch unions and
    /// 64 shared arity nodes. No caller copies its transitive target set.
    pub(crate) fn check_preemption(
        &self,
        order: &[UnitId],
        effects: &EffectQueries,
        native: bool,
    ) -> Vec<Diagnostic> {
        let units = self.units.borrow();
        if units
            .values()
            .all(|u| u.preemption.as_ref().is_none_or(|p| p.tasks.is_empty()))
        {
            return Vec::new();
        }
        let mut keys = Vec::new();
        let mut positions = HashMap::new();
        for &unit in order {
            if let Some(facts) = units.get(&unit) {
                for &id in facts.graph.ids() {
                    positions.insert((unit, id), keys.len());
                    keys.push((unit, id));
                }
            }
        }
        let mut edges = vec![Vec::new(); keys.len()];
        let mut unknown = vec![false; keys.len()];
        let mut missing = HashMap::new();
        let mut loops = vec![false; keys.len()];
        let resolve = |unit, target: &FunctionId| {
            positions.get(&(unit, *target)).copied().or_else(|| {
                effects
                    .external_target(unit, target, &units[&unit].imports)
                    .and_then(|key| positions.get(&key).copied())
            })
        };
        let leaf = |unit, target: &FunctionId| {
            let inputs = units[&unit].preemption.as_ref().expect("preemption inputs");
            if inputs.leaves.contains(target)
                || crate::semantic::intrinsics::builtin_target_effects(target).is_some()
                || crate::semantic::intrinsics::builtin_static_effects(target, &inputs.std_aliases)
                    .is_some()
            {
                return true;
            }
            effects
                .external_target(unit, target, &units[&unit].imports)
                .and_then(|(unit, id)| {
                    units
                        .get(&unit)?
                        .preemption
                        .as_ref()
                        .map(|p| p.leaves.contains(&id))
                })
                .unwrap_or(false)
        };
        for &unit in order {
            let Some(facts) = units.get(&unit) else {
                continue;
            };
            let Some(inputs) = &facts.preemption else {
                continue;
            };
            let async_callables: HashSet<_> = inputs.tasks.iter().map(|(id, _)| *id).collect();
            let copies: HashSet<_> = inputs
                .effects
                .preemption_copies
                .iter()
                .map(|(id, _, _)| *id)
                .collect();
            for (&id, calls) in facts.graph.iter() {
                let node = positions[&(unit, id)];
                loops[node] =
                    inputs.effects.preemption_loops.contains(&id) && !async_callables.contains(&id);
                unknown[node] = inputs.foreign.contains(&id)
                    || calls
                        .unsupported_initialization
                        .contains(&UnsupportedInitialization::UnresolvedCall)
                    || (calls.has_unknown && calls.unsupported_initialization.is_empty())
                    || (calls
                        .unsupported_initialization
                        .contains(&UnsupportedInitialization::InjectedDefault)
                        && !copies.contains(&id)
                        && calls.targets.is_empty());
                for target in &calls.targets {
                    // The build-wide union below owns virtual dispatch. A
                    // module-local abstract slot is not itself an executed body.
                    // BodyIndex identifies the actual instantiated default;
                    // unit-local effect unions can still name its template.
                    if copies.contains(&id) && target.name().starts_with("$default$") {
                        continue;
                    }
                    if calls.virtual_calls.contains(target) {
                        continue;
                    }
                    if let Some(target) = resolve(unit, target) {
                        edges[node].push(target);
                    } else if !leaf(unit, target) {
                        unknown[node] = true;
                        missing.entry(node).or_insert_with(|| target.to_string());
                    }
                }
                if !inputs.effects.preemption_bodies.contains(&id)
                    && !id.is_lambda()
                    && calls.targets.is_empty()
                    && calls.virtual_calls.is_empty()
                    && !leaf(unit, &id)
                {
                    unknown[node] = true;
                }
            }
            for &(caller, declaring, target) in &inputs.effects.preemption_copies {
                if let Some(&caller) = positions.get(&(unit, caller)) {
                    if let Some(&target) = positions.get(&(declaring, target)) {
                        edges[caller].push(target);
                    } else {
                        unknown[caller] = true;
                    }
                }
            }
        }
        let bodies = keys.len();
        let dispatch = super::connect_build_dispatch(
            order, &units, effects, &positions, &mut keys, &mut edges,
        );
        unknown.resize(edges.len(), false);
        loops.resize(edges.len(), false);
        let mut concrete = vec![false; edges.len()];
        concrete[..bodies].fill(true);
        let concrete = propagate(&edges, concrete).0;
        for &node in dispatch.targets.values() {
            unknown[node] |= !concrete[node];
        }
        for &node in &dispatch.unresolved {
            unknown[node] = true;
        }
        let arities = edges.len();
        edges.resize_with(arities + 64, Vec::new);
        unknown.resize(edges.len(), false);
        loops.resize(edges.len(), false);
        let mut has_value = 0u64;
        for &unit in order {
            let Some(inputs) = units.get(&unit).and_then(|u| u.preemption.as_ref()) else {
                continue;
            };
            for value in &inputs.indirect.values {
                has_value |= value.bit;
                let node = arities + value.bit.trailing_zeros() as usize;
                match value.target {
                    Some(target) => {
                        if let Some(target_node) = resolve(unit, &target) {
                            edges[node].push(target_node);
                        } else if !leaf(unit, &target) {
                            unknown[node] = true;
                        }
                    }
                    None => unknown[node] = true,
                }
            }
            for call in &inputs.indirect.calls {
                let mask = call.bit;
                if let Some(&caller) = positions.get(&(unit, call.caller)) {
                    for bit in 0..64 {
                        if mask & (1 << bit) != 0 {
                            edges[caller].push(arities + bit);
                        }
                    }
                }
            }
        }
        for bit in 0..64 {
            unknown[arities + bit] |= has_value & (1 << bit) == 0;
        }
        // Creating a Task never executes its body synchronously. Check task
        // bodies as roots, but remove incoming edges to those roots.
        let mut tasks = Vec::new();
        for &unit in order {
            if let Some(inputs) = units.get(&unit).and_then(|u| u.preemption.as_ref()) {
                for &(id, span) in &inputs.tasks {
                    if let Some(&node) = positions.get(&(unit, id)) {
                        tasks.push((node, span));
                    }
                }
            }
        }
        let async_nodes: HashSet<_> = tasks.iter().map(|(node, _)| *node).collect();
        for targets in &mut edges {
            targets.retain(|target| !async_nodes.contains(target));
        }
        if !native {
            let components = crate::semantic::effects::strongly_connected_components(&edges);
            let mut sizes = vec![0; edges.len()];
            for &component in &components {
                sizes[component] += 1;
            }
            for node in 0..edges.len() {
                loops[node] |= sizes[components[node]] > 1 || edges[node].contains(&node);
            }
        }
        let (unknown, origins, _) = propagate(&edges, unknown);
        let blocked = if native {
            vec![false; edges.len()]
        } else {
            propagate(&edges, loops).0
        };
        let failed_arities = (0..64).fold(0u64, |mask, bit| {
            if unknown[arities + bit] || blocked[arities + bit] {
                mask | (1 << bit)
            } else {
                mask
            }
        });
        tasks
            .into_iter()
            .filter_map(|(node, span)| {
                let reason = if unknown[node] {
                    "unresolved call target or unavailable preemption summary"
                } else if blocked[node] {
                    "synchronous loop or recursion requires native sync-stack preemption"
                } else {
                    return None;
                };
                let witness = if unknown[node] {
                    missing
                        .get(&origins[node])
                        .cloned()
                        .or_else(|| keys.get(origins[node]).map(|(_, id)| id.to_string()))
                        .unwrap_or_else(|| {
                            format!("function value with arity {}", origins[node] - arities)
                        })
                } else {
                    keys[node].1.to_string()
                };
                // Point at a source call in the failing task, not its entire
                // declaration. Inspect each root's direct sites only once.
                let (unit, id) = keys[node];
                let sites = units[&unit].graph.get(&id).expect("task graph node");
                let call_span = sites
                    .call_spans
                    .iter()
                    .filter_map(|(target, span)| {
                        let failed = resolve(unit, target).map_or_else(
                            || !leaf(unit, target),
                            |target| {
                                !async_nodes.contains(&target)
                                    && (unknown[target] || blocked[target])
                            },
                        );
                        failed.then_some(*span)
                    })
                    .chain(sites.virtual_spans.iter().filter_map(|(target, span)| {
                        let failed = dispatch
                            .targets
                            .get(&(unit, *target))
                            .is_none_or(|&node| unknown[node] || blocked[node]);
                        failed.then_some(*span)
                    }))
                    .chain(
                        sites
                            .indirect_spans
                            .iter()
                            .filter_map(|(bit, span)| (failed_arities & bit != 0).then_some(*span)),
                    )
                    .chain(sites.unknown_span)
                    .min_by_key(|s| (s.file_id, s.start));
                let span = call_span.unwrap_or(span);
                Some(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0810,
                        format!(
                            "cannot prove preemptibility of task `{}`: {reason}",
                            keys[node].1
                        ),
                    )
                    .with_label(Label::primary(
                        span,
                        if unknown[node] {
                            "this task reaches an unproven synchronous call"
                        } else {
                            "this target cannot preempt synchronous loops or recursion"
                        },
                    ))
                    .with_help(if unknown[node] {
                        "use a callable with an analyzed body and a resolved dispatch target set"
                    } else {
                        crate::semantic::concurrency::NONPREEMPTIBLE_HELP
                    })
                    .with_note(format!("unproven call in `{witness}`")),
                )
            })
            .collect()
    }
}

/// Each node is queued once, each reverse edge is inspected at most once.
fn propagate(edges: &[Vec<usize>], mut marked: Vec<bool>) -> (Vec<bool>, Vec<usize>, usize) {
    let mut origins: Vec<_> = (0..edges.len()).collect();
    let mut callers = vec![Vec::new(); edges.len()];
    for (caller, targets) in edges.iter().enumerate() {
        for &target in targets {
            callers[target].push(caller);
        }
    }
    let mut pending: Vec<_> = marked
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| b.then_some(i))
        .collect();
    let mut visits = 0;
    while let Some(target) = pending.pop() {
        for &caller in &callers[target] {
            visits += 1;
            if !marked[caller] {
                marked[caller] = true;
                origins[caller] = origins[target];
                pending.push(caller);
            }
        }
    }
    (marked, origins, visits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preemption_propagation_visits_each_edge_once() {
        for n in [32, 128, 512, 2048] {
            for shape in ["chain", "fanout", "cycle", "repeated"] {
                let mut edges = vec![Vec::new(); n];
                for (i, targets) in edges.iter_mut().enumerate().take(n - 1) {
                    match shape {
                        "fanout" => targets.push(n - 1),
                        "repeated" => targets.extend([i + 1; 8]),
                        _ => targets.push(i + 1),
                    }
                }
                if shape == "cycle" {
                    edges[n - 1].push(0);
                }
                let mut marked = vec![false; n];
                marked[n - 1] = true;
                let (marked, origins, visits) = propagate(&edges, marked);
                let count: usize = edges.iter().map(Vec::len).sum();
                assert!(marked.iter().all(|b| *b));
                assert!(origins.iter().all(|i| *i == n - 1));
                assert_eq!(visits, count);
                println!("preemption shape={shape} nodes={n} edges={count} visits={visits}");
            }
        }
    }

    fn errors(source: &str, native: bool) -> Vec<Diagnostic> {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, diagnostics) = crate::parser::Parser::new(tokens).parse();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut checker = crate::semantic::TypeChecker::new().with_sync_stack_preemption(native);
        let queries = std::rc::Rc::new(EffectQueries::default());
        checker.set_effect_queries(std::rc::Rc::clone(&queries), UnitId::ENTRY);
        checker.check_program(&program);
        if checker.errors.is_empty() {
            checker
                .errors
                .extend(queries.static_initializers.check_preemption(
                    &[UnitId::ENTRY],
                    &queries,
                    native,
                ));
        }
        checker.errors
    }

    #[test]
    fn preemption_indirect_span_uses_only_failed_arity_obligations() {
        let source = "fn safe()->i64{return 1;}\nasync fn run(f:fn(i64)->i64) {\nlet g=safe; println(g());\nprintln(f(1));\n} fn main(){}";
        for native in [false, true] {
            let diagnostics = errors(source, native);
            let error = diagnostics
                .iter()
                .find(|e| e.code == ErrorCode::E0810)
                .unwrap();
            assert_eq!(error.labels[0].span.line, 4, "{diagnostics:?}");
        }
        let source = "fn safe()->i64{return 1;} fn looped(x:i64)->i64{let mut n=0;while n<x{n=n+1;}return n;}\nasync fn main(){\nlet g=safe; println(g());\nlet f=looped; println(f(1));\n}";
        assert!(errors(source, true).is_empty());
        let diagnostics = errors(source, false);
        let error = diagnostics
            .iter()
            .find(|e| e.code == ErrorCode::E0810)
            .unwrap();
        assert_eq!(error.labels[0].span.line, 4, "{diagnostics:?}");
    }

    #[test]
    fn preemption_unknown_foreign_and_interface_fail_closed_on_both_targets() {
        for native in [false, true] {
            for source in [
                "extern rust { fn foreign(x:i64)->i64; } async fn main(){ println(foreign(42)); }",
                "interface I { fn get(self)->i64; } async fn run(i:I)->i64{return i.get();} fn main(){}",
                "async fn run(f:fn(i64)->i64)->i64{return f(1);} fn main(){}",
            ] {
                let errors = errors(source, native);
                assert!(
                    errors.iter().any(|e| e.code == ErrorCode::E0810),
                    "native={native}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn a9_diagnostic_remedies_match_the_remaining_obligation() {
        // Indirect dispatch reaches the build-wide proof on both capabilities.
        let source = "fn looped(x:i64)->i64 {let mut n=0;while n<x{n=n+1;}return n;} fn apply(f:fn(i64)->i64)->i64{return f(42);} async fn main(){println(apply(looped));}";
        assert!(errors(source, true).is_empty());
        let diagnostics = errors(source, false);
        let diagnostic = diagnostics
            .iter()
            .find(|e| e.code == ErrorCode::E0810)
            .unwrap();
        assert!(
            diagnostic
                .message
                .contains("requires native sync-stack preemption")
        );
        assert!(
            diagnostic.labels[0]
                .message
                .contains("this target cannot preempt")
        );
        assert_eq!(
            diagnostic.helps,
            [crate::semantic::concurrency::NONPREEMPTIBLE_HELP]
        );
        for native in [false, true] {
            let diagnostics = errors(
                "async fn run(f:fn(i64)->i64)->i64{return f(1);} fn main(){}",
                native,
            );
            let diagnostic = diagnostics
                .iter()
                .find(|e| e.code == ErrorCode::E0810)
                .unwrap();
            assert!(diagnostic.message.contains("unresolved call target"));
            assert_eq!(
                diagnostic.helps,
                ["use a callable with an analyzed body and a resolved dispatch target set"]
            );
            assert!(diagnostic.notes.iter().all(|n| !n.contains("temporary")));
        }
    }

    #[test]
    fn preemption_indirect_loop_uses_target_capability() {
        let source = "fn looped(x:i64)->i64 { let mut n=0; while n<x {n=n+1;} return n; } fn apply(f:fn(i64)->i64)->i64{return f(42);} async fn main(){println(apply(looped));}";
        assert!(errors(source, true).is_empty());
        assert!(
            errors(source, false)
                .iter()
                .any(|e| e.code == ErrorCode::E0810)
        );
    }

    #[test]
    fn preemption_sync_defer_loop_requires_native_stack() {
        let source =
            "fn work() { defer { let mut n=0; while n<42 {n=n+1;} } } async fn main(){work();}";
        assert!(errors(source, true).is_empty());
        assert!(
            errors(source, false)
                .iter()
                .any(|e| e.code == ErrorCode::E0810)
        );
    }

    #[test]
    fn preemption_startup_work_is_not_task_work() {
        let source = "fn seed()->i64 {let mut n=0;while n<42 {n=n+1;}return n;} class C {pub static x:i64=seed();} async fn main(){println(C::x);}";
        let errors = errors(source, false);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn preemption_matching_foreign_value_remains_unknown() {
        let source = "extern rust { fn foreign(x:i64)->i64; } fn apply(f:fn(i64)->i64)->i64{return f(41);} async fn main(){println(apply(foreign));}";
        assert!(
            errors(source, true)
                .iter()
                .any(|e| e.code == ErrorCode::E0810)
        );
    }

    #[test]
    fn preemption_arity_partitions_unrelated_foreign_values() {
        let source = "extern rust { fn foreign(x:i64,y:i64)->i64; } fn keep()->fn(i64,i64)->i64{return foreign;} fn add(x:i64)->i64{return x+1;} fn apply(f:fn(i64)->i64)->i64{return f(41);} async fn main(){println(apply(add));}";
        let errors = errors(source, true);
        assert!(errors.is_empty(), "{errors:?}");
    }
}

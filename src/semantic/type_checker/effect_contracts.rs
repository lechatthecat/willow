//! Declaration contracts consume the same solved effects as lock checking.
use super::*;
use crate::compiler_db::effects::EffectWitness;
use crate::semantic::call_graph::CallGraph;
use crate::semantic::effects::{EffectFacts, RuntimeEffects};
use std::collections::VecDeque;

#[cfg(test)]
thread_local! {
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
            if asynchronous.get(callee) != Some(&true)
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
    pub(super) fn report_effect_contracts(
        &mut self,
        program: &Program,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
        templates: bool,
    ) {
        let mut declarations = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f)
                    if f.type_params.is_empty() != templates && !f.effect_contracts.is_empty() =>
                {
                    declarations.push((FunctionId::free(&f.name), &f.effect_contracts))
                }
                Item::Class(c) if c.type_params.is_empty() != templates => {
                    for m in &c.methods {
                        if !m.is_default_injected && !m.effect_contracts.is_empty() {
                            declarations.push((
                                FunctionId::method(TypeId::local(&c.name), &m.name),
                                &m.effect_contracts,
                            ));
                        }
                    }
                }
                Item::Interface(i) if i.type_params.is_empty() != templates => {
                    for m in &i.methods {
                        // Signature contracts are retained for conformance checking;
                        // Check executable defaults independently of conformance obligations.
                        if m.default_body.is_some() && !m.effect_contracts.is_empty() {
                            declarations.push((
                                interface_default_effect_id(&i.name, &m.name),
                                &m.effect_contracts,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        if declarations.is_empty() {
            return;
        }
        let summaries: HashMap<_, _> = facts.iter().map(|(&id, summary)| (id, summary)).collect();
        let bits = [RuntimeEffects::MAY_BLOCK, RuntimeEffects::MAY_SUSPEND];
        let paths = bits.map(|bit| witness_paths(graph, facts, &self.effect_index.callables, bit));
        for (id, contracts) in declarations {
            for contract in contracts {
                let Some(summary) = summaries.get(&id) else {
                    continue;
                };
                let Some(index) = bits.iter().position(|bit| {
                    contract.kind.forbidden().intersects(*bit) && summary.intersects(*bit)
                }) else {
                    continue;
                };
                let bit = bits[index];
                let cause = summary.witness(bit).and_then(EffectWitness::lock);
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2610,
                    format!(
                        "effect contract `{}` violated by `{id}`",
                        contract.kind.name()
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
                            format!("root operation `{}`", cause.operation),
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
                            "waiting effect has no available source witness",
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
}

#[cfg(test)]
mod tests {
    use super::super::check::check_source;
    use super::*;

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

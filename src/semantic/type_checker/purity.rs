//! Compiler-only purity facts. Runtime waiting effects are deliberately independent.
use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::call_graph::CallGraph;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PuritySummary(u8);
impl PuritySummary {
    const READ_EXTERNAL: Self = Self(1);
    const WRITE_EXTERNAL: Self = Self(2);
    const NONDETERMINISTIC: Self = Self(4);
    const UNKNOWN: Self = Self(8);
    const BITS: [Self; 4] = [
        Self::READ_EXTERNAL,
        Self::WRITE_EXTERNAL,
        Self::NONDETERMINISTIC,
        Self::UNKNOWN,
    ];
    fn contains(self, bit: Self) -> bool {
        self.0 & bit.0 != 0
    }
    fn insert(&mut self, bits: Self) {
        self.0 |= bits.0;
    }
    fn name(self) -> &'static str {
        match self.0 {
            1 => "READ_EXTERNAL",
            2 => "WRITE_EXTERNAL",
            4 => "NONDETERMINISTIC",
            _ => "UNKNOWN",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purity_acceptance_perspectives() {
        let cases = [
            (
                "integer",
                "@pure fn f(x:i64)->i64 { return x * 2 + 1; }",
                false,
            ),
            (
                "local mutation",
                "@pure fn f(x:i64)->i64 { let mut y=x; y=y+1; return y; }",
                false,
            ),
            ("boolean", "@pure fn f(x:bool)->bool { return !x; }", false),
            ("float", "@pure fn f(x:f64)->f64 { return x+1.0; }", false),
            (
                "string",
                "@pure fn f(x:String)->String { return x; }",
                false,
            ),
            (
                "branch",
                "@pure fn f(x:i64)->i64 { if x>0 { return x; } return 0; }",
                false,
            ),
            (
                "loop",
                "@pure fn f()->i64 { let mut x=0; while x<3 { x=x+1; } return x; }",
                false,
            ),
            (
                "forward pure",
                "@pure fn f()->i64 { return g(); } @pure fn g()->i64 { return 2; }",
                false,
            ),
            (
                "known ordinary",
                "@pure fn f()->i64 { return g(); } fn g()->i64 { return 2; }",
                false,
            ),
            (
                "recursive",
                "@pure fn f(x:i64)->i64 { if x<1 {return 0;} return f(x-1); }",
                false,
            ),
            (
                "mutual recursion",
                "@pure fn f(x:i64)->i64 { return g(x); } @pure fn g(x:i64)->i64 { return f(x); }",
                false,
            ),
            ("no declaration", "fn f() { println(1); }", false),
            (
                "combined",
                "@pure @effects(no_wait) fn f()->i64 { return 1; }",
                false,
            ),
            (
                "field read",
                "class C { pub x:i64; } @pure fn f(c:C)->i64 { return c.x; }",
                true,
            ),
            (
                "field write",
                "class C { pub x:i64; } @pure fn f(c:C) { c.x=2; }",
                true,
            ),
            (
                "static read",
                "class C { pub static mut x:i64=1; } @pure fn f()->i64 { return C::x; }",
                true,
            ),
            (
                "static write",
                "class C { pub static mut x:i64=1; } @pure fn f() { C::x=2; }",
                true,
            ),
            ("print", "@pure fn f() { println(1); }", true),
            (
                "transitive",
                "@pure fn f() { middle(); } fn middle() { leaf(); } fn leaf() { println(1); }",
                true,
            ),
            (
                "indirect",
                "@pure fn f(g:fn()->i64)->i64 { return g(); }",
                true,
            ),
            (
                "no wait write",
                "class C { pub x:i64; } @effects(no_wait) fn g(c:C) { c.x=2; } @pure fn f(c:C) { g(c); }",
                true,
            ),
            ("defer", "@pure fn f() { defer { println(1); } }", true),
            ("async", "@pure async fn f() { await sleep(1); }", true),
            ("channel", "@pure fn f(c:Channel<i64>) { c.recv(); }", true),
            (
                "dynamic",
                "open class C { @pure pub open fn g(self)->i64 { return 1; } } @pure fn f(c:C)->i64 { return c.g(); }",
                true,
            ),
            (
                "direct method",
                "class C { @pure pub fn g(self)->i64 { return 1; } } @pure fn f(c:C)->i64 { return c.g(); }",
                false,
            ),
            (
                "inherited direct method",
                "open class B { @pure pub fn g(self)->i64 { return 1; } } class C extends B {} @pure fn f(c:C)->i64 { return c.g(); }",
                false,
            ),
            (
                "array read",
                "import std::collections::Array; @pure fn f(a:Array<i64>)->i64 { return a[0]; }",
                true,
            ),
            (
                "lambda value",
                "@pure fn f() { let g=|| { println(1); }; }",
                false,
            ),
        ];
        for (name, source, rejected) in cases {
            let diagnostics = check_source(&format!("{source} fn main() {{}}"));
            let errors: Vec<_> = diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|d| d.code == ErrorCode::E2620),
                rejected,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2620),
                "{name}: unexpected errors {errors:?}"
            );
        }
    }

    #[test]
    fn purity_unsupported_allocations_are_unknown() {
        for body in [
            "let xs = [1, 2];",
            "let xs = [[1], [2]];",
            "let xs: Array<i64> = [];",
            "let c = new C(1);",
            "let c = C { x: 1 };",
        ] {
            let legacy_object = body.contains("C {");
            for transitive in [false, true] {
                let functions = if transitive {
                    format!("@pure fn f() {{ helper(); }} fn helper() {{ {body} }}")
                } else {
                    format!("@pure fn f() {{ {body} }}")
                };
                let source = format!(
                    "import std::collections::Array; class C {{ pub x:i64; }} {functions} fn main() {{}}"
                );
                let diagnostics = check_source(&source);
                let errors: Vec<_> = diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert_eq!(
                    errors.len(),
                    1 + usize::from(legacy_object),
                    "{source}: {errors:?}"
                );
                let purity = errors
                    .iter()
                    .find(|d| d.code == ErrorCode::E2620)
                    .expect("purity diagnostic");
                assert!(purity.message.contains("UNKNOWN"), "{source}: {errors:?}");
                if transitive {
                    assert!(
                        purity.notes.iter().any(|n| n.contains("f -> helper")),
                        "{errors:?}"
                    );
                }
                let plain = check_source(&source.replace("@pure ", ""));
                assert!(
                    plain.iter().all(|d| d.severity != Severity::Error
                        || (legacy_object && d.code == ErrorCode::E0847)),
                    "{plain:?}"
                );
            }
        }
    }

    #[test]
    fn purity_closure_allocation_uses_resolved_type() {
        for (body, rejected) in [
            ("let g = || x;", true),
            ("let g: closure() -> i64 = || 1;", true),
            ("let g: closure() -> i64 = || x;", true),
            ("let g = || 1;", false),
            ("let g: fn() -> i64 = || 1;", false),
            ("let g = || { println(1); };", false),
            ("let g = || { let h = || x; };", true),
            ("let g = || { let y = 1; let h = || y; };", false),
        ] {
            for transitive in [false, true] {
                let functions = if transitive {
                    format!("@pure fn f(x:i64) {{ helper(x); }} fn helper(x:i64) {{ {body} }}")
                } else {
                    format!("@pure fn f(x:i64) {{ {body} }}")
                };
                let source = format!("{functions} fn main() {{}}");
                let diagnostics = check_source(&source);
                let errors: Vec<_> = diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert_eq!(errors.len(), usize::from(rejected), "{source}: {errors:?}");
                if rejected {
                    assert_eq!(errors[0].code, ErrorCode::E2620, "{errors:?}");
                    assert!(errors[0].message.contains("UNKNOWN"), "{errors:?}");
                    if transitive {
                        assert!(
                            errors[0].notes.iter().any(|n| n.contains("f -> helper")),
                            "{errors:?}"
                        );
                    }
                }
                let plain = check_source(&source.replace("@pure ", ""));
                assert!(
                    plain.iter().all(|d| d.severity != Severity::Error),
                    "{plain:?}"
                );
            }
        }
    }

    #[test]
    fn purity_string_and_enum_allocations_are_unknown() {
        for (declarations, result, body, rejected) in [
            ("", "String", "return a + b;", true),
            ("", "String", "return (a + b) + a;", true),
            ("", "String", "return a;", false),
            ("", "i64", "return 1 + 2;", false),
            (
                "enum Pair { Values(i64,i64) }",
                "Pair",
                "return Pair::Values(1,2);",
                true,
            ),
            (
                "enum Pair { Values(i64,i64) }",
                "Pair",
                "let p:Pair = Pair::Values(1,2); return p;",
                true,
            ),
            (
                "enum Pair { Values(i64,i64) }",
                "Pair",
                "return Values(1,2);",
                true,
            ),
            ("enum Pair { Empty }", "Pair", "return Pair::Empty;", true),
            ("enum Pair { Empty }", "Pair", "return Empty;", true),
            (
                "enum Pair<T> { Value(T) }",
                "Pair<i64>",
                "return Pair::Value(1);",
                true,
            ),
        ] {
            for transitive in [false, true] {
                let functions = if transitive {
                    format!(
                        "@pure fn f(a:String,b:String)->{result} {{ return helper(a,b); }} fn helper(a:String,b:String)->{result} {{ {body} }}"
                    )
                } else {
                    format!("@pure fn f(a:String,b:String)->{result} {{ {body} }}")
                };
                let source = format!("{declarations} {functions} fn main() {{}}");
                let diagnostics = check_source(&source);
                let errors: Vec<_> = diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert_eq!(errors.len(), usize::from(rejected), "{source}: {errors:?}");
                if rejected {
                    assert_eq!(errors[0].code, ErrorCode::E2620, "{errors:?}");
                    assert!(errors[0].message.contains("UNKNOWN"), "{errors:?}");
                    if transitive {
                        assert!(
                            errors[0].notes.iter().any(|n| n.contains("f -> helper")),
                            "{errors:?}"
                        );
                    }
                }
                let plain = check_source(&source.replace("@pure ", ""));
                assert!(
                    plain.iter().all(|d| d.severity != Severity::Error),
                    "{plain:?}"
                );
            }
        }
    }

    #[test]
    fn purity_try_propagation_is_unknown() {
        for (declarations, params, args, result, body) in [
            (
                "",
                "x:Option<i64>",
                "x",
                "Option<i64>",
                "let n = x?; return x;",
            ),
            (
                "",
                "x:Result<i64,i64>",
                "x",
                "Result<i64,i64>",
                "let n = x?; return x;",
            ),
            (
                "interface Into<T> { fn into(self)->T; } class Dest {} class Source implements Into<Dest> { pub dest:Dest; pub fn into(self)->Dest { println(97); return self.dest; } }",
                "x:Result<i64,Source>, y:Result<i64,Dest>",
                "x,y",
                "Result<i64,Dest>",
                "let n = x?; return y;",
            ),
        ] {
            for transitive in [false, true] {
                let functions = if transitive {
                    format!(
                        "@pure fn f({params})->{result} {{ return helper({args}); }} fn helper({params})->{result} {{ {body} }}"
                    )
                } else {
                    format!("@pure fn f({params})->{result} {{ {body} }}")
                };
                let source = format!("{declarations} {functions} fn main() {{}}");
                let diagnostics = check_source(&source);
                let errors: Vec<_> = diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                assert_eq!(errors[0].code, ErrorCode::E2620, "{errors:?}");
                assert!(errors[0].message.contains("UNKNOWN"), "{errors:?}");
                if transitive {
                    assert!(
                        errors[0].notes.iter().any(|n| n.contains("f -> helper")),
                        "{errors:?}"
                    );
                }
                let plain = check_source(&source.replace("@pure ", ""));
                assert!(
                    plain.iter().all(|d| d.severity != Severity::Error),
                    "{plain:?}"
                );
            }
        }
    }

    #[test]
    fn purity_cause_path() {
        let errors = check_source(
            "@pure fn f() { middle(); } fn middle() { leaf(); } fn leaf() { println(1); } fn main() {}",
        );
        assert!(
            errors.iter().any(|d| d.code == ErrorCode::E2620
                && d.notes.iter().any(|n| n.contains("f -> middle -> leaf"))),
            "{errors:?}"
        );
    }

    #[test]
    fn purity_attribute_syntax_and_storage() {
        for source in [
            "@pure() fn f() {}",
            "@pure class C {}",
            "@effects(pure) fn f() {}",
        ] {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (_, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(!errors.is_empty(), "{source}");
        }
        let tokens = crate::lexer::Lexer::new("@pure pub fn f() {}")
            .tokenize()
            .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let json = serde_json::to_string(&program).unwrap();
        let restored: Program = serde_json::from_str(&json).unwrap();
        let Item::Function(f) = &restored.items[0] else {
            panic!("function");
        };
        assert_eq!(f.effect_contracts[0].kind, EffectContractKind::Pure);
    }

    #[test]
    fn purity_propagation_work_bounds() {
        for n in [16, 64, 256, 1024] {
            for shape in ["chain", "fan", "cycle"] {
                let ids: Vec<_> = (0..n).map(|i| FunctionId::free(format!("f{i}"))).collect();
                let mut nodes: HashMap<_, _> =
                    ids.iter().map(|&id| (id, Node::default())).collect();
                nodes.get_mut(&ids[n - 1]).unwrap().summary = PuritySummary(15);
                let mut reverse: HashMap<_, Vec<_>> = HashMap::new();
                for i in 0..n - 1 {
                    let callee = if shape == "fan" { n - 1 } else { i + 1 };
                    reverse.entry(ids[callee]).or_default().push((ids[i], None));
                }
                if shape == "cycle" {
                    reverse.entry(ids[0]).or_default().push((ids[n - 1], None));
                }
                let edges = n - 1 + usize::from(shape == "cycle");
                let work = propagate(&mut nodes, &reverse, &ids);
                assert_eq!(work, 4 * edges);
                assert!(nodes.values().all(|n| n.summary == PuritySummary(15)));
                println!("{shape} nodes={n} edges={edges} edge_visits={work}");
            }
        }
    }
}

#[derive(Default)]
struct Node {
    summary: PuritySummary,
    roots: [Option<Span>; 4],
    next: [Option<(FunctionId, Option<Span>)>; 4],
}
impl Node {
    fn seed(&mut self, bits: PuritySummary, span: Span) {
        self.summary.insert(bits);
        for (i, bit) in PuritySummary::BITS.iter().enumerate() {
            if bits.contains(*bit) && self.roots[i].is_none() {
                self.roots[i] = Some(span);
            }
        }
    }
}

/// Each bit enters each node once. Edges are visited at most four times;
/// next-hop witnesses share paths rather than copying a chain at every node.
fn propagate(
    nodes: &mut HashMap<FunctionId, Node>,
    reverse: &HashMap<FunctionId, Vec<(FunctionId, Option<Span>)>>,
    seeds: &[FunctionId],
) -> usize {
    let mut queue = VecDeque::new();
    for &id in seeds {
        for (i, bit) in PuritySummary::BITS.iter().enumerate() {
            if nodes[&id].summary.contains(*bit) {
                queue.push_back((id, i));
            }
        }
    }
    let mut work = 0;
    while let Some((callee, i)) = queue.pop_front() {
        for &(caller, span) in reverse.get(&callee).into_iter().flatten() {
            work += 1;
            let node = nodes.get_mut(&caller).expect("indexed caller");
            let bit = PuritySummary::BITS[i];
            if !node.summary.contains(bit) {
                node.summary.insert(bit);
                node.next[i] = Some((callee, span));
                queue.push_back((caller, i));
            }
        }
    }
    work
}

impl TypeChecker {
    fn purity_body(
        &self,
        body: &Block,
        params: &[Param],
        asynchronous: bool,
        external: bool,
    ) -> Node {
        let mut node = Node::default();
        if asynchronous {
            node.seed(PuritySummary::NONDETERMINISTIC, body.span);
        }
        if external {
            node.seed(PuritySummary::UNKNOWN, body.span);
        }
        let mut references = crate::semantic::scopes::Scopes::with_frame(params.iter().map(|p| {
            (
                p.name.clone(),
                matches!(p.mode, ParamMode::Reference { .. }),
            )
        }));
        let mut walk = AstWalk::new(AstEvent::Block(body));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => references.push(),
                AstEvent::ExitScope => references.pop(),
                AstEvent::Bind(name) => references.insert(name.to_owned(), false),
                AstEvent::Expr(expr) => {
                    let bit = match expr {
                        // Allocation is outside the supported purity subset.
                        // Literal allocations have no resolved call edge, and
                        // a constructor's body alone cannot prove allocation pure.
                        Expr::ArrayLiteral(..) | Expr::ObjectLiteral(_) | Expr::New(_) => {
                            Some(PuritySummary::UNKNOWN)
                        }
                        // Failure paths can allocate an enum and invoke an
                        // implicit Into conversion absent from the call graph.
                        Expr::TryPropagate(..) => Some(PuritySummary::UNKNOWN),
                        Expr::Binary(b)
                            if b.op == BinOp::Add
                                && self.expr_types.get(&expr.id()) == Some(&Type::String) =>
                        {
                            Some(PuritySummary::UNKNOWN)
                        }
                        // Contextual variants can bypass call-edge recording.
                        // Use the same resolved enum identity as lowering, not
                        // the presence of an ordinary function call edge.
                        Expr::Call(_) | Expr::Var(..)
                            if self.enum_variant_resolutions.contains_key(&expr.id()) =>
                        {
                            Some(PuritySummary::UNKNOWN)
                        }
                        Expr::StaticCall(c) if self.static_call_enum_key(&c.class).is_some() => {
                            Some(PuritySummary::UNKNOWN)
                        }
                        Expr::StaticField(c) if self.static_call_enum_key(&c.class).is_some() => {
                            Some(PuritySummary::UNKNOWN)
                        }
                        Expr::FieldAccess(..) | Expr::StaticField(..) | Expr::Index(..) => {
                            Some(PuritySummary::READ_EXTERNAL)
                        }
                        Expr::StaticCall(c)
                            if self.is_static_field_call(c)
                                && self
                                    .symbols
                                    .lookup_module_func(&c.class, &c.method)
                                    .is_none_or(|f| f.constant.is_none()) =>
                        {
                            Some(PuritySummary::READ_EXTERNAL)
                        }
                        Expr::Var(name, ..) if references.get(name).copied().unwrap_or(false) => {
                            Some(PuritySummary::READ_EXTERNAL)
                        }
                        Expr::Print(..) => Some(PuritySummary::WRITE_EXTERNAL),
                        Expr::Await(..) | Expr::Select(..) => Some(PuritySummary::NONDETERMINISTIC),
                        Expr::Binary(b)
                            if matches!(b.op, BinOp::Eq | BinOp::Ne)
                                && self.expr_types.get(&b.lhs.id()).is_some_and(|t| {
                                    !matches!(t, Type::I64 | Type::F64 | Type::Bool | Type::String)
                                }) =>
                        {
                            Some(PuritySummary::NONDETERMINISTIC)
                        }
                        Expr::Lambda(_) => {
                            // Closure-typed values allocate an environment even
                            // without captures. Plain function values do not.
                            // Neither form executes its body at creation time.
                            walk.skip_children();
                            matches!(self.expr_types.get(&expr.id()), Some(Type::Closure(..)))
                                .then_some(PuritySummary::UNKNOWN)
                        }
                        _ => None,
                    };
                    if let Some(bit) = bit {
                        node.seed(bit, expr.span());
                    }
                }
                AstEvent::Stmt(stmt) => {
                    let bit = match stmt {
                        Stmt::FieldAssign(_)
                        | Stmt::StaticFieldAssign(_)
                        | Stmt::IndexAssign(_) => Some(PuritySummary::WRITE_EXTERNAL),
                        Stmt::Assign(s) if references.get(&s.name).copied().unwrap_or(false) => {
                            Some(PuritySummary::WRITE_EXTERNAL)
                        }
                        Stmt::Lock(_) => Some(PuritySummary::NONDETERMINISTIC),
                        Stmt::For(s) if !matches!(s.iterable, Expr::Range(_)) => {
                            Some(PuritySummary::READ_EXTERNAL)
                        }
                        _ => None,
                    };
                    if let Some(bit) = bit {
                        node.seed(bit, stmt.span());
                    }
                }
                _ => {}
            }
        }
        node
    }

    pub(super) fn report_purity(
        &mut self,
        program: &Program,
        graph: &CallGraph,
        declarations: &[(FunctionId, EffectContract)],
        templates: bool,
    ) {
        let mut nodes = HashMap::new();
        let mut spans = HashMap::new();
        let mut order = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let id = FunctionId::free(&f.name);
                    nodes.insert(
                        id,
                        self.purity_body(&f.body, &f.params, f.is_async, f.rust_bridge.is_some()),
                    );
                    spans.insert(id, f.span);
                    order.push(id);
                }
                Item::Class(c) => {
                    for m in &c.methods {
                        let id = FunctionId::method(TypeId::local(&c.name), &m.name);
                        nodes.insert(id, self.purity_body(&m.body, &m.params, m.is_async, false));
                        spans.insert(id, m.span);
                        order.push(id);
                    }
                }
                Item::Interface(i) => {
                    for m in &i.methods {
                        if let Some(body) = &m.default_body {
                            let id = interface_default_effect_id(&i.name, &m.name);
                            nodes.insert(id, self.purity_body(body, &m.params, false, false));
                            spans.insert(id, m.span);
                            order.push(id);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut reverse: HashMap<_, Vec<_>> = HashMap::new();
        let mut external_summaries = HashMap::new();
        let mut dynamic_targets = HashMap::new();
        for (&caller, sites) in graph.iter() {
            let Some(&span) = spans.get(&caller) else {
                continue;
            };
            if sites.has_unknown || !sites.indirect_spans.is_empty() {
                nodes.get_mut(&caller).unwrap().seed(
                    PuritySummary::UNKNOWN,
                    sites
                        .unknown_span
                        .or_else(|| sites.indirect_spans.values().next().copied())
                        .unwrap_or(span),
                );
            }
            for target in &sites.virtual_calls {
                let dynamic = *dynamic_targets.entry(*target).or_insert_with(|| {
                    target.owner_type().is_none_or(|owner| {
                        let name = owner.to_string();
                        self.symbols.lookup_interface(&name).is_some()
                            || self
                                .resolved_method_class(&name, target.name().as_ref())
                                .and_then(|owner| self.symbols.lookup_class(&owner))
                                .and_then(|c| c.methods.get(target.name().as_ref()))
                                .is_none_or(|m| m.is_open)
                    })
                });
                if dynamic {
                    nodes.get_mut(&caller).unwrap().seed(
                        PuritySummary::UNKNOWN,
                        sites.virtual_spans.get(target).copied().unwrap_or(span),
                    );
                }
            }
            for target in &sites.targets {
                let target_local = self.effect_index.local_target(*target);
                let site = sites.call_spans.get(target).copied();
                if nodes.contains_key(&target_local) {
                    reverse
                        .entry(target_local)
                        .or_default()
                        .push((caller, site));
                } else {
                    let summary = *external_summaries.entry(*target).or_insert_with(|| {
                        self.effect_queries
                            .as_ref()
                            .and_then(|(q, unit)| {
                                let (provider, id) =
                                    q.external_target(*unit, target, &self.effect_index.imports)?;
                                q.purity_facts.borrow().get(&provider)?.get(&id).copied()
                            })
                            .unwrap_or(PuritySummary::UNKNOWN)
                    });
                    nodes
                        .get_mut(&caller)
                        .unwrap()
                        .seed(summary, site.unwrap_or(span));
                }
            }
        }
        propagate(&mut nodes, &reverse, &order);
        if !templates && let Some((queries, unit)) = &self.effect_queries {
            queries.purity_facts.borrow_mut().insert(
                *unit,
                nodes.iter().map(|(&id, n)| (id, n.summary)).collect(),
            );
        }
        for (id, contract) in declarations
            .iter()
            .filter(|(_, c)| c.kind == EffectContractKind::Pure)
        {
            let Some(node) = nodes.get(id) else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2620,
                        format!("@pure cannot be proven for `{id}`: UNKNOWN body"),
                    )
                    .with_label(Label::primary(contract.span, "purity declared here")),
                );
                continue;
            };
            let Some(i) = PuritySummary::BITS
                .iter()
                .position(|b| node.summary.contains(*b))
            else {
                continue;
            };
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2620,
                format!(
                    "@pure violated by `{id}`: {}",
                    PuritySummary::BITS[i].name()
                ),
            )
            .with_label(Label::primary(contract.span, "purity declared here"));
            let mut current = *id;
            let mut path = vec![id.to_string()];
            while let Some((next, span)) = nodes[&current].next[i] {
                if let Some(span) = span {
                    diagnostic =
                        diagnostic.with_label(Label::secondary(span, format!("calls `{next}`")));
                }
                path.push(next.to_string());
                current = next;
            }
            if let Some(span) = nodes[&current].roots[i] {
                diagnostic =
                    diagnostic.with_label(Label::secondary(span, "operation prevents purity"));
            }
            diagnostic = diagnostic.with_note(format!("purity cause: {}", path.join(" -> ")));
            self.push(diagnostic);
        }
    }
}

use super::*;

/// Count the real builder's work, including typed-array alias edge attempts,
/// writeback, base-constructor arguments and collection summaries. The parser
/// fixture puts all statement kinds in one root; checking legality is covered
/// by the source regressions.
#[test]
fn function_flow_storage_constraint_construction_is_linear() {
    let safe = FunctionId::free("safe");
    let slow = FunctionId::free("slow");
    let replace = FunctionId::free("replace");
    let base = FunctionId::method(TypeId::local("Base"), "init");
    let callback = Type::Fn(vec![Type::I64], Box::new(Type::I64));
    for count in [8, 32, 128, 512] {
        let block = "if true { let a=[safe]; replace(&a[0]); a.push(slow); super.init(slow); let f=a[0]; f(1); }";
        let source = format!("fn run() {{ {} }}", block.repeat(count));
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let Item::Function(function) = &program.items[0] else {
            panic!()
        };
        let mut types = HashMap::new();
        let mut resolved = HashMap::new();
        let mut origins = HashMap::new();
        let mut super_calls = HashMap::new();
        for event in AstWalk::new(AstEvent::Block(&function.body)) {
            match event {
                AstEvent::Expr(e) => {
                    if matches!(e, Expr::ArrayLiteral(..))
                        || matches!(e, Expr::Var(name,..) if name == "a")
                    {
                        types.insert(e.id(), Type::Array(Box::new(callback.clone())));
                    }
                    if let Expr::Var(name, ..) = e
                        && let Some(id) = match name.as_str() {
                            "safe" => Some(safe),
                            "slow" => Some(slow),
                            _ => None,
                        }
                    {
                        origins.insert(e.span(), Some(id));
                        types.insert(e.id(), callback.clone());
                    }
                    if matches!(e, Expr::Call(c) if c.callee == "replace") {
                        resolved.insert(e.id(), Some(replace));
                    }
                }
                AstEvent::Stmt(Stmt::SuperInit(s)) => {
                    super_calls.insert(s.span, base);
                }
                _ => {}
            }
        }
        let mut builder = Builder {
            solver: Solver::default(),
            exprs: HashMap::new(),
            origins,
            calls: HashMap::new(),
            fields: HashMap::new(),
            memberwise: HashMap::new(),
            classes: HashSet::new(),
            open_methods: HashSet::new(),
            storage_escape: false,
            forwarded: HashMap::new(),
            types: &types,
            resolved: &resolved,
            super_calls,
            escape: 0,
        };
        builder.escape = builder.solver.node();
        builder.signature(safe, 1);
        builder.signature(slow, 1);
        let reference = builder.signature(replace, 1);
        builder
            .solver
            .signatures
            .get_mut(&replace)
            .unwrap()
            .references[0] = true;
        builder.solver.target(reference.params[0], slow, 0);
        let constructor = builder.signature(base, 1);
        let field = builder.field("callback");
        builder.solver.edge(constructor.params[0], field);
        builder.root(
            FunctionId::free("run"),
            &[],
            AstEvent::Block(&function.body),
            false,
        );
        builder.solver.solve();
        let solver = &builder.solver;
        assert!(solver.nodes.len() <= 32 * count + 16);
        assert!(solver.edges <= 32 * count + 16);
        assert!(solver.edge_attempts <= 96 * count + 16);
        assert!(solver.operations <= 2 * (LIMIT + 5) * solver.edges);
        println!(
            "storage-flow sites={count} nodes={} edges={} edge_attempts={} propagation_operations={}",
            solver.nodes.len(),
            solver.edges,
            solver.edge_attempts,
            solver.operations
        );
    }
}

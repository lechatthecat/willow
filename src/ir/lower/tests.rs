use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;

#[test]
fn construction_identity_uses_normalized_type_alias() {
    use crate::semantic::ids::{SemanticType, TypeId};
    let tokens = Lexer::new(
        "class Alias { n: i64; } fn f() { let a = new Alias(1); let b = Alias { n: 2 }; }",
    )
    .tokenize()
    .unwrap();
    let (program, errors) = Parser::new(tokens).parse();
    assert!(errors.is_empty());
    let normalized = HashMap::from([(
        Type::Named("Alias".into()),
        Type::Named("sales::Amount".into()),
    )]);
    let tables = PartialCheckerTables {
        normalized_types: Some(&normalized),
        ..Default::default()
    };
    let (hir, diagnostics) = lower_program_with_partial(&program, &tables);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    for statement in &hir.functions[0].body {
        if let HirStmt::Let { value, .. } = statement {
            let (HirExprKind::New { class, .. } | HirExprKind::ObjectLiteral { class, .. }) =
                &value.kind
            else {
                panic!("constructor expected");
            };
            assert_eq!(*class, TypeId::from_source_name("sales::Amount"));
            assert_eq!(value.ty, SemanticType::Named(*class));
        }
    }
}

#[test]
fn resolution_inherits_static_fields_and_resolves_self() {
    let (hir, diagnostics) = lower_src(
        "open class Base { pub static value: i64 = 7; } class Child extends Base { pub static fn read() -> i64 { return Self::value; } }",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let child = crate::semantic::ids::TypeId::local("Child");
    assert_eq!(
        hir.resolution.classes[&child].static_fields["value"],
        crate::semantic::ids::SemanticType::I64
    );
    let method = hir
        .classes
        .iter()
        .find(|class| class.name == child)
        .unwrap()
        .methods
        .iter()
        .find(|method| method.name.unqualified_name().as_deref().unwrap_or("") == "read")
        .unwrap();
    let HirStmt::Return {
        value: Some(value), ..
    } = &method.body[0]
    else {
        panic!("return expected")
    };
    assert!(matches!(&value.kind, HirExprKind::StaticField { class, .. } if *class == child));
}

#[test]
fn resolution_snapshot_preserves_payload_order_and_parameter_modes() {
    use crate::semantic::ids::{FunctionId, TypeId};
    let (hir, diagnostics) = lower_src(
        "interface Reader<T> { fn read(self) -> T; } interface NamedReader extends Reader { fn name(self) -> String; } enum Pair { Empty, Values(i64, String) } class Item { pub z: i64; pub a: String; pub init(self, n: i64) { self.z = n; self.a = \"x\"; } pub static fn twice(n: i64) -> i64 { return n + n; } } fn touch(x: &mut i64) { x = 1; }",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let reader = &hir.resolution.interfaces[&TypeId::local("Reader")];
    assert_eq!(reader.type_params, vec![TypeId::local("T")]);
    assert_eq!(
        reader.methods["read"].return_type,
        crate::semantic::ids::SemanticType::Named(TypeId::local("T"))
    );
    assert_eq!(
        hir.resolution.interfaces[&TypeId::local("NamedReader")].extends,
        vec![TypeId::local("Reader")]
    );
    let pair = &hir.resolution.enums[&TypeId::local("Pair")];
    assert_eq!(pair.variants[1].name, "Values");
    assert_eq!(pair.variants[1].tag, 1);
    assert_eq!(
        pair.variants[1].payloads,
        vec![
            crate::semantic::ids::SemanticType::I64,
            crate::semantic::ids::SemanticType::String
        ]
    );
    let item = &hir.resolution.classes[&TypeId::local("Item")];
    assert_eq!(
        item.fields
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert_eq!(
        item.constructor.as_ref().unwrap().params,
        vec![crate::semantic::ids::SemanticType::I64]
    );
    assert!(item.methods["twice"].is_static);
    assert!(matches!(
        hir.resolution.functions[&FunctionId::free("touch")].param_modes[0],
        crate::parser::ast::ParamMode::Reference { mutable: true, .. }
    ));
    assert_eq!(
        hir.resolution.enums[&TypeId::local("Option")].variants[0].name,
        "Some"
    );
}

/// Deeply nested expressions must lower without exhausting a modest
/// stack. Lowering is recursive, so the cost that matters is the size of
/// ONE level: the arms of `lower_expr_inner` live in their own functions
/// precisely so a level pays for the arm it takes instead of for every arm
/// at once. With them inline, a debug build spent ~63KB per
/// level and 256 levels needed 16MiB; a level now costs ~6KB, so this fits
/// several times over.
#[test]
fn deep_expression_nesting_lowers_within_a_four_megabyte_stack() {
    const DEPTH: usize = 256;
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024)
        .spawn(|| {
            let chain = vec!["1"; DEPTH].join(" + ");
            let src = format!("fn main() {{ let total = {chain}; println(total); }}");
            let (hir, diags) = lower_src(&src);
            assert!(diags.is_empty(), "{diags:?}");
            assert_eq!(hir.functions.len(), 1);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn operator_lowering_uses_heap_frames_on_success_and_error() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            for fail_after_left in [false, true] {
                let span = Span::new(0, 1, 1, 1);
                let mut expr = Expr::Integer(1, span, ExprId::fresh());
                for depth in 0..50_000 {
                    expr = if depth % 2 == 0 {
                        Expr::Unary(Box::new(UnaryExpr {
                            id: ExprId::fresh(),
                            op: UnaryOp::Neg,
                            expr,
                            span,
                        }))
                    } else {
                        Expr::Binary(Box::new(BinaryExpr {
                            id: ExprId::fresh(),
                            op: BinOp::Add,
                            lhs: expr,
                            rhs: Expr::Integer(1, span, ExprId::fresh()),
                            span,
                        }))
                    };
                }
                if fail_after_left {
                    expr = Expr::Binary(Box::new(BinaryExpr {
                        id: ExprId::fresh(),
                        op: BinOp::Add,
                        lhs: expr,
                        rhs: Expr::Var("missing".into(), span, ExprId::fresh()),
                        span,
                    }));
                }
                let program = Program {
                    instantiation_types: Vec::new(),
                    type_uses: Vec::new(),
                    module: None,
                    imports: vec![],
                    items: vec![Item::Function(FunctionDecl {
                        requires: Vec::new(),
                        ensures: Vec::new(),
                        effect_contracts: Vec::new(),
                        return_type_span: None,
                        type_params: Vec::new(),
                        name: "main".into(),
                        public: false,
                        is_async: false,
                        params: vec![],
                        return_type: Type::Void,
                        body: Block {
                            id: crate::parser::ast::BodyId::fresh(),
                            stmts: vec![Stmt::Expr(crate::parser::ast::ExprStmt { expr, span })],
                            span,
                        },
                        span,
                        rust_bridge: None,
                        constant: None,
                    })],
                };
                let (hir, diagnostics) = lower_program(&program);
                let mut count = 0;
                let mut root_type = None;
                for function in &hir.functions {
                    for statement in &function.body {
                        if let HirStmt::Expr(expr) = statement {
                            count = expr.walk_postorder(true).count();
                            root_type = Some(expr.ty.to_source());
                        }
                    }
                }
                drop(hir);
                drop(program);
                if fail_after_left {
                    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                } else {
                    assert!(diagnostics.is_empty(), "{diagnostics:?}");
                    assert_eq!(root_type, Some(Type::I64));
                    assert_eq!(count, 75_001);
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn lower_src(src: &str) -> (HirProgram, Vec<Diagnostic>) {
    let tokens = Lexer::new(src).tokenize().expect("lexing failed");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
    lower_program(&program)
}

/// Lower a program expected to be wholly within slice 1; assert no
/// diagnostics and return the first function's body.
fn lower_body(src: &str) -> Vec<HirStmt> {
    let (hir, diags) = lower_src(src);
    assert!(
        diags.is_empty(),
        "unexpected lowering diagnostics: {diags:?}"
    );
    hir.functions
        .into_iter()
        .next()
        .expect("at least one function")
        .body
}

/// Extract the value expression of the first `return` statement in a body.
fn first_return(body: &[HirStmt]) -> &HirExpr {
    body.iter()
        .find_map(|s| match s {
            HirStmt::Return { value: Some(v), .. } => Some(v),
            _ => None,
        })
        .expect("a return with a value")
}

fn return_ty(src: &str) -> Type {
    let body = lower_body(src);
    first_return(&body).ty.to_source()
}

// 1. integer literal → I64
#[test]
fn p01_integer_literal_is_i64() {
    assert_eq!(return_ty("fn f() -> i64 { return 7; }"), Type::I64);
}

// 2. float literal → F64
#[test]
fn p02_float_literal_is_f64() {
    assert_eq!(return_ty("fn f() -> f64 { return 1.5; }"), Type::F64);
}

// 3. bool literal → Bool
#[test]
fn p03_bool_literal_is_bool() {
    assert_eq!(return_ty("fn f() -> bool { return true; }"), Type::Bool);
}

// 4. string literal → String
#[test]
fn p04_string_literal_is_string() {
    assert_eq!(
        return_ty("fn f() -> String { return \"hi\"; }"),
        Type::String
    );
}

// 5. parameter variable read carries its declared type
#[test]
fn p05_param_var_has_declared_type() {
    assert_eq!(return_ty("fn f(a: i64) -> i64 { return a; }"), Type::I64);
}

// 6. f64 parameter variable read
#[test]
fn p06_param_var_f64() {
    assert_eq!(return_ty("fn f(a: f64) -> f64 { return a; }"), Type::F64);
}

// 7. integer addition → I64
#[test]
fn p07_i64_add_is_i64() {
    assert_eq!(return_ty("fn f() -> i64 { return 1 + 2; }"), Type::I64);
}

// 8. float addition → F64
#[test]
fn p08_f64_add_is_f64() {
    assert_eq!(return_ty("fn f() -> f64 { return 1.0 + 2.0; }"), Type::F64);
}

// 9. subtraction/multiplication/division/remainder preserve operand type
#[test]
fn p09_arithmetic_preserves_operand_type() {
    for op in ["-", "*", "/", "%"] {
        let src = format!("fn f() -> i64 {{ return 6 {op} 3; }}");
        assert_eq!(return_ty(&src), Type::I64, "op {op}");
    }
}

// 10. equality → Bool
#[test]
fn p10_eq_is_bool() {
    assert_eq!(return_ty("fn f() -> bool { return 1 == 2; }"), Type::Bool);
}

// 11. relational comparisons → Bool
#[test]
fn p11_relational_is_bool() {
    for op in ["<", "<=", ">", ">=", "!="] {
        let src = format!("fn f() -> bool {{ return 1 {op} 2; }}");
        assert_eq!(return_ty(&src), Type::Bool, "op {op}");
    }
}

// 12. logical and/or → Bool
#[test]
fn p12_logical_is_bool() {
    assert_eq!(
        return_ty("fn f() -> bool { return true && false; }"),
        Type::Bool
    );
    assert_eq!(
        return_ty("fn f() -> bool { return true || false; }"),
        Type::Bool
    );
}

// 13. unary negation preserves operand type
#[test]
fn p13_unary_neg_preserves_type() {
    assert_eq!(return_ty("fn f(a: i64) -> i64 { return -a; }"), Type::I64);
}

// 14. unary not → Bool
#[test]
fn p14_unary_not_is_bool() {
    assert_eq!(
        return_ty("fn f(a: bool) -> bool { return !a; }"),
        Type::Bool
    );
}

// 15. nested binary propagates the operand type outward
#[test]
fn p15_nested_binary_type() {
    assert_eq!(
        return_ty("fn f() -> i64 { return (1 + 2) * 3 - 4; }"),
        Type::I64
    );
}

// 16. comparison of arithmetic sub-expressions is still Bool
#[test]
fn p16_compare_of_arithmetic_is_bool() {
    assert_eq!(
        return_ty("fn f() -> bool { return 1 + 2 < 3 * 4; }"),
        Type::Bool
    );
}

// 17. free-function call carries the callee's return type
#[test]
fn p17_call_has_callee_return_type() {
    let ty = return_ty("fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }");
    assert_eq!(ty, Type::I64);
}

// 18. call with arguments lowers each argument
#[test]
fn p18_call_with_args() {
    let (hir, diags) = lower_src(
        "fn add(a: i64, b: i64) -> i64 { return a + b; } \
             fn f() -> i64 { return add(1, 2); }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|fun| fun.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .expect("function f");
    match &first_return(&f.body).kind {
        HirExprKind::Call { callee, args } => {
            assert_eq!(
                callee,
                &crate::semantic::ids::FunctionId::free_from_source_name("add")
            );
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].ty, Type::I64);
        }
        other => panic!("expected call, got {other:?}"),
    }
}

#[test]
fn free_callee_reservation_shares_repeated_names() {
    for n in [8, 16, 32, 64] {
        let source = format!(
            "fn open() -> i64 {{ return 1; }} fn main() {{ {} let open = 7; println(open); }}",
            "println(open());".repeat(n)
        );
        let (hir, diags) = lower_src(&source);
        assert!(diags.is_empty(), "{diags:?}");
        let main = hir
            .functions
            .iter()
            .find(|f| f.name.is_free_named("main"))
            .unwrap();
        assert_eq!(main.body.len(), n + 2);
        let HirStmt::Let { name, .. } = &main.body[n] else {
            panic!("missing local")
        };
        // Repeated calls reserve one spelling; they do not consume suffixes
        // or emit additional HIR instructions.
        assert_eq!(name, "open$2");
        eprintln!(
            "free callee calls={n} statements={} local={name}",
            main.body.len()
        );
    }
}

// 19. print expression is Void
#[test]
fn p19_print_is_void() {
    let body = lower_body("fn f() { print(1); }");
    match &body[0] {
        HirStmt::Expr(e) => {
            assert_eq!(e.ty, Type::Void);
            assert!(matches!(e.kind, HirExprKind::Print { newline: false, .. }));
        }
        other => panic!("expected expr stmt, got {other:?}"),
    }
}

// 20. println sets the newline flag
#[test]
fn p20_println_newline_flag() {
    let body = lower_body("fn f() { println(1); }");
    match &body[0] {
        HirStmt::Expr(e) => {
            assert!(matches!(e.kind, HirExprKind::Print { newline: true, .. }));
        }
        other => panic!("expected expr stmt, got {other:?}"),
    }
}

// 21. let binds the inferred value type into scope
#[test]
fn p21_let_binds_inferred_type() {
    let ty = return_ty("fn f() -> i64 { let x = 5; return x; }");
    assert_eq!(ty, Type::I64);
}

// 22. let with an explicit annotation pins the binding type
#[test]
fn p22_let_annotation_pins_type() {
    let body = lower_body("fn f() { let x: f64 = 2.0; }");
    match &body[0] {
        HirStmt::Let { mutable, value, .. } => {
            assert!(!mutable);
            assert_eq!(value.ty, Type::F64);
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 23. let mut records mutability
#[test]
fn p23_let_mut_records_mutability() {
    let body = lower_body("fn f() { let mut x = 1; }");
    assert!(matches!(body[0], HirStmt::Let { mutable: true, .. }));
}

// 24. assignment lowers its value expression
#[test]
fn p24_assign_lowers_value() {
    let body = lower_body("fn f() { let mut x = 1; x = 2; }");
    match &body[1] {
        HirStmt::Assign { name, value, .. } => {
            assert_eq!(name, "x");
            assert_eq!(value.ty, Type::I64);
        }
        other => panic!("expected assign, got {other:?}"),
    }
}

// 25. if lowers a Bool condition and both branches
#[test]
fn p25_if_cond_is_bool_with_branches() {
    let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } else { print(2); } }");
    match &body[0] {
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            assert_eq!(cond.ty, Type::Bool);
            assert_eq!(then_branch.len(), 1);
            assert_eq!(else_branch.as_ref().map(|b| b.len()), Some(1));
        }
        other => panic!("expected if, got {other:?}"),
    }
}

// 26. if without else has no else branch
#[test]
fn p26_if_without_else() {
    let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } }");
    assert!(matches!(
        &body[0],
        HirStmt::If {
            else_branch: None,
            ..
        }
    ));
}

// 27. while lowers a Bool condition and a body
#[test]
fn p27_while_cond_is_bool() {
    let body = lower_body("fn f(a: bool) { while a { print(1); } }");
    match &body[0] {
        HirStmt::While { cond, body, .. } => {
            assert_eq!(cond.ty, Type::Bool);
            assert_eq!(body.len(), 1);
        }
        other => panic!("expected while, got {other:?}"),
    }
}

// 28. return without a value lowers to None
#[test]
fn p28_bare_return() {
    let body = lower_body("fn f() { return; }");
    assert!(matches!(body[0], HirStmt::Return { value: None, .. }));
}

// 29. function parameters and return type are carried on HirFunction
#[test]
fn p29_function_signature_carried() {
    let (hir, diags) = lower_src("fn f(a: i64, b: bool) -> i64 { return a; }");
    assert!(diags.is_empty(), "{diags:?}");
    let f = &hir.functions[0];
    assert_eq!(f.name.name().as_ref(), "f");
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.params[0].ty, Type::I64);
    assert_eq!(f.params[1].ty, Type::Bool);
    assert_eq!(f.return_type, Type::I64);
}

// 30. a block-scoped binding does not leak to an outer scope
#[test]
fn p30_block_scope_does_not_leak() {
    // `x` is declared inside the `if` block; reading `x` after the block
    // would be an unbound-variable internal error if scopes leaked. Here the
    // outer `return y` only sees the outer binding, so lowering succeeds.
    let (_, diags) =
        lower_src("fn f(c: bool) -> i64 { let y = 1; if c { let x = 2; print(x); } return y; }");
    assert!(diags.is_empty(), "{diags:?}");
}

// 31. an out-of-slice construct (`for` over a non-iterable String) is
// reported, not panicked on. Map iteration lowers through `entries()`.
#[test]
fn p31_unsupported_construct_reports_diagnostic() {
    let (_, diags) = lower_src("fn f(s: String) { for v in s { print(v); } }");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("does not yet support")),
        "expected an unsupported-construct diagnostic, got {diags:?}"
    );
}

// 32. multi-statement function lowers every statement in order
#[test]
fn p32_multi_statement_function() {
    let body = lower_body("fn f() -> i64 { let a = 1; let b = 2; let c = a + b; return c; }");
    assert_eq!(body.len(), 4);
    assert!(matches!(body[0], HirStmt::Let { .. }));
    assert!(matches!(body[3], HirStmt::Return { .. }));
}

// 33. an i64 array literal has type Array<i64>
#[test]
fn p33_array_literal_i64() {
    let body = lower_body("fn f() { let xs = [1, 2, 3]; }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Array(Box::new(Type::I64)));
            assert!(matches!(&value.kind, HirExprKind::Array { elements } if elements.len() == 3));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 34. an f64 array literal has type Array<f64>
#[test]
fn p34_array_literal_f64() {
    let body = lower_body("fn f() { let xs = [1.0, 2.0]; }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Array(Box::new(Type::F64)));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 35. indexing an array yields its element type
#[test]
fn p35_index_yields_element_type() {
    assert_eq!(
        return_ty("fn f() -> i64 { let xs = [4, 5, 6]; return xs[0]; }"),
        Type::I64
    );
}

// 36. a ternary takes the (shared) branch type
#[test]
fn p36_ternary_branch_type() {
    assert_eq!(
        return_ty("fn f(c: bool) -> i64 { return c ? 1 : 2; }"),
        Type::I64
    );
}

// 37. an index expression composes inside arithmetic
#[test]
fn p37_index_in_arithmetic() {
    assert_eq!(
        return_ty("fn f() -> i64 { let xs = [1, 2]; return xs[0] + xs[1]; }"),
        Type::I64
    );
}

// 38. an empty array literal is reported (element type needs context)
#[test]
fn p38_empty_array_literal_reports() {
    let (_, diags) = lower_src("fn f() { let xs: Array<i64> = []; }");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("empty array literal")),
        "expected an empty-array diagnostic, got {diags:?}"
    );
}

// 39. `new Class(...)` has the class type
#[test]
fn p39_new_has_class_type() {
    let body = lower_body("class Box { pub v: i64; } fn f() { let b = new Box(7); }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Named("Box".into()));
            assert!(
                matches!(&value.kind, HirExprKind::New { class, .. } if class == &crate::semantic::ids::TypeId::local("Box"))
            );
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 40. a field access takes the field's declared type
#[test]
fn p40_field_access_type() {
    assert_eq!(
        return_ty("class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.v; }"),
        Type::I64
    );
}

// 41. a method call takes the method's return type
#[test]
fn p41_method_call_return_type() {
    let src = "class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } } \
                   fn f() -> i64 { let b = new Box(7); return b.get(); }";
    assert_eq!(return_ty(src), Type::I64);
}

// 42. an unknown field is reported (not yet covered, e.g. inherited)
#[test]
fn p42_unknown_field_reports() {
    let (_, diags) = lower_src(
        "class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.missing; }",
    );
    assert!(
        diags.iter().any(|d| d.message.contains("field not found")),
        "expected a field-not-found diagnostic, got {diags:?}"
    );
}

// 43. an unknown method is reported
#[test]
fn p43_unknown_method_reports() {
    let (_, diags) = lower_src(
        "class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.gone(); }",
    );
    assert!(
        diags.iter().any(|d| d.message.contains("method not found")),
        "expected a method-not-found diagnostic, got {diags:?}"
    );
}

// 44. a class method body lowers into HirProgram.classes with a typed `self`
#[test]
fn p44_class_method_lowered() {
    let (hir, diags) =
        lower_src("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(hir.classes.len(), 1);
    let class = &hir.classes[0];
    assert_eq!(class.name, crate::semantic::ids::TypeId::local("Box"));
    assert_eq!(class.methods.len(), 1);
    let m = &class.methods[0];
    assert_eq!(m.name.name().as_ref(), "get");
    assert_eq!(m.params[0].name, "self");
    assert_eq!(m.params[0].ty, Type::Named("Box".into()));
}

// 45. `self.field` inside a method body resolves to the field's type
#[test]
fn p45_self_field_access_typed_in_method() {
    let (hir, diags) =
        lower_src("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
    assert!(diags.is_empty(), "{diags:?}");
    let m = &hir.classes[0].methods[0];
    assert_eq!(first_return(&m.body).ty, Type::I64);
}

// 46. a method parameter is bound in the method body
#[test]
fn p46_method_param_bound_in_body() {
    let (hir, diags) = lower_src("class Box { pub fn echo(self, n: i64) -> i64 { return n; } }");
    assert!(diags.is_empty(), "{diags:?}");
    let m = &hir.classes[0].methods[0];
    assert_eq!(m.params.len(), 2, "self + n");
    assert_eq!(m.params[1].name, "n");
    assert_eq!(first_return(&m.body).ty, Type::I64);
}

// 47. a static method has no `self` parameter
#[test]
fn p47_static_method_has_no_self_param() {
    let (hir, diags) = lower_src("class Box { static fn make() -> i64 { return 1; } }");
    assert!(diags.is_empty(), "{diags:?}");
    let m = &hir.classes[0].methods[0];
    assert!(
        m.params.is_empty(),
        "static method should have no self param"
    );
}

// 48. an inherited field resolves along the base-class chain
#[test]
fn p48_inherited_field_type() {
    let src = "class A { v: i64; } class B extends A {} \
                   fn f() -> i64 { let b = new B(1); return b.v; }";
    assert_eq!(return_ty(src), Type::I64);
}

// 49. an inherited method's return type resolves along the chain
#[test]
fn p49_inherited_method_type() {
    let src = "class A { pub fn m(self) -> i64 { return 1; } } class B extends A {} \
                   fn f() -> i64 { let b = new B(); return b.m(); }";
    assert_eq!(return_ty(src), Type::I64);
}

// 50. a `for` over an array binds the loop variable to the element type
#[test]
fn p50_for_binds_element_type() {
    let (hir, diags) = lower_src(
        "fn f() -> i64 { let xs = [10]; let mut s = 0; for v in xs { s = s + v; } return s; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let body = &hir.functions[0].body;
    assert!(body.iter().any(|s| matches!(
        s,
        HirStmt::For { iterable, .. } if iterable.ty == Type::Array(Box::new(Type::I64))
    )));
}

#[test]
fn map_iteration_lowers_one_snapshot_per_loop() {
    for count in [1, 8, 32, 128] {
        let source = format!(
            "import std::collections::Map; fn f(m: Map<i64, i64>) {{ {} }}",
            "for _ in m {}".repeat(count)
        );
        let (hir, diagnostics) = lower_src(&source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut snapshots = 0;
        for statement in &hir.functions[0].body {
            if let HirStmt::For { iterable, .. } = statement {
                assert!(
                    matches!(&iterable.kind, HirExprKind::MethodCall { method, args, .. } if method == "entries" && args.is_empty())
                );
                snapshots += 1;
            }
        }
        assert_eq!(snapshots, count);
    }
}

// 51. an object literal has the class type
#[test]
fn p51_object_literal_type() {
    let body = lower_body("class P { x: i64; } fn f() { let p = P { x: 1 }; }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Named("P".into()));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 53. a static property read takes the property's type
#[test]
fn p53_static_field_read_type() {
    assert_eq!(
        return_ty("class C { static v: i64 = 0; } fn f() -> i64 { return C::v; }"),
        Type::I64
    );
}

// 54. a static call takes the static method's return type
#[test]
fn p54_static_call_return_type() {
    assert_eq!(
        return_ty(
            "class C { static fn make() -> i64 { return 1; } } fn f() -> i64 { return C::make(); }"
        ),
        Type::I64
    );
}

// 55. field/index/static-field assignment statements lower cleanly
#[test]
fn p55_assignment_statements() {
    let (hir, diags) = lower_src(
        "class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }",
    );
    assert!(diags.is_empty(), "{diags:?}");
    // A class with a static initializer contributes its own `$static_init`
    // function ahead of the user's, so pick `f` by name.
    let body = &hir
        .functions
        .iter()
        .find(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .expect("`f` lowers")
        .body;
    assert!(
        body.iter()
            .any(|s| matches!(s, HirStmt::FieldAssign { .. }))
    );
    assert!(
        body.iter()
            .any(|s| matches!(s, HirStmt::IndexAssign { .. }))
    );
    assert!(
        body.iter()
            .any(|s| matches!(s, HirStmt::StaticFieldAssign { .. }))
    );
}

// 56. a range expression has type Range<i64>
#[test]
fn p56_range_expr_type() {
    let body = lower_body("fn f() { let r = 0..3; }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Generic("Range".into(), vec![Type::I64]));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 57. a `for` over a range binds the loop variable to i64
#[test]
fn p57_range_for_binds_i64() {
    let (hir, diags) =
        lower_src("fn f() -> i64 { let mut s = 0; for i in 0..4 { s = s + i; } return s; }");
    assert!(diags.is_empty(), "{diags:?}");
    // Body lowered without an unbound-variable error means `i` was bound;
    // the addition typing i64 confirms the element type.
    let body = &hir.functions[0].body;
    assert!(body.iter().any(|s| matches!(s, HirStmt::For { .. })));
}

// 58. calling an async fn yields Task<T> at the call site
#[test]
fn p58_async_call_yields_task() {
    let (hir, diags) = lower_src("async fn g() -> i64 { return 1; } fn f() { let t = g(); }");
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .unwrap();
    let body = &f.body;
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Generic("Task".into(), vec![Type::I64]));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 59. awaiting a Task<T> yields T
#[test]
fn p59_await_task_unwraps() {
    let src = "async fn g() -> i64 { return 1; } async fn f() -> i64 { return await g(); }";
    let (hir, diags) = lower_src(src);
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .unwrap();
    assert_eq!(first_return(&f.body).ty, Type::I64);
}

// 60. awaiting the builtin sleep (Future<void>) lowers cleanly
#[test]
fn p60_await_builtin_sleep() {
    let (_, diags) = lower_src("async fn f() { await sleep(1); }");
    assert!(diags.is_empty(), "{diags:?}");
}

// 61. `?` on a Result<T, E> yields T
#[test]
fn p61_try_on_result_yields_ok_type() {
    assert_eq!(
        return_ty("fn f(r: Result<i64, String>) -> i64 { return r?; }"),
        Type::I64
    );
}

// 62. `?` on an Option<T> yields T
#[test]
fn p62_try_on_option_yields_some_type() {
    assert_eq!(
        return_ty("fn f(o: Option<i64>) -> i64 { return o?; }"),
        Type::I64
    );
}

// 63. a lambda with annotated params has an fn(..) -> ret type
#[test]
fn p63_lambda_fn_type() {
    let body = lower_body("fn f() { let d = |x: i64| x * 2; }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Fn(vec![Type::I64], Box::new(Type::I64)));
        }
        other => panic!("expected let, got {other:?}"),
    }
}

// 64. an unannotated lambda parameter is reported, not guessed
#[test]
fn p64_unannotated_lambda_param_reports() {
    let (_, diags) = lower_src("fn f() { let d = |x| x; }");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unannotated lambda parameter")),
        "{diags:?}"
    );
}

// 65. calling a local fn-typed variable is an indirect call typed by fn(..)->R
#[test]
fn p65_indirect_call_via_fn_var() {
    assert_eq!(
        return_ty("fn f(g: fn(i64) -> i64) -> i64 { return g(1); }"),
        Type::I64
    );
}

// 66. a constructor lowers as `init` with self bound; super.init lowers too
#[test]
fn p66_constructor_and_super_init() {
    let src = "open class A { v: i64; init(self, v: i64) { self.v = v; } } \
                   class B extends A { init(self, v: i64) { super.init(v); } }";
    let (hir, diags) = lower_src(src);
    assert!(diags.is_empty(), "{diags:?}");
    let b = hir
        .classes
        .iter()
        .find(|c| c.name == crate::semantic::ids::TypeId::local("B"))
        .unwrap();
    let init = &b.methods[0];
    assert_eq!(init.name.name().as_ref(), "init");
    assert_eq!(init.params[0].name, "self");
    assert!(
        init.body
            .iter()
            .any(|s| matches!(s, HirStmt::SuperInit { .. }))
    );
}

// 67. a builtin function call is typed from the seeded builtin registry
#[test]
fn p67_builtin_call_typed() {
    assert_eq!(
        return_ty("fn f() -> i64 { return gc_allocated_bytes(); }"),
        Type::I64
    );
    assert_eq!(
        return_ty("fn f() -> i64 { return gc_total_allocated_bytes(); }"),
        Type::I64
    );
}

// 68. match on a user enum: arm bodies typed, payload bindings typed
#[test]
fn p68_match_user_enum() {
    let src = "enum Shape { Circle(i64), Empty, } \
                   fn f(s: Shape) -> i64 { return match s { Shape::Circle(r) => r * 2, Shape::Empty => 0, }; }";
    assert_eq!(return_ty(src), Type::I64);
}

// 69. match on Option<i64>: variant payload substituted to i64
#[test]
fn p69_match_option_substitutes() {
    let src = "fn f(o: Option<i64>) -> i64 { return match o { Some(v) => v, None => -1, }; }";
    assert_eq!(return_ty(src), Type::I64);
}

// 70. match on Result<i64, String>: Ok/Err payloads substituted
#[test]
fn p70_match_result_substitutes() {
    let src = "fn f(r: Result<i64, String>) -> i64 { \
                   return match r { Ok(v) => v, Err(_) => 0, }; }";
    assert_eq!(return_ty(src), Type::I64);
}

// 71. a panicking (Never) arm coerces to the other arms' type
#[test]
fn p71_never_arm_coerces() {
    let src = "fn f(o: Option<i64>) -> i64 { \
                   return match o { Some(v) => v, None => panic(\"none\"), }; }";
    assert_eq!(return_ty(src), Type::I64);
}

// 72. enum variant construction has the enum type
#[test]
fn p72_enum_construction() {
    let src = "enum Shape { Circle(i64), } fn f() -> Shape { return Shape::Circle(1); }";
    assert_eq!(return_ty(src), Type::Named("Shape".into()));
}

// 73. a fieldless variant value read has the enum type
#[test]
fn p73_fieldless_variant_value() {
    let src = "enum Color { Red, Blue, } fn f() -> Color { return Color::Red; }";
    assert_eq!(return_ty(src), Type::Named("Color".into()));
}

// Source array signatures supply len/pop/freeze expression types.
#[test]
fn p74_array_source_methods() {
    for (source, expected) in [
        (
            "fn f() -> i64 { let xs = [1]; return xs.len(); }",
            Type::I64,
        ),
        (
            "fn f() -> i64 { let xs = [1]; return xs.pop(); }",
            Type::I64,
        ),
        (
            "fn f() -> FrozenArray<i64> { let xs = [1]; return xs.freeze(); }",
            Type::Generic("FrozenArray".into(), vec![Type::I64]),
        ),
    ] {
        let (hir, diagnostics) = lower_with_checker(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let HirStmt::Return {
            value: Some(value), ..
        } = hir.functions[0].body.last().unwrap()
        else {
            panic!()
        };
        assert_eq!(value.ty.to_source(), expected);
    }
}

// 75. builtin map/task/lock methods: get/contains/len/result/lock types
#[test]
fn p75_map_task_lock_builtin_methods() {
    assert_eq!(
        return_ty("fn f(m: Map<String, i64>) -> bool { return m.contains(\"k\"); }"),
        Type::Bool
    );
    let body = lower_body("fn f(m: Map<String, i64>) { let v = m.get(\"k\"); }");
    match &body[0] {
        HirStmt::Let { value, .. } => {
            assert_eq!(value.ty, Type::Generic("Option".into(), vec![Type::I64]));
        }
        other => panic!("expected let, got {other:?}"),
    }
    assert_eq!(
        return_ty(
            "async fn g() -> i64 { return 1; } async fn f() -> i64 { let t = g(); return await t; }"
        ),
        Type::I64
    );
    // `Mutex<T>` lost its accessors in mutex lowering; the blocking cell
    // and the RwLock keep theirs, and both must still lower.
    assert_eq!(
        return_ty("fn f(c: BlockingCell<i64>) -> i64 { return c.get(); }"),
        Type::I64
    );
    assert_eq!(
        return_ty("fn f(r: BlockingRwCell<i64>) -> i64 { return r.read(); }"),
        Type::I64
    );
}

/// Run the real type checker, then lower with its side tables.
fn lower_with_checker(src: &str) -> (HirProgram, Vec<Diagnostic>) {
    let tokens = Lexer::new(src).tokenize().expect("lexing failed");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "parse errors: {errs:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude registers");
    checker.check_program(&program);
    assert!(
        checker.errors.is_empty(),
        "checker errors: {:?}",
        checker.errors
    );
    let tables = SemanticFacts::from_checker(&checker);
    lower_program_with(&program, &tables)
}

#[test]
fn source_append_expansion_has_constant_operations_per_call() {
    for calls in [1, 8, 32] {
        let source = format!(
            "fn main() {{ let xs = [0]; {} }}",
            "xs.push(1);".repeat(calls)
        );
        let (hir, errors) = lower_with_checker(&source);
        assert!(errors.is_empty(), "{errors:?}");
        let body = &hir.functions[0].body;
        assert_eq!(body.len(), 1 + 5 * calls);
        for expansion in body[1..].chunks_exact(5) {
            assert!(matches!(&expansion[0], HirStmt::Let { .. }));
            assert!(matches!(&expansion[1], HirStmt::Let { .. }));
            assert!(
                matches!(&expansion[2], HirStmt::Let { value: HirExpr { kind: HirExprKind::MethodCall { method, .. }, .. }, .. } if method == "__abi_len")
            );
            assert!(
                matches!(&expansion[3], HirStmt::Expr(HirExpr { kind: HirExprKind::MethodCall { method, .. }, .. }) if method == "__abi_resize")
            );
            assert!(matches!(&expansion[4], HirStmt::IndexAssign { .. }));
        }
    }
}

// 76. an unannotated lambda types via the checker's inferred fn(..) table
#[test]
fn p76_unannotated_lambda_via_checker_tables() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); } \
                   fn g() -> i64 { return apply(|x| x * 3, 14); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let g = hir
        .functions
        .iter()
        .find(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("g"))
        .unwrap();
    // The lambda argument inside the call carries the full fn type.
    let HirStmt::Return { value: Some(v), .. } = &g.body[0] else {
        panic!("expected return");
    };
    let HirExprKind::Call { args, .. } = &v.kind else {
        panic!("expected call");
    };
    assert_eq!(args[0].ty, Type::Fn(vec![Type::I64], Box::new(Type::I64)));
}

// 77. an unqualified variant construction resolves via the checker table
#[test]
fn p77_unqualified_enum_construction_via_checker_tables() {
    let src = "enum Msg { Num(i64), } fn f(b: bool) -> Msg { return Num(7); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let f = &hir.functions[0];
    assert_eq!(first_return(&f.body).ty, Type::Named("Msg".into()));
    assert!(matches!(
        &first_return(&f.body).kind,
        HirExprKind::StaticCall { class, method, .. } if class == &crate::semantic::ids::TypeId::local("Msg") && method == "Num"
    ));
}

// 79. generic enum construction carries full type args via expr_types
#[test]
fn p79_generic_enum_construction_via_expr_types() {
    let src = "fn f(b: bool) -> Result<i64, String> { \
                   if b { return Err(\"x\"); } return Ok(1); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let f = &hir.functions[0];
    let expected = Type::Generic("Result".into(), vec![Type::I64, Type::String]);
    assert_eq!(first_return(&f.body).ty, expected);
}

// 80. a bare fieldless `None` resolves to Option<T> via the checker tables
#[test]
fn p80_bare_none_via_checker_tables() {
    let src = "fn f() -> Option<i64> { return None; }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let ret = first_return(&hir.functions[0].body);
    assert_eq!(ret.ty, Type::Generic("Option".into(), vec![Type::I64]));
    assert!(matches!(
        &ret.kind,
        HirExprKind::StaticField { class, field } if class == &crate::semantic::ids::TypeId::local("Option") && field == "None"
    ));
}

// 81. an interface-typed receiver's method call types via expr_types
#[test]
fn p81_interface_method_via_expr_types() {
    let src = "interface Animal { fn speak(self) -> i64; } \
                   class Dog implements Animal { pub fn speak(self) -> i64 { return 7; } } \
                   fn f(a: Animal) -> i64 { return a.speak(); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .unwrap();
    assert_eq!(first_return(&f.body).ty, Type::I64);
}

// 82. `return Result::Ok();` in a `Result<void, E>` function types through
//     the checker table. The checker special-cases the zero-argument form
//     and used to return before recording anything, so lowering could not
//     type the call and reported a gap -- which dropped the WHOLE function
//     from the HIR, and with it from the LIR.
#[test]
fn p82_zero_arg_result_ok_types_via_checker_tables() {
    let src = "fn f() -> Result<void, String> { return Result::Ok(); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .unwrap();
    assert_eq!(
        first_return(&f.body).ty,
        Type::Generic("Result".into(), vec![Type::Void, Type::String])
    );
}

// 83. The same shape as the entry point, which is the case that mattered:
//     `main` has to survive into `HirProgram::functions` for the backend to
//     have any lowered IR to select.
#[test]
fn p83_result_void_main_reaches_the_hir() {
    let src = "fn check(n: i64) -> Result<i64, String> { return Result::Ok(n); } \
                   fn main() -> Result<void, String> { let v = check(21)?; println(v); \
                   return Result::Ok(); }";
    let (hir, diags) = lower_with_checker(src);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(
        hir.functions
            .iter()
            .any(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("main")),
        "main must not be dropped: {:?}",
        hir.functions.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
}

// 84. The argument-carrying `Result::Ok(v)` is the neighbouring case: it
//     goes through the ordinary `check_expr` path, so its type was always
//     recorded. The fix above must not have moved that.
#[test]
fn p84_result_ok_with_a_payload_still_types() {
    let (hir, diags) =
        lower_with_checker("fn f() -> Result<i64, String> { return Result::Ok(7); }");
    assert!(diags.is_empty(), "{diags:?}");
    let f = hir
        .functions
        .iter()
        .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
        .unwrap();
    assert_eq!(
        first_return(&f.body).ty,
        Type::Generic("Result".into(), vec![Type::I64, Type::String])
    );
}

// 78. without checker tables, both cases still degrade to diagnostics
#[test]
fn p78_structural_fallback_still_reports() {
    let (_, diags) = lower_src("fn f() { let d = |x| x; }");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unannotated lambda parameter")),
        "{diags:?}"
    );
    let (_, diags) = lower_src("enum Msg { Num(i64), } fn f() -> Msg { return Num(7); }");
    assert!(!diags.is_empty(), "unresolved construction must report");
}

// the unit-local class table records a module-qualified base
// by its full path, and `Classes::resolve` stops at a repeated class.

/// `(class, base, fields)` rows for a hand-built class table.
type TableRow<'a> = (&'a str, Option<&'a str>, &'a [(&'a str, Type)]);

fn table(entries: &[TableRow<'_>]) -> Classes {
    let mut classes = Classes::default();
    for (name, base, fields) in entries {
        classes.map.insert(
            (*name).to_string(),
            ClassInfo {
                fields: fields
                    .iter()
                    .map(|(f, ty)| ((*f).to_string(), ty.clone()))
                    .collect(),
                base: base.map(str::to_string),
                ..ClassInfo::default()
            },
        );
    }
    classes
}

// A qualified base is keyed as `shapes::Sized`, not `Sized`, so a
// same-named local class does not become its own base.
#[test]
fn class_table_qualified_base_keeps_its_module_in_the_class_table() {
    let tokens = Lexer::new(
        "class Sized extends shapes::Sized { depth: i64; }\n\
             class Block extends Sized { tag: i64; }",
    )
    .tokenize()
    .expect("lexing failed");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
    let bases: Vec<Option<String>> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Class(c) => Some(c.base_class.as_ref().map(Classes::base_key)),
            _ => None,
        })
        .collect();
    assert_eq!(
        bases,
        vec![Some("shapes::Sized".to_string()), Some("Sized".to_string())]
    );
}

// A base outside the table ends the walk with `None` (the checker tables
// take over), instead of looping.
#[test]
fn class_table_resolve_stops_at_a_base_missing_from_the_table() {
    let classes = table(&[
        ("Sized", Some("shapes::Sized"), &[("depth", Type::I64)]),
        ("Block", Some("Sized"), &[]),
    ]);
    assert_eq!(classes.field_type("Sized", "depth"), Some(Type::I64));
    assert_eq!(classes.field_type("Block", "depth"), Some(Type::I64));
    assert_eq!(classes.field_type("Block", "width"), None);
    assert_eq!(classes.field_type("Sized", "width"), None);
}

// The shape the bug produced: a class that is its own base. The walk
// must return, not spin.
#[test]
fn class_table_resolve_terminates_on_a_self_ring() {
    let classes = table(&[("Sized", Some("Sized"), &[("depth", Type::I64)])]);
    assert_eq!(classes.field_type("Sized", "depth"), Some(Type::I64));
    assert_eq!(classes.field_type("Sized", "width"), None);
    assert_eq!(classes.method_type("Sized", "area"), None);
}

// A two-class ring terminates too, and a member found on the ring is
// still returned.
#[test]
fn class_table_resolve_terminates_on_a_two_class_ring() {
    let classes = table(&[
        ("A", Some("B"), &[("a", Type::I64)]),
        ("B", Some("A"), &[("b", Type::Bool)]),
    ]);
    assert_eq!(classes.field_type("A", "b"), Some(Type::Bool));
    assert_eq!(classes.field_type("B", "a"), Some(Type::I64));
    assert_eq!(classes.field_type("A", "missing"), None);
    assert_eq!(classes.field_type("B", "missing"), None);
}

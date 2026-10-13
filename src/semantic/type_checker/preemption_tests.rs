//! A7: complete preemption summaries and conservative unresolved dispatch.
use super::TypeChecker;
use crate::{diagnostics::ErrorCode, lexer::Lexer, parser::Parser};

fn check(source: &str, native: bool, rejected: bool) {
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = TypeChecker::new().with_sync_stack_preemption(native);
    checker.check_program(&program);
    let preemption: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| d.code == ErrorCode::E0810)
        .collect();
    let other: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| d.code != ErrorCode::E0810)
        .collect();
    assert!(other.is_empty(), "{other:?}");
    assert_eq!(
        !preemption.is_empty(),
        rejected,
        "native={native}: {preemption:?}\n{source}"
    );
    if rejected && native {
        assert!(
            preemption
                .iter()
                .all(|d| d.message.contains("cannot prove preemptibility")),
            "{preemption:?}"
        );
    }
}

macro_rules! case {
    ($name:ident, $source:expr, $native:expr, $fallback:expr) => {
        #[test]
        fn $name() {
            check($source, true, $native);
            check($source, false, $fallback);
        }
    };
}

case!(
    a7_01_unresolved_interface,
    "interface I { fn run(self) -> i64; } async fn f(x: I) -> i64 { return x.run(); }",
    true,
    true
);
case!(
    a7_02_transitive_interface,
    "interface I { fn run(self) -> i64; } fn helper(x: I) -> i64 { return x.run(); } async fn f(x: I) -> i64 { return helper(x); }",
    true,
    true
);
case!(
    a7_03_indirect,
    "async fn f(g: fn(i64) -> i64) -> i64 { return g(1); }",
    true,
    true
);
case!(
    a7_04_transitive_indirect,
    "fn helper(g: fn(i64) -> i64) -> i64 { return g(1); } async fn f(g: fn(i64) -> i64) -> i64 { return helper(g); }",
    true,
    true
);
case!(
    a7_05_pure_helper,
    "fn helper() -> i64 { return 1; } async fn f() -> i64 { return helper(); }",
    false,
    false
);
case!(
    a7_06_loop_helper,
    "fn helper() -> i64 { let mut n = 0; while n < 3 { n = n + 1; } return n; } async fn f() -> i64 { return helper(); }",
    false,
    true
);
case!(
    a7_07_recursive_helper,
    "fn helper(n: i64) -> i64 { if n == 0 { return 1; } return helper(n - 1); } async fn f() -> i64 { return helper(3); }",
    false,
    true
);
case!(
    a7_08_unknown_dominates_loop,
    "fn helper(g: fn(i64) -> i64) -> i64 { let mut n = 0; while n < 3 { n = g(n); } return n; } async fn f(g: fn(i64) -> i64) -> i64 { return helper(g); }",
    true,
    true
);
case!(
    a7_09_sync_only_unknown,
    "interface I { fn run(self) -> i64; } fn f(x: I) -> i64 { return x.run(); }",
    false,
    false
);
case!(a7_10_builtin, "async fn f() { println(1); }", false, false);
case!(
    a7_11_implicit_constructor,
    "class C {} async fn f() { let c = new C(); }",
    false,
    false
);
case!(
    a7_12_explicit_constructor,
    "class C { pub init(self) {} } async fn f() { let c = new C(); }",
    false,
    false
);
case!(
    a7_13_constructor_unknown,
    "interface I { fn run(self) -> i64; } class C { pub init(self, x: I) { x.run(); } } async fn f(x: I) { let c = new C(x); }",
    true,
    true
);
case!(
    a7_14_enum_constructor,
    "enum E { One(i64), } async fn f() { let x = E::One(1); }",
    false,
    false
);
case!(
    a7_15_concrete_method,
    "class C { pub fn run(self) -> i64 { return 1; } } async fn f(x: C) -> i64 { return x.run(); }",
    false,
    false
);
case!(
    a7_16_method_unknown,
    "interface I { fn run(self) -> i64; } class C { pub fn run(self, x: I) -> i64 { return x.run(); } } async fn f(c: C, x: I) -> i64 { return c.run(x); }",
    true,
    true
);
case!(
    a7_17_static_unknown,
    "interface I { fn run(self) -> i64; } class C { pub static fn run(x: I) -> i64 { return x.run(); } } async fn f(x: I) -> i64 { return C::run(x); }",
    true,
    true
);
case!(
    a7_18_unused_unknown_helper,
    "interface I { fn run(self) -> i64; } fn helper(x: I) -> i64 { return x.run(); } async fn f() { println(1); }",
    false,
    false
);
case!(
    a7_19_nested_argument,
    "interface I { fn run(self) -> i64; } async fn f(x: I) { println(x.run()); }",
    true,
    true
);
case!(
    a7_20_async_boundary,
    "async fn child() -> i64 { let mut n = 0; while n < 3 { n = n + 1; } return n; } fn make() -> Task<i64> { return child(); } async fn f() { let t = make(); println(await t); }",
    false,
    false
);

case!(
    a7_21_constructor_loop,
    "class C { pub init(self) { let mut i = 0; while i < 2 { i = i + 1; } } } async fn f() { let c = new C(); }",
    false,
    true
);
case!(
    a7_22_known_implementation_does_not_close_interface,
    "interface I { fn run(self) -> i64; } class C implements I { pub fn run(self) -> i64 { return 1; } } async fn f(x: I) -> i64 { return x.run(); }",
    true,
    true
);
case!(
    a7_23_callback_adapter,
    "async fn f(xs: Option<i64>) { let ys = xs.map(|x: i64| -> i64 { return x + 1; }); }",
    true,
    true
);

case!(
    a7_24_std_alias_contract,
    "import std::fs as files; async fn f() { files::exists(\"unused\"); }",
    false,
    false
);

#[test]
fn a7_shared_dispatch_equations_do_not_multiply_with_call_sites() {
    for implementations in [4, 16, 64] {
        for calls in [1, 8, 32] {
            let mut source =
                "open class Base { pub open fn run(self) -> i64 { return 1; } }".to_owned();
            for i in 1..implementations {
                source.push_str(&format!(
                    "class C{i} extends Base {{ pub override fn run(self) -> i64 {{ return 1; }} }}"
                ));
            }
            source.push_str("async fn work(x: Base) -> i64 {");
            source.push_str(&"x.run();".repeat(calls));
            source.push_str("return 0; }");
            let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let mut checker = TypeChecker::new().with_sync_stack_preemption(true);
            checker.check_program(&program);
            assert_eq!(checker.errors.len(), calls, "{:?}", checker.errors);
            assert!(
                checker.errors.iter().all(|d| d.code == ErrorCode::E0810
                    && d.message.contains("cannot prove preemptibility"))
            );
            let unions: Vec<_> = checker
                .effect_inputs
                .edges
                .iter()
                .filter(|(id, _)| id.name().starts_with("$preemption$"))
                .collect();
            assert_eq!(unions.len(), 1);
            assert_eq!(unions[0].1.len(), implementations + 1);
            eprintln!(
                "a7-dispatch implementations={implementations} call_sites={calls} union_nodes=1 union_edges={} unresolved_edges=1",
                implementations + 1
            );
        }
    }
}

case!(
    a7_26_open_virtual_is_unproven,
    "open class Base { pub open fn run(self) -> i64 { return 0; } } async fn f(x: Base) -> i64 { return x.run(); }",
    true,
    true
);
case!(
    a7_27_inherited_open_virtual_is_unproven,
    "open class Base { pub open fn run(self) -> i64 { return 0; } } open class Mid extends Base {} async fn f(x: Mid) -> i64 { return x.run(); }",
    true,
    true
);
case!(
    a7_28_sealed_receiver_is_closed,
    "open class Base { pub open fn run(self) -> i64 { return 0; } } class Closed extends Base {} async fn f(x: Closed) -> i64 { return x.run(); }",
    false,
    false
);
case!(
    a7_29_nonoverridable_method_is_closed,
    "open class Base { pub fn run(self) -> i64 { return 0; } } async fn f(x: Base) -> i64 { return x.run(); }",
    false,
    false
);
case!(
    a7_30_final_override_closes_slot,
    "open class Base { pub open fn run(self) -> i64 { return 0; } } open class Mid extends Base { pub override fn run(self) -> i64 { return 1; } } async fn f(x: Mid) -> i64 { return x.run(); }",
    false,
    false
);
case!(
    a7_31_super_call_is_closed,
    "open class Base { pub open fn run(self) -> i64 { return 0; } } class Closed extends Base { pub fn direct(self) -> i64 { return super.run(); } } async fn f(x: Closed) -> i64 { return x.direct(); }",
    false,
    false
);
case!(
    a7_32_static_call_is_closed,
    "open class Base { pub static fn run() -> i64 { return 0; } } async fn f() -> i64 { return Base::run(); }",
    false,
    false
);

case!(
    a7_33_open_async_keeps_task_boundary,
    "open class Base { pub open async fn run(self) -> i64 { return 0; } } async fn f(x: Base) { x.run(); }",
    false,
    false
);

use super::{TypeChecker, check_source};
use crate::diagnostics::Span;
use crate::parser::ast::{CallArg, Expr, ExprId, MethodCallExpr, REBUILT_TYPE_NODES, Type};
use crate::semantic::builtin_types::BuiltinTypeId as B;

#[test]
fn nested_retired_wait_arguments_are_checked_once() {
    use crate::semantic::intrinsics::{METHOD_LOOKUPS, RECEIVER_LOOKUPS};
    check_source("");
    for depth in [0, 1, 4, 8, 32, 128] {
        let mut expr = String::from(r#""leaf".len(1)"#);
        let mut expected = vec!["len takes 0 argument(s) but 1 were supplied".to_string()];
        for level in 0..depth {
            let name = if level % 2 == 0 { "join" } else { "try_join" };
            expr = format!("x.{name}({expr})");
            expected.push(format!("no method `{name}` on type `Option<i64>`"));
        }
        METHOD_LOOKUPS.with(|count| count.set(0));
        RECEIVER_LOOKUPS.with(|count| count.set(0));
        let diagnostics = check_source(&format!("fn probe(x: Option<i64>) {{ {expr}; }}"));
        let classifications = METHOD_LOOKUPS.with(|count| count.get());
        assert_eq!(classifications, depth + 1);
        let receiver_lookups = RECEIVER_LOOKUPS.with(|count| count.get());
        assert_eq!(receiver_lookups, 2 * depth + 1);
        let messages: Vec<_> = diagnostics.iter().map(|d| &d.message).collect();
        assert_eq!(messages, expected.iter().collect::<Vec<_>>());
        println!(
            "wait_depth={depth} classifications={classifications} receiver_lookups={receiver_lookups} diagnostics={}",
            diagnostics.len()
        );
    }
}

#[test]
fn declared_join_checks_nested_argument_calls_once() {
    use crate::semantic::intrinsics::{METHOD_LOOKUPS, RECEIVER_LOOKUPS};
    check_source("");
    for wrapper in [
        "true ? EXPR + 1 : 0",
        "match b { true => EXPR, false => 0 }",
        "match b { true if guard.len() > 0 => EXPR, _ => 0 }",
    ] {
        for depth in [1, 4, 8, 32] {
            let mut expr = String::from(r#""leaf".len()"#);
            for _ in 0..depth {
                expr = format!("x.join({})", wrapper.replace("EXPR", &expr));
            }
            METHOD_LOOKUPS.with(|count| count.set(0));
            RECEIVER_LOOKUPS.with(|count| count.set(0));
            let diagnostics = check_source(&format!(
                "interface I {{ fn join(self, n: i64) -> i64; }} fn probe(x: I, b: bool, guard: String) -> i64 {{ return {expr}; }}"
            ));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let count = METHOD_LOOKUPS.with(|count| count.get());
            let expected = if wrapper.contains("guard") {
                2 * depth + 1
            } else {
                depth + 1
            };
            assert_eq!(
                RECEIVER_LOOKUPS.with(|c| c.get()),
                expected,
                "{wrapper} at depth {depth}"
            );
            assert_eq!(count, expected, "{wrapper} at depth {depth}");
        }
    }
}

#[test]
fn declared_join_checks_enum_payload_calls_once() {
    use crate::semantic::intrinsics::{METHOD_LOOKUPS, RECEIVER_LOOKUPS};
    check_source("");
    for (param, wrap) in [
        ("Option<i64>", "Option::Some(EXPR)"),
        ("Option<i64>", "Some(EXPR)"),
        ("Option<i64>", "b ? Option::Some(EXPR) : Option::Some(0)"),
        (
            "Option<i64>",
            "match b { true => Option::Some(EXPR), false => Option::Some(0) }",
        ),
        ("i64", "identity(EXPR)"),
        ("Option<i64>", "identity(Option::Some(EXPR))"),
        ("i64", "Utility::value(EXPR)"),
        ("BlockingCell<i64>", "BlockingCell::new(EXPR)"),
        ("Boxed<i64>", "new Boxed<i64>(EXPR)"),
        ("Result<i64,i64>", "b ? Ok(EXPR) : Err(0)"),
        ("fn() -> i64", "|| EXPR"),
        ("i64", "apply(|| EXPR)"),
    ] {
        for depth in [1, 4, 8, 32] {
            let mut value = r#""s".len()"#.to_owned();
            for _ in 0..depth {
                value = format!("x.join({})", wrap.replace("EXPR", &value));
            }
            let source = format!(
                "enum Option<T> {{Some(T),None}} enum Result<T,E> {{Ok(T),Err(E)}} fn identity<T>(v:T)->T {{return v;}}
                 fn apply<T>(f:fn()->T)->T {{return f();}}
                 class Utility {{pub static fn value(n:i64)->i64 {{return n;}}}}
                 class Boxed<T> {{pub item:T;}}
                 interface I {{fn join(self,n:{param})->i64;}}
                 fn probe(x:I,b:bool)->i64 {{return {value};}}"
            );
            METHOD_LOOKUPS.with(|c| c.set(0));
            RECEIVER_LOOKUPS.with(|c| c.set(0));
            let diagnostics = check_source(&source);
            let expected_errors = if wrap == "Some(EXPR)" { depth } else { 0 };
            assert_eq!(
                diagnostics.len(),
                expected_errors,
                "{wrap} at {depth}: {diagnostics:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d.message == "shorthand `Some` needs an expected `Option` type")
            );
            assert_eq!(
                RECEIVER_LOOKUPS.with(|c| c.get()),
                depth + 1,
                "{wrap} at {depth}"
            );
            assert_eq!(
                METHOD_LOOKUPS.with(|c| c.get()),
                depth + 1,
                "{wrap} at {depth}"
            );
        }
    }
}

#[test]
fn declared_join_preserves_each_failed_branch() {
    for (left, right, error) in [
        ("{ missing; }", "3", "cannot find variable `missing`"),
        (
            "12 ? 1 : 2",
            r#""s""#,
            "if-expression condition must be `bool`, found `i64`",
        ),
    ] {
        for (first, second) in [(left, right), (right, left)] {
            let diagnostics = check_source(&format!(
                "interface I {{fn join(self,n:i64);}} fn f(x:I,b:bool) {{
                 x.join(match b {{true => {first}, false => {second}}});}}"
            ));
            let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
            assert_eq!(messages, [error]);
        }
    }
}

#[test]
fn declared_join_refines_match_to_interface_without_rechecking_arms() {
    use crate::semantic::intrinsics::{METHOD_LOOKUPS, RECEIVER_LOOKUPS};
    check_source("");
    for depth in [1, 4, 8, 32] {
        let mut value = String::from("leaf");
        for _ in 0..depth {
            value = format!("x.join(match b {{ true if s.len() > 0 => {value}, _ => leaf }})");
        }
        let source = format!(
            "interface Tag {{ fn value(self)->i64; }}
            class Leaf implements Tag {{pub fn value(self)->i64 {{return 1;}}}}
            interface I {{fn join(self, arg:Tag)->Leaf;}}
            fn probe(x:I,b:bool,s:String,leaf:Leaf)->Leaf {{return {value};}}"
        );
        METHOD_LOOKUPS.with(|c| c.set(0));
        RECEIVER_LOOKUPS.with(|c| c.set(0));
        let diagnostics = check_source(&source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(METHOD_LOOKUPS.with(|c| c.get()), depth * 2);
        assert_eq!(RECEIVER_LOOKUPS.with(|c| c.get()), depth * 2);
    }
}

#[test]
fn declared_join_keeps_independent_match_errors_before_arity() {
    for (arg, first) in [
        (
            r#"match b { true if 12 => "s", _ => "t" }"#,
            "match guard must be `bool`, found `i64`",
        ),
        (
            "match b { true => { missing; }, false => {} }",
            "cannot find variable `missing`",
        ),
    ] {
        let diagnostics = check_source(&format!(
            "interface I {{fn join(self, n:i64);}}
            fn probe(x:I,b:bool) {{x.join({arg}, surplus);}}"
        ));
        let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        // The public diagnostic renderer coalesces identical recovery messages.
        assert_eq!(
            messages,
            [
                first,
                "cannot find variable `surplus`",
                "method `join` takes 1 argument(s) but 2 were supplied"
            ]
        );
    }
}

#[test]
fn builtin_method_diagnostic_receivers_and_names() {
    // Six receiver families and four bad names: 24 explicit diagnostic cases.
    for (receiver, ty, available) in [
        ("s", "String", "substring"),
        ("n", "i64", "wrapping_add"),
        ("f", "f64", "to_string"),
        ("b", "bool", "to_string"),
        ("a", "Array<i64>", "push"),
        ("m", "Map<String, i64>", "insert"),
    ] {
        for method in ["missing", "toStrng", "toF64", "charAt"] {
            let source = format!(
                "import std::collections::{{Array, Map}}; fn test(s: String, n: i64, f: f64, b: bool, a: Array<i64>, m: Map<String, i64>) {{ {receiver}.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            let d = &diagnostics[0];
            assert_eq!(d.message, format!("no method `{method}` on type `{ty}`"));
            assert!(d.helps.iter().any(|h| h.contains(available)), "{d:?}");
            assert_eq!(
                d.labels.len(),
                1,
                "stored receivers have no call-origin label"
            );
        }
    }
}

#[test]
fn builtin_method_diagnostic_call_origins() {
    for (prefix, receiver, ty) in [
        ("", "\"abc\".len()", "i64"),
        ("", "\"abc\".find(\"b\")", "Option<i64>"),
        ("", "\"abc\".trim()", "String"),
        ("", "\"abc\".contains(\"b\")", "bool"),
        ("fn value() -> f64 { return 1.0; }", "value()", "f64"),
        (
            "fn values() -> Array<i64> { return [1]; }",
            "values()",
            "Array<i64>",
        ),
        ("", "Map<String, i64>::new()", "Map<String, i64>"),
    ] {
        let source = format!(
            "import std::collections::{{Array, Map}}; {prefix} fn main() {{ {receiver}.missing(); }}"
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
        let label = diagnostics[0]
            .labels
            .iter()
            .find(|l| l.message.contains("this call returns"))
            .expect("call origin");
        assert_eq!(
            label.message,
            format!("this call returns `{ty}`, the receiver type")
        );
        let start = source.rfind(receiver).unwrap();
        assert!(label.span.start >= start);
        assert!(label.span.end <= start + receiver.len());
        assert!(receiver.contains(&source[label.span.start..label.span.end]));
        assert!(label.span.start < label.span.end);
    }
}

#[test]
fn builtin_method_diagnostic_output_scales_linearly() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "fn main() {{ {} }}",
            "\"abc\".len().unwrap_or(0);".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count);
        assert_eq!(
            diagnostics.iter().map(|d| d.labels.len()).sum::<usize>(),
            2 * count
        );
        assert_eq!(
            diagnostics.iter().map(|d| d.helps.len()).sum::<usize>(),
            count
        );
    }
}

#[test]
fn generic_parameter_method_diagnostics_declaration_shapes() {
    // Five receiver shapes by four names exercise twenty independent cases.
    for receiver in ["x", "xs.pop()", "identity(x)", "xs[0]", "(x)"] {
        for method in ["unwrap", "missing", "to_string", "len"] {
            let source = format!(
                "import std::collections::Array; fn identity<T>(x: T) -> T {{ return x; }} fn bad<T>(x: T, xs: Array<T>) {{ {receiver}.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                format!("type parameter `T` has no method `{method}`")
            );
            if receiver == "xs.pop()" || receiver == "identity(x)" {
                assert!(
                    diagnostics[0]
                        .labels
                        .iter()
                        .any(|l| l.message == "this call returns `T`, the receiver type")
                );
            }
        }
    }
}

#[test]
fn generic_parameter_method_diagnostics_recover_and_scale() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "fn bad<T>(x: T) {{ {} }}",
            "x.missing().again();".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message == "type parameter `T` has no method `missing`")
        );
    }
    let diagnostics = check_source("fn bad<T>(x: T) { x.missing(unknown); }");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.message.contains("unknown")));
}

#[test]
fn generic_parameter_method_diagnostics_specialization_counts() {
    use super::TypeChecker;
    use crate::{lexer::Lexer, parser::Parser};
    for count in [1, 8, 64] {
        let mut source = String::from("fn bad<T>(x: T) { x.missing(); } ");
        for i in 0..count {
            source.push_str(&format!("class Value{i} {{}} "));
        }
        source.push_str("fn main() {");
        for i in 0..count {
            source.push_str(&format!("bad(new Value{i}());"));
        }
        source.push('}');
        let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        let mut checked = 0;
        while let Some(instance) = checker.generics.pending.pop_front() {
            checker.check_generic_instance(instance);
            checked += 1;
        }
        assert_eq!(checked, count);
        assert_eq!(checker.generics.rejected_parameter_methods.len(), 1);
        assert_eq!(checker.errors.len(), 1, "{:?}", checker.errors);
        assert_eq!(
            checker.errors[0].message,
            "type parameter `T` has no method `missing`"
        );
    }
}

fn method_call(spelling: &str) -> MethodCallExpr {
    let span = Span::dummy();
    MethodCallExpr {
        id: ExprId::fresh(),
        object: Expr::Var("receiver".into(), span, ExprId::fresh()),
        method: spelling.to_owned().into(),
        args: Vec::new(),
        span,
    }
}

fn nested_type(depth: usize) -> Type {
    (0..depth).fold(Type::I64, |ty, _| Type::Array(Box::new(ty)))
}

fn rebuilt_nodes(mut operation: impl FnMut(), calls: usize) -> usize {
    REBUILT_TYPE_NODES.with(|count| count.set(0));
    for _ in 0..calls {
        operation();
    }
    REBUILT_TYPE_NODES.with(|count| count.get())
}

#[test]
fn dispatch_copies_only_payloads_owned_by_the_result() {
    let mut checker = TypeChecker::new();
    for depth in [0, 8, 32, 128] {
        let item = nested_type(depth);
        let channel = B::Channel.apply(vec![item.clone()]);
        let option = B::Option.apply(vec![item.clone()]);
        let result = B::Result.apply(vec![item.clone(), item.clone()]);
        let map = B::Map.apply(vec![Type::I64, item.clone()]);
        let frozen = B::FrozenMap.apply(vec![Type::I64, item.clone()]);
        let values = Type::Array(Box::new(item.clone()));
        let item_nodes = depth + 1;
        assert_eq!(
            rebuilt_nodes(|| drop(result.clone()), 1),
            2 * item_nodes + 1
        );

        for (receiver, method, expected, per_call) in [
            (&result, "is_ok", &Type::Bool, 0),
            (&result, "is_err", &Type::Bool, 0),
            (&option, "is_some", &Type::Bool, 0),
            (&option, "is_none", &Type::Bool, 0),
            (&map, "len", &Type::I64, 0),
            (&frozen, "len", &Type::I64, 0),
            (&option, "unwrap", &item, item_nodes),
            (&result, "unwrap_err", &item, item_nodes),
            (&channel, "recv", &item, item_nodes),
            (&channel, "recv_opt", &option, item_nodes),
            (&map, "values", &values, item_nodes),
            (&map, "freeze", &frozen, item_nodes + 1),
        ] {
            let call = method_call(method);
            for calls in [1, 8, 64] {
                let copies = rebuilt_nodes(
                    || {
                        assert_eq!(
                            &checker.check_receiver_method_call(receiver, &call),
                            expected
                        );
                    },
                    calls,
                );
                assert_eq!(
                    copies,
                    calls * per_call,
                    "{method}, depth {depth}, calls {calls}"
                );
            }
        }
        for receiver in [&map, &frozen] {
            let keyed_call = |method| {
                let mut call = method_call(method);
                call.args
                    .push(CallArg::value(Expr::Integer(0, call.span, ExprId::fresh())));
                call
            };
            let get = keyed_call("get");
            let contains = keyed_call("contains");
            for calls in [1, 8, 64] {
                let keys = rebuilt_nodes(
                    || {
                        assert_eq!(
                            checker.check_receiver_method_call(receiver, &contains),
                            Type::Bool
                        );
                    },
                    calls,
                );
                let reads = rebuilt_nodes(
                    || {
                        assert_eq!(checker.check_receiver_method_call(receiver, &get), option);
                    },
                    calls,
                );
                assert_eq!(
                    reads,
                    keys + calls * item_nodes,
                    "depth {depth}, calls {calls}"
                );
            }
        }
    }
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn channel_send_copies_the_element_only_for_async_retention() {
    let mut checker = TypeChecker::new();
    checker.symbols.push_scope();
    let mut call = method_call("send");
    call.args.push(CallArg::value(Expr::Var(
        "value".into(),
        call.span,
        ExprId::fresh(),
    )));
    for depth in [0, 8, 32, 128] {
        let item = nested_type(depth);
        let channel = B::Channel.apply(vec![item.clone()]);
        checker.symbols.define_var(
            "value".into(),
            crate::semantic::symbols::VarInfo {
                ty: item.clone(),
                mutable: false,
                is_param: true,
                declaration_span: call.span,
            },
        );
        for calls in [1, 8, 64] {
            checker.local.current_async_context = false;
            let checking_value = rebuilt_nodes(
                || {
                    let got = checker.check_expr_expecting(&call.args[0].expr, &item);
                    assert_eq!(got, item);
                    assert!(checker.types_compatible(&item, &got));
                },
                calls,
            );
            for asynchronous in [false, true] {
                checker.local.current_async_context = asynchronous;
                let before = checker.local.async_local_types.len();
                let send = rebuilt_nodes(
                    || {
                        assert_eq!(
                            checker.check_receiver_method_call(&channel, &call),
                            Type::Void
                        );
                    },
                    calls,
                );
                let retained = checker.local.async_local_types.len() - before;
                assert_eq!(retained, if asynchronous { calls } else { 0 });
                assert_eq!(
                    send,
                    checking_value + retained * (depth + 1),
                    "depth {depth}, calls {calls}"
                );
            }
        }
    }
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn array_method_dispatch_adds_no_element_copies_to_specialization() {
    let mut checker = TypeChecker::new();
    let (program, errors) =
        crate::parser::Parser::new(crate::lexer::Lexer::new("").tokenize().unwrap()).parse();
    assert!(errors.is_empty());
    checker.check_program(&program);
    let call = method_call("len");
    let template = "$std::collections::Array";

    for depth in [0, 2, 6, 12] {
        let item = nested_type(depth);
        let args = std::slice::from_ref(&item);
        let class = checker
            .instantiate_class(template, args, call.span)
            .unwrap();
        let Type::Named(owner) = &class else {
            panic!("expected a class")
        };
        let owner = super::TypeId::from_source_name(owner);
        for receiver in [
            Type::Array(Box::new(item.clone())),
            Type::FixedArray(Box::new(item.clone()), 3),
        ] {
            assert_eq!(
                checker.check_receiver_method_call(&receiver, &call),
                Type::I64
            );
            assert_eq!(checker.static_call_classes[&call.id], owner);
            for calls in [1, 8, 64] {
                // Both paths instantiate the same previously registered class and check
                // its declaration. Only the array dispatch has a receiver to normalize.
                let direct = rebuilt_nodes(
                    || {
                        let class = checker
                            .instantiate_class(template, args, call.span)
                            .unwrap();
                        assert_eq!(
                            checker.resolve_method(
                                &class,
                                &call,
                                &call.method,
                                false,
                                Some((call.method.kind(), None))
                            ),
                            Type::I64
                        );
                    },
                    calls,
                );
                let dispatch = rebuilt_nodes(
                    || {
                        assert_eq!(
                            checker.check_receiver_method_call(&receiver, &call),
                            Type::I64
                        );
                    },
                    calls,
                );
                assert_eq!(dispatch, direct, "depth {depth}, calls {calls}");
                println!(
                    "fixed={} depth={depth} calls={calls} dispatch={dispatch} direct={direct}",
                    matches!(receiver, Type::FixedArray(..))
                );
            }
        }
    }
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn option_method_diagnostics_unknown_payloads_and_suggestions() {
    // Cover every Option candidate, an equal-distance tie, and Result-only names.
    for payload in ["i64", "String", "bool", "Node", "T", "Box<i64>"] {
        for (method, suggestion) in [
            ("ok_or", None),
            ("is_err", None),
            ("map_err", None),
            ("unwra", Some("unwrap")),
            ("expec", Some("expect")),
            ("unwrap_o", Some("unwrap_or")),
            ("is_sone", Some("is_some")),
            ("is_non", Some("is_none")),
            ("mapp", Some("map")),
            ("or_els", Some("or_else")),
            ("is_som", Some("is_some")),
            ("and_thn", Some("and_then")),
        ] {
            let source = format!(
                "class Node {{}} class Box<T> {{}} fn bad<T>(x: Option<{payload}>) {{ x.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            let d = &diagnostics[0];
            assert_eq!(
                d.message,
                format!("no method `{method}` on type `Option<{payload}>`")
            );
            assert!(!format!("{d:?}").contains("$type"));
            assert!(!format!("{d:?}").contains("handling absence"));
            let expected_help =
                suggestion.map(|name| format!("there is a method with a similar name: `{name}`"));
            assert_eq!(d.helps.as_slice(), expected_help.as_slice(), "{source}");
        }
    }
}

#[test]
fn option_payload_probe_borrows_map_value_types() {
    use crate::semantic::intrinsics::MethodName;

    for depth in [0, 8, 32, 128] {
        let item = nested_type(depth);
        let map = B::Map.apply(vec![Type::I64, item]);
        let option = B::Option.apply(vec![map]);
        let inner = crate::semantic::builtin_types::unary_arg(&option, B::Option).unwrap();
        let get = MethodName::classify("get");
        let check_calls = |spelling, count| {
            let mut checker = TypeChecker::new();
            let mut call = method_call(spelling);
            call.args
                .push(CallArg::value(Expr::Integer(1, call.span, ExprId::fresh())));
            let copies = rebuilt_nodes(
                || {
                    checker.check_receiver_method_call(&option, &call);
                },
                count,
            );
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|d| d.message.contains("without handling absence"))
            );
            copies
        };
        for count in [1, 8, 64] {
            let copies = rebuilt_nodes(|| assert!(get.has_method_on(inner)), count);
            assert_eq!(copies, 0, "depth {depth}, calls {count}");
            // The two calls check the same key and format the same receiver.
            // Only get would copy the nested value if it built a signature.
            assert_eq!(
                check_calls("get", count),
                check_calls("contains", count),
                "depth {depth}, calls {count}"
            );
        }
    }
}

#[test]
fn option_method_diagnostics_existing_payload_methods() {
    for (prefix, payload, method, args) in [
        ("", "String", "len", ""),
        (
            "import std::collections::Map;",
            "Map<i64, String>",
            "get",
            "1",
        ),
        ("", "String", "substring", "0, 1"),
        ("", "String", "substring", ""),
        ("", "i64", "checked_add", "1"),
        ("", "i64", "wrapping_neg", ""),
        ("", "f64", "to_string", ""),
        ("", "bool", "to_string", ""),
        (
            "class Node { pub fn get(self) -> i64 { return 1; } }",
            "Node",
            "get",
            "",
        ),
        (
            "open class Base { pub fn get(self) -> i64 { return 1; } } class Node extends Base {}",
            "Node",
            "get",
            "",
        ),
        (
            "interface Value { fn get(self) -> i64; }",
            "Value",
            "get",
            "",
        ),
        (
            "interface Value<T> { fn get(self) -> T; }",
            "Value<i64>",
            "get",
            "",
        ),
        ("", "Result<i64, String>", "unwrap_err", ""),
        (
            "class Box<T> { pub fn get(self) -> i64 { return 1; } }",
            "Box<i64>",
            "get",
            "",
        ),
        (
            "class Box<T> { pub fn get(self) -> i64 { return 1; } }",
            "Box<String>",
            "get",
            "",
        ),
    ] {
        let source = format!("{prefix} fn bad(x: Option<{payload}>) {{ x.{method}({args}); }}");
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            format!(
                "cannot call method `{method}` on `Option<{payload}>` without handling absence"
            )
        );
    }
}

#[test]
fn option_method_diagnostics_recovery_and_linear_counts() {
    use super::diagnostics::EDIT_CELLS;
    let mut unit_cells = 0;
    for count in [1, 8, 64, 256] {
        EDIT_CELLS.with(|cells| cells.set(0));
        let source = format!(
            "fn bad(x: Option<i64>) {{ {} }}",
            "x.ok_or().again();".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count, "{diagnostics:?}");
        let cells = EDIT_CELLS.with(|cells| cells.get());
        if count == 1 {
            unit_cells = cells;
        }
        assert!(unit_cells > 0);
        assert_eq!(cells, unit_cells * count);
        println!(
            "Option calls={count}: diagnostics={}, edit cells={cells}",
            diagnostics.len()
        );
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message == "no method `ok_or` on type `Option<i64>`")
        );
    }
    let diagnostics = check_source("fn bad(x: Option<i64>) { x.ok_or(unknown); }");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.message.contains("unknown")));
}

#[test]
fn option_method_diagnostics_call_origin_and_nested_option() {
    let diagnostics = check_source(
        "fn make(x: Option<i64>) -> Option<i64> { return x; } fn bad(x: Option<i64>) { make(x).ok_or(); }",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]
            .labels
            .iter()
            .any(|l| l.message == "this call returns `Option<i64>`, the receiver type")
    );
    let diagnostics = check_source("fn bad(x: Option<Option<String>>) { x.len(); }");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "no method `len` on type `Option<Option<String>>`"
    );
}

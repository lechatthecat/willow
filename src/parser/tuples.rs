//! Tuples reuse the checked, GC-traced single-variant aggregate representation.
//! The `$` prefix cannot be spelled by a source identifier. Arity declarations
//! are shared per program, and their identity is independent of module names.
use super::ast::*;
use crate::diagnostics::Span;

pub(crate) const VARIANT: &str = "$tuple";

pub(crate) fn name(arity: usize) -> String {
    format!("$Tuple{arity}")
}

pub(crate) fn is_tuple(name: &str) -> bool {
    name.strip_prefix("$Tuple")
        .is_some_and(|arity| !arity.is_empty() && arity.bytes().all(|b| b.is_ascii_digit()))
}

pub(super) fn declaration(arity: usize) -> Item {
    let span = Span::new(0, 0, 1, 1);
    let type_params: Vec<_> = (0..arity).map(|i| format!("$T{i}")).collect();
    Item::Enum(EnumDecl {
        name: name(arity),
        public: true,
        variants: vec![EnumVariant {
            name: VARIANT.into(),
            payload: type_params.iter().cloned().map(Type::Named).collect(),
            span,
        }],
        type_params,
        span,
    })
}

#[cfg(test)]
mod tests {
    use crate::{lexer::Lexer, parser::Parser, semantic::TypeChecker};
    fn errors(source: &str) -> Vec<crate::diagnostics::Diagnostic> {
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        if !errors.is_empty() {
            return errors;
        }
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        checker
            .errors
            .into_iter()
            .filter(|e| e.severity == crate::diagnostics::Severity::Error)
            .collect()
    }

    #[test]
    fn tuple_twenty_four_type_and_pattern_perspectives() {
        let accepted = [
            "fn pair() -> (i64, i64) { return (1, 2); } fn main() {}",
            "fn main() { let p: (i64, bool) = (1, true); }",
            "fn main() { let p = (1,); match p { (a,) => { println(a); } } }",
            "fn main() { let p: (String, f64, bool) = (\"a\", 1.5, true); }",
            "fn main() { let p: ((i64, bool), String) = ((1, true), \"a\"); }",
            "fn main() { let (a, b) = (1, 2); println(a + b); }",
            "fn main() { let (a, b): (i64, bool) = (1, true); println(b); }",
            "fn main() { let (_, b) = (1, 2); println(b); }",
            "fn main() { let (a, b) = (1, 2); let (c, d) = (a, b); println(c + d); }",
            "fn main() { let p: (i64, bool,) = (1, true,); }",
            "fn main() { let p: (Option<i64>, bool) = (None, true); }",
            "fn main() { let p = (1, true); println(match p { (a, b) if b => a, _ => 0 }); }",
            "fn sum(p: (i64, i64)) -> i64 { let (a, b) = p; return a + b; } fn main() {}",
            "fn main() { let p: (i64,) = (1,); }",
            "fn main() { let x: (i64) = (1); }",
            "fn main() { let p = ((1, 2), true); let (inner, flag) = p; let (a, b) = inner; println(a); }",
        ];
        for source in accepted {
            assert!(errors(source).is_empty(), "{source}: {:?}", errors(source));
        }
        let rejected = [
            "fn main() { let p: (i64, bool) = (true, 1); }",
            "fn main() { let p: (i64, bool) = (1, true, 3); }",
            "fn main() { let p: (i64,) = 1; }",
            "fn main() { let (a, b) = (1, true, 3); }",
            "fn main() { let (a, b) = 1; }",
            "fn main() { let p = (missing, true); }",
            "fn main() { let p = (1, true); match p { (a, b) if b => {} } }",
            "fn main() { let p: (Unknown, i64) = (1, 2); }",
            "fn main() { let (a, a) = (1, 2); }",
            "fn main() { let (a, b) = (1, 2); a = 3; }",
            "fn main() { let (a, b) = (1, 2); } fn other() { println(a); }",
            "fn main() { let p = (1, 2); match p { (a, b) => {} } println(a); }",
        ];
        for source in rejected {
            assert!(!errors(source).is_empty(), "{source}");
        }
    }
    #[test]
    fn tuple_parser_scaling_counts() {
        use crate::parser::{
            PARSER_TOKEN_READS,
            iter::{AstEvent, AstWalk},
        };
        for shape in ["wide", "repeated", "nested"] {
            let mut samples = Vec::new();
            for n in [8, 16, 32, 64] {
                let source = match shape {
                    "wide" => format!("fn main() {{ let p = ({}); }}", vec!["1"; n].join(",")),
                    "repeated" => format!("fn main() {{ {} }}", "let (a, b) = (1, 2);".repeat(n)),
                    _ => format!(
                        "fn main() {{ let p = {}1{}; }}",
                        "(".repeat(n),
                        ",)".repeat(n)
                    ),
                };
                let tokens = Lexer::new(&source).tokenize().unwrap();
                PARSER_TOKEN_READS.with(|reads| reads.set(0));
                let (program, diagnostics) = Parser::new(tokens).parse();
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
                let reads = PARSER_TOKEN_READS.with(|reads| reads.get());
                let super::Item::Function(function) = &program.items[0] else {
                    unreachable!()
                };
                let nodes = AstWalk::new(AstEvent::Block(&function.body)).count();
                samples.push((n, reads, nodes));
            }
            for col in 0..2 {
                let value = |s: &(usize, usize, usize)| if col == 0 { s.1 } else { s.2 };
                let slope = value(&samples[1]) - value(&samples[0]);
                for pair in samples.windows(2) {
                    assert_eq!(
                        value(&pair[1]) - value(&pair[0]),
                        slope * (pair[1].0 - pair[0].0) / 8,
                        "{shape}"
                    );
                }
            }
            eprintln!("tuple parser {shape}: {samples:?}");
        }
    }
    #[test]
    fn tuple_diagnostics_name_the_failing_element_and_recover() {
        let diagnostics = errors("fn main() { let p: (i64, bool) = (1, 2); }");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "tuple element expects `bool`, found `i64`"),
            "{diagnostics:?}"
        );
        let (program, diagnostics) = Parser::new(
            Lexer::new("fn main() { let (a, a) = (1, 2); println(3); } fn after() {}")
                .tokenize()
                .unwrap(),
        )
        .parse();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            program
                .items
                .iter()
                .any(|item| matches!(item, super::Item::Function(f) if f.name == "after"))
        );
        let super::Item::Function(main) = &program.items[0] else {
            unreachable!()
        };
        assert_eq!(main.body.stmts.len(), 1);
    }
    #[test]
    fn tuple_let_lookup_probes_scale_linearly() {
        use crate::ir::lower::{CheckerTables, lower_program_with};
        use crate::semantic::scopes::LOOKUP_PROBES;
        for lambda in [false, true] {
            let mut samples = Vec::new();
            for count in [8, 16, 32, 64, 128] {
                let body = format!(
                    "let seed = 1; {} println(seed);",
                    "let (a, b) = (seed, 2);".repeat(count)
                );
                let source = if lambda {
                    format!("fn main() {{ let f = || {{ {body} }}; f(); }}")
                } else {
                    format!("fn main() {{ {body} }}")
                };
                let (program, diagnostics) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
                let mut checker = TypeChecker::new();
                crate::register_prelude(&mut checker).unwrap();
                LOOKUP_PROBES.with(|n| n.set(0));
                checker.check_program(&program);
                let checking = LOOKUP_PROBES.with(|n| n.get());
                assert!(
                    !checker
                        .errors
                        .iter()
                        .any(|e| e.severity == crate::diagnostics::Severity::Error),
                    "{:?}",
                    checker.errors
                );
                LOOKUP_PROBES.with(|n| n.set(0));
                let (_, diagnostics) =
                    lower_program_with(&program, &CheckerTables::from_checker(&checker));
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
                let lowering = LOOKUP_PROBES.with(|n| n.get());
                samples.push((count, checking, lowering));
            }
            for column in [1, 2] {
                let get =
                    |sample: &(usize, usize, usize)| if column == 1 { sample.1 } else { sample.2 };
                let slope = get(&samples[1]) - get(&samples[0]);
                for pair in samples.windows(2) {
                    assert_eq!(
                        get(&pair[1]) - get(&pair[0]),
                        slope * (pair[1].0 - pair[0].0) / 8,
                        "lambda={lambda} {samples:?}"
                    );
                }
            }
            eprintln!("tuple lookup lambda={lambda} (N, checker probes, HIR probes): {samples:?}");
        }
    }

    #[test]
    fn tuple_let_preserves_source_recovery_depth() {
        for prefix in [
            "let (x,) = (1,);",
            "let (x,) = (1,); let (y,) = (x,);",
            "let (x,): (i64,) = (1,);",
        ] {
            for function in ["fn", "async fn"] {
                let source = format!(
                    "{function} value() -> i64 {{ {prefix} defer match recover() {{ Some(_) => {{}}, None => {{}} }} panic(\"boom\"); }} fn main() {{}}"
                );
                let diagnostics = errors(&source);
                assert!(
                    diagnostics
                        .iter()
                        .any(|d| d.code == crate::diagnostics::ErrorCode::E0905
                            && d.message.contains("outermost recovery scope")),
                    "{source}: {diagnostics:?}"
                );
                let source = format!(
                    "{function} value() -> i64 {{ if true {{ {prefix} defer match recover() {{ Some(_) => {{}}, None => {{}} }} panic(\"boom\"); }} return 9; }} fn main() {{}}"
                );
                assert!(
                    errors(&source).is_empty(),
                    "{source}: {:?}",
                    errors(&source)
                );
            }
        }
    }

    #[test]
    fn tuple_let_recovery_source_survives_syntax_roundtrip() {
        let source = "fn value() -> i64 { let (x,) = (1,); defer match recover() { Some(_) => {}, None => {} } panic(\"boom\"); } fn main() {}";
        let (program, diagnostics) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(diagnostics.is_empty());
        let wire = serde_json::to_value(&program).unwrap();
        let decoded: super::Program = serde_json::from_value(wire).unwrap();
        for program in [program.clone(), decoded] {
            let mut checker = TypeChecker::new();
            crate::register_prelude(&mut checker).unwrap();
            checker.check_program(&program);
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|d| d.code == crate::diagnostics::ErrorCode::E0905
                        && d.message.contains("outermost recovery scope")),
                "{:?}",
                checker.errors
            );
        }
    }
    fn check_on_small_stack(source: String) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                // Include parsing, checking and dropping both trees. Merely
                // constructing an AST would miss a synchronous checker edge.
                let (program, diagnostics) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
                let mut checker = TypeChecker::new();
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                drop(checker);
                drop(program);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn tuple_consecutive_lets_use_256_kib_stack() {
        for depth in [128, 512, 2048] {
            check_on_small_stack(format!(
                "fn main() {{ let seed = 1; {} }}",
                "let (a, b) = (seed, 2);".repeat(depth)
            ));
        }
    }

    #[test]
    fn tuple_checker_ordinary_nested_blocks_use_256_kib_stack() {
        for depth in [128, 512, 2048] {
            check_on_small_stack(format!(
                "fn main() {{ {} let leaf = 1; {} }}",
                "if true {".repeat(depth),
                "}".repeat(depth)
            ));
        }
    }

    #[test]
    fn tuple_nested_binding_initializers_use_256_kib_stack() {
        for depth in [128, 512, 2048] {
            // Each initializer contains a match arm block with another tuple
            // binding. Unlike a tuple literal chain, this crosses the new
            // parse_tuple_binding helper at every level. Arm blocks yield void,
            // keeping element type width fixed rather than testing type growth.
            check_on_small_stack(format!(
                "fn main() {{ {} let (leaf,) = (1,); {} }}",
                "let (x,) = (match true { _ => {".repeat(depth),
                "} },);".repeat(depth)
            ));
        }
    }
}

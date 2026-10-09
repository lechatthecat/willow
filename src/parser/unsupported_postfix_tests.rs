use super::*;
use crate::lexer::Lexer;

fn errors(source: &str) -> Vec<Diagnostic> {
    let tokens = Lexer::new(source).tokenize().unwrap();
    Parser::new(tokens).parse().1
}

#[test]
fn unsupported_postfix_call_perspectives() {
    for expression in [
        "pick(0)(5)",
        "pick()(5)",
        "pick(0)()",
        "(pick(0))(5)",
        "obj.pick(0)(5)",
        "Factory::pick(0)(5)",
        "pick(0)(5)(6)",
        "1 + pick(0)(5)",
        "outer(pick(0)(5))",
        "[pick(0)(5)]",
    ] {
        let source = format!("fn main() {{ println({expression}); }}");
        let diagnostics = errors(&source);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.message, "calling a call result is not supported",
            "{source}: {diagnostics:?}"
        );
        assert!(
            diagnostic
                .helps
                .iter()
                .any(|s| s.contains("result of `pick`"))
        );
        let span = diagnostic.labels[0].span;
        assert_eq!(&source[span.start as usize..span.end as usize], "(");
    }
}

#[test]
fn unsupported_postfix_cast_perspectives() {
    for statement in [
        "println(x as f64);",
        "let y = x as i64;",
        "return x as f64;",
        "x as f64;",
        "println((x as f64));",
        "let y = [x as f64];",
        "let y = a[x as i64];",
        "if x as bool {}",
        "while x as bool {}",
        "let y = yes ? x as f64 : z;",
        "let y = yes ? z : x as f64;",
        "lock m.get(x as f64) as v {}",
        "lock (x as f64) as v {}",
    ] {
        let source = format!("fn main() {{ {statement} }}");
        let diagnostics = errors(&source);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.message, "`as` casts are not supported",
            "{source}: {diagnostics:?}"
        );
        assert!(diagnostic.helps.iter().any(|s| s.contains("f64 literal")));
        let span = diagnostic.labels[0].span;
        assert_eq!(&source[span.start as usize..span.end as usize], "as");
    }
}

#[test]
fn unsupported_postfix_preserves_supported_syntax() {
    for source in [
        "fn main() { let f = pick(0); f(5); }",
        "fn main() { println(pick(0)); }",
        "fn main() { obj.pick(0).run(5); }",
        "fn main() { let x = pick(0)[5]; }",
        "fn main() { lock m as v {} }",
        "fn main() { lock read m as v {} }",
        "fn main() { lock write m as mut v {} }",
        "fn main() { lock m.get() as v {} }",
        "fn main() { lock read as v {} }",
        "import std::math as math; fn main() {}",
    ] {
        assert!(errors(source).is_empty(), "{source}: {:?}", errors(source));
    }
}

#[test]
fn unsupported_postfix_preserves_missing_parenthesis() {
    assert_eq!(errors("fn main() { println(x; }")[0].code, ErrorCode::E0104);
}

#[test]
fn unsupported_postfix_token_reads_scale_linearly() {
    for statement in [
        "println(pick(0)(5));",
        "println(x as f64);",
        "let f = pick(0); f(5);",
    ] {
        let mut samples = Vec::new();
        for count in [8, 16, 32, 64] {
            let source = format!("fn main() {{ {} }}", statement.repeat(count));
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let diagnostics = errors(&source);
            assert_eq!(
                diagnostics.len(),
                if statement.starts_with("let") {
                    0
                } else {
                    count
                }
            );
            samples.push((count, PARSER_TOKEN_READS.with(|v| v.get())));
        }
        let slope = (samples[1].1 - samples[0].1) / 8;
        for pair in samples.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
        }
        eprintln!("{statement}: {samples:?}");
    }
}

#[test]
fn ticket_42_generic_functions_and_recovery() {
    for modifiers in ["", "pub ", "async ", "pub async "] {
        for parameters in ["T", "T, U", "Element", "T: Bound", ""] {
            let source = format!(
                "{modifiers}fn take3<{parameters}>(x: i64) {{ println(x); }} async fn main() {{}}"
            );
            let (program, diagnostics) =
                Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert!(
                diagnostics[0]
                    .message
                    .contains("generic functions are not supported yet")
            );
            assert!(diagnostics[0].helps[0].contains("take3"));
            let span = diagnostics[0].primary_span().unwrap();
            assert_eq!(&source[span.start..span.end], "<");
            assert!(
                program.items.iter().any(
                    |item| matches!(item, Item::Function(f) if f.name == "main" && f.is_async)
                )
            );
        }
    }
}

#[test]
fn ticket_42_call_result_help_and_nested_recovery() {
    for expression in ["choose(1)(2)", "object.choose()(2)", "Factory::choose()(2)"] {
        for statement in [
            format!("{expression};"),
            format!("match v {{ A => {expression}, B => 0 }}"),
            format!("if true {{ {expression}; }}"),
        ] {
            let source = format!("fn broken() {{ {statement} }} pub fn following() {{}}");
            let (program, diagnostics) =
                Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert!(diagnostics[0].helps[0].contains("`choose`"));
            assert!(!diagnostics[0].helps[0].contains("pick"));
            assert!(program.items.iter().any(
                |item| matches!(item, Item::Function(f) if f.name == "following" && f.public)
            ));
        }
    }
}

#[test]
fn ticket_42_recovery_reads_scale_linearly() {
    for nested in [false, true] {
        let mut samples = Vec::new();
        for count in [8, 16, 32, 64] {
            let bad = "match value { Wrap::Value(Inner::Bad(n)) => 0 }";
            let source = if nested {
                format!(
                    "fn bad() {{ {} {bad} {} }}",
                    "if true {".repeat(count),
                    "}".repeat(count)
                )
            } else {
                format!("fn bad() {{ {bad} }}").repeat(count)
            };
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let diagnostics = errors(&source);
            assert_eq!(
                diagnostics.len(),
                if nested { 1 } else { count },
                "{diagnostics:?}"
            );
            samples.push((count, PARSER_TOKEN_READS.with(|v| v.get())));
        }
        let slope = (samples[1].1 - samples[0].1) / 8;
        for pair in samples.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
        }
        eprintln!("recovery nested={nested}: {samples:?}");
    }
}

#[test]
fn ticket_60_generic_classes_and_recovery() {
    for modifiers in ["", "pub ", "open ", "pub open "] {
        for header in [
            "<T>",
            "<T, U>",
            "<>",
            "<T: Bound>",
            "<T> extends Base implements Get<T>",
        ] {
            let source = format!(
                "{modifiers}class Box{header} {{ pub v: T; pub init(self, v: T) {{ self.v = v; }} pub fn get(self) -> T {{ if true {{ return self.v; }} return self.v; }} }} pub fn following() {{}}"
            );
            let (program, diagnostics) =
                Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                "generic classes are not supported yet (willow-b06l)"
            );
            assert_eq!(diagnostics[0].code, ErrorCode::E0102);
            let span = diagnostics[0].primary_span().unwrap();
            assert_eq!(&source[span.start..span.end], "<");
            assert_eq!(program.items.len(), 1);
            assert!(
                matches!(&program.items[0], Item::Function(f) if f.name == "following" && f.public)
            );
        }
    }
}

#[test]
fn ticket_60_supported_interfaces_and_classes() {
    for source in [
        "interface Get<T> { fn get(self) -> T; }",
        "pub interface Pair<T, U> { fn get(self) -> T; fn other(self) -> U; }",
        "class Box { pub v: i64; }",
        "class Box implements Get<i64> { pub fn get(self) -> i64 { return 1; } }",
    ] {
        assert!(errors(source).is_empty(), "{source}");
    }
}

#[test]
fn ticket_60_truncated_generic_class() {
    for source in [
        "class Box<T>",
        "class Box<",
        "class Box<T> { pub fn get(self) { if true {} }",
    ] {
        let diagnostics = errors(source);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "generic classes are not supported yet (willow-b06l)"
        );
    }
}

#[test]
fn ticket_60_recovery_reads_scale_linearly() {
    for shape in ["declarations", "depth", "members"] {
        let mut samples = Vec::new();
        for count in [8, 16, 32, 64] {
            let source = match shape {
                "declarations" => "class Box<T> { pub fn get(self) { } }".repeat(count),
                "depth" => format!(
                    "class Box<T> {{ fn get(self) {{ {} {} }} }}",
                    "if true {".repeat(count),
                    "}".repeat(count)
                ),
                _ => format!("class Box<T> {{ {} }}", "pub fn get(self) {}".repeat(count)),
            };
            PARSER_TOKEN_READS.with(|v| v.set(0));
            assert_eq!(
                errors(&source).len(),
                if shape == "declarations" { count } else { 1 }
            );
            samples.push((count, PARSER_TOKEN_READS.with(|v| v.get())));
        }
        let slope = (samples[1].1 - samples[0].1) / 8;
        for pair in samples.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
        }
        eprintln!("generic class {shape}: {samples:?}");
    }
}

#[test]
fn ticket_60_following_item_boundaries() {
    for following in [
        "open class Following {}",
        "pub open class Following {}",
        "const VALUE: i64 = 1;",
        "class Following {}",
        "interface Following<T> {}",
        "enum Following { Value }",
        "async fn following() {}",
    ] {
        let tokens = Lexer::new(following).tokenize().unwrap();
        let (expected, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let source = format!("class Box<T> {{}} {following}");
        let (actual, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert_eq!(errors.len(), 1);
        assert_eq!(actual.items.len(), expected.items.len());
        if let Item::Class(class) = &actual.items[0] {
            assert_eq!(class.is_open, following.contains("open"));
        }
        assert_eq!(
            std::mem::discriminant(&actual.items[0]),
            std::mem::discriminant(&expected.items[0])
        );
    }
}

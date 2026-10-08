use super::*;
use crate::lexer::Lexer;

fn parse(source: &str) -> (Program, Vec<Diagnostic>) {
    Parser::new(Lexer::new(source).tokenize().unwrap()).parse()
}

#[test]
fn generic_interface_extends_rejection_perspectives() {
    // Each case must produce exactly one dedicated diagnostic, omit the rejected
    // interface, and retain the following declaration without a `self` cascade.
    for declaration in [
        "interface F extends P<i64> {}",            // 1 concrete argument
        "interface F<T> extends P<T> {}",           // 2 type parameter
        "pub interface F extends P<i64> {}",        // 3 visibility
        "interface F extends lib::P<i64> {}",       // 4 qualified name
        "interface F extends a::b::P<i64> {}",      // 5 deep path
        "interface F extends P<i64, String> {}",    // 6 multiple arguments
        "interface F extends P<Array<i64>> {}",     // 7 nested closers
        "interface F extends P<A<B<C<i64>>>> {}",   // 8 deeper nesting
        "interface F extends Plain, P<i64> {}",     // 9 second parent
        "interface F extends P<i64>, Plain {}",     // 10 following parent
        "interface F extends P<i64>, Q<String> {}", // 11 multiple unsupported parents
        "interface F extends P<i64> { fn get(self) -> i64; }", // 12 receiver
        "interface F extends P<i64> { fn a(self); fn b(self); }", // 13 multiple receivers
        "interface F extends P<i64> { fn a(); }",   // 14 no receiver
        "interface F extends P<i64> { fn a(self) { if true { return; } } }", // 15 nested body
        "interface F extends P<i64> { fn a(self) { println(\"}\"); } }", // 16 brace in string
        "interface F extends P</* { */ i64> { /* } */ fn a(self); }", // 17 comments
        "interface F extends P < i64 > {}",         // 18 whitespace
        "interface F extends P<fn(i64) -> i64> {}", // 19 function type
        "interface F extends P<(i64, String)> {}",  // 20 tuple type
        "interface F extends P<i64> { fn a(self) { let x = Box { value: 1 }; } }", // 21 object braces
    ] {
        let source = format!("{declaration} fn after() {{}}");
        let (program, errors) = parse(&source);
        assert_eq!(errors.len(), 1, "{source}: {errors:?}");
        assert_eq!(errors[0].code, ErrorCode::E0427, "{source}");
        assert!(errors[0].message.contains("not supported"));
        assert!(!errors[0].helps.is_empty());
        let span = errors[0].primary_span().unwrap();
        assert_eq!(&source[span.start..span.end], "<");
        assert_eq!(program.items.len(), 1, "{source}");
        assert!(matches!(&program.items[0], Item::Function(f) if f.name == "after"));
    }
}

#[test]
fn generic_interface_extends_preserves_following_items_and_errors() {
    for next in [
        "pub fn after() {}",
        "async fn after() {}",
        "class After {}",
        "interface After {}",
        "enum After { Value }",
    ] {
        let (program, errors) = parse(&format!("interface F extends P<i64> {{}} {next}"));
        assert_eq!(errors.len(), 1);
        assert_eq!(program.items.len(), 1, "{next}");
    }
    let (_, errors) = parse("interface F extends P<i64> {} fn after(x i64) {}");
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].code, ErrorCode::E0427);
    assert_eq!(errors[1].code, ErrorCode::E0102);
}

#[test]
fn generic_interface_extends_eof_terminates() {
    for source in [
        "interface F extends P<",
        "interface F extends P<i64>",
        "interface F extends P<i64> { fn a(self);",
        "interface F extends P<i64> { fn a(self) {",
    ] {
        let (program, errors) = parse(source);
        assert!(program.items.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ErrorCode::E0427);
    }
}

#[test]
fn generic_interface_extends_supported_syntax_unchanged() {
    for source in [
        "interface F extends P { fn a(self); }",
        "interface F extends P, Q {}",
        "interface F<T> extends P {}",
        "interface F extends lib::P {}",
        "interface P<T> { fn get(self) -> T; }",
        "class C implements P<i64> {}",
    ] {
        let (program, errors) = parse(source);
        assert!(errors.is_empty(), "{source}: {errors:?}");
        assert_eq!(program.items.len(), 1);
    }
}

#[test]
fn generic_interface_extends_linear_token_reads() {
    for shape in ["methods", "depth", "declarations", "parents", "arguments"] {
        let mut samples = Vec::new();
        for n in [8, 16, 32, 64] {
            let source = match shape {
                "methods" => format!(
                    "interface F extends P<i64> {{ {} }}",
                    "fn a(self);".repeat(n)
                ),
                "depth" => format!(
                    "interface F extends P<i64> {{ fn a(self) {{ {} {} }} }}",
                    "{".repeat(n),
                    "}".repeat(n)
                ),
                "declarations" => "interface F extends P<i64> { fn a(self); }".repeat(n),
                "parents" => format!("interface F extends {} P<i64> {{}}", "Plain, ".repeat(n)),
                "arguments" => format!(
                    "interface F extends P<{}i64{}> {{}}",
                    "A<".repeat(n),
                    ">".repeat(n)
                ),
                _ => unreachable!(),
            };
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let (_, errors) = parse(&source);
            assert_eq!(errors.len(), if shape == "declarations" { n } else { 1 });
            samples.push((n, PARSER_TOKEN_READS.with(|v| v.get())));
        }
        for pair in samples.windows(3) {
            assert_eq!(pair[2].1 - pair[1].1, 2 * (pair[1].1 - pair[0].1));
        }
        eprintln!("{shape}: {samples:?}");
    }
}

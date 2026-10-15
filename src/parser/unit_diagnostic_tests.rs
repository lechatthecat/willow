use crate::{diagnostics::ErrorCode, lexer::Lexer, parser::Parser};

#[test]
fn unit_value_diagnostic_names_supported_alternative() {
    // Ten expression positions, including the original reported syntax.
    for source in [
        "fn finish() -> Result<(), String> { return Ok(()); }",
        "fn finish() -> Result<void, String> { return Result::Ok(()); }",
        "fn main() { let x = (); }",
        "fn main() { (); }",
        "fn main() { consume(()); }",
        "fn main() { let x = (()); }",
        "fn main() { let x = (1, ()); }",
        "fn main() { let x = [()]; }",
        "fn main() { let x = match true { _ => () }; }",
        "fn main() { let f = || (); }",
    ] {
        let (_, diagnostics) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(
            diagnostics.iter().any(|d| d.code == ErrorCode::E0102
                && d.message.contains("`()` is not a unit value")
                && d.message.contains("Result<void, E>")
                && d.message.contains("Result::Ok()")),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn unit_diagnostic_preserves_parentheses_and_void_syntax() {
    // Ten neighboring syntax perspectives must remain accepted by the parser.
    for source in [
        "fn main() {}",
        "fn finish() -> () {}",
        "fn finish() -> void {}",
        "fn finish() -> Result<(), String> { return Result::Ok(); }",
        "fn finish() -> Result<void, String> { return Result::Ok(); }",
        "fn main() { let x = (1); }",
        "fn main() { let x = (1,); }",
        "fn main() { let x = (1, true); }",
        "fn main() { let f = || 1; }",
        "fn main() { let x: fn() -> () = finish; }",
    ] {
        let (_, diagnostics) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
    }
}

#[test]
fn unit_diagnostic_parser_scaling_is_linear() {
    use super::PARSER_TOKEN_READS;
    let mut samples = Vec::new();
    for n in [8, 16, 32, 64] {
        let source = format!("fn main() {{ {} }}", "let x = ();".repeat(n));
        let tokens = Lexer::new(&source).tokenize().unwrap();
        PARSER_TOKEN_READS.with(|v| v.set(0));
        let (_, errors) = Parser::new(tokens).parse();
        let reads = PARSER_TOKEN_READS.with(|v| v.get());
        assert_eq!(errors.len(), n);
        samples.push((n, reads));
    }
    let slope = samples[1].1 - samples[0].1;
    for pair in samples.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0) / 8);
    }
    eprintln!("unit diagnostic (expressions, token reads): {samples:?}");
}

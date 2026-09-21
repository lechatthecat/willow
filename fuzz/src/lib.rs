use willow_compiler::{desugar::DesugarPass, lexer::Lexer, parser::Parser, semantic::TypeChecker};

/// Exercise checking only after the preceding stages accept the source.
/// `None` means an earlier stage rejected it; `Some` counts checker diagnostics.
/// Diagnostics are normal outcomes, but panics and destructor failures propagate.
pub fn check_source(data: &[u8]) -> Option<usize> {
    let source = std::str::from_utf8(data).ok()?;
    let tokens = Lexer::new(source).tokenize().ok()?;
    let (mut program, diagnostics) = Parser::new(tokens).parse();
    if !diagnostics.is_empty() {
        return None;
    }
    if !DesugarPass::run(&mut program, &mut [])
        .diagnostics
        .is_empty()
    {
        return None;
    }
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    Some(checker.errors.len())
}

#[cfg(test)]
mod tests {
    use super::check_source;

    #[test]
    fn seeds_reach_the_checker() {
        for (source, valid) in [
            (
                include_bytes!("../seeds/type_checker/scalars.wi").as_slice(),
                true,
            ),
            (
                include_bytes!("../seeds/type_checker/classes.wi").as_slice(),
                true,
            ),
            (
                include_bytes!("../seeds/type_checker/type_errors.wi").as_slice(),
                false,
            ),
            (
                include_bytes!("../seeds/type_checker/cycle.wi").as_slice(),
                false,
            ),
            (
                include_bytes!("../seeds/type_checker/control.wi").as_slice(),
                false,
            ),
        ] {
            let errors = check_source(source).expect("seed must reach the checker");
            assert_eq!(errors == 0, valid, "{}", String::from_utf8_lossy(source));
        }
    }

    #[test]
    fn invalid_frontend_input_is_rejected() {
        for source in [b"\xff".as_slice(), b"\"unterminated", b"fn main( {"] {
            assert_eq!(check_source(source), None);
        }
    }

    #[test]
    fn increasing_shapes_reach_the_checker() {
        for size in [1, 8, 32, 128] {
            let mut chain = String::from("open class C0 {}\n");
            let mut fanout = String::from("fn base() -> i64 { return 1; }\n");
            let mut calls = String::from("fn base() {} fn main() {\n");
            let mut cycle = String::new();
            for index in 0..size {
                chain.push_str(&format!(
                    "open class C{} extends C{index} {{}}\n",
                    index + 1
                ));
                fanout.push_str(&format!(
                    "fn helper_{index}() -> i64 {{ return base(); }}\n"
                ));
                calls.push_str("base();\n");
                cycle.push_str(&format!(
                    "open class C{index} extends C{} {{}}\n",
                    (index + 1) % size
                ));
            }
            chain.push_str("fn main() {}\n");
            fanout.push_str("fn main() {}\n");
            calls.push('}');
            cycle.push_str("fn main() {}\n");
            for (shape, source, valid) in [
                ("chain", chain, true),
                ("fanout", fanout, true),
                ("calls", calls, true),
                ("cycle", cycle, false),
            ] {
                let errors = check_source(source.as_bytes())
                    .unwrap_or_else(|| panic!("{shape} size={size} must reach checker"));
                assert_eq!(errors == 0, valid, "{shape} size={size}");
            }
        }
    }
}

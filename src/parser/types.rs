use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode};
use crate::lexer::token::TokenKind;

#[willow_continuations::parser]
impl Parser {
    pub(super) fn parse_type(&mut self) -> Result<Type, Diagnostic> {
        enum Frame {
            Generic(String, Vec<Type>),
            Params(bool, Vec<Type>),
            Return(bool, Vec<Type>),
        }
        fn callable(closure: bool, params: Vec<Type>, ret: Type) -> Type {
            if closure {
                Type::Closure(params, Box::new(ret))
            } else {
                Type::Fn(params, Box::new(ret))
            }
        }
        fn generic(name: String, mut args: Vec<Type>) -> Type {
            if name == "Array" && args.len() == 1 {
                Type::Array(Box::new(args.remove(0)))
            } else if name == "JoinHandle" && args.len() == 1 {
                Type::Generic("Task".into(), args)
            } else {
                Type::Generic(name, args)
            }
        }
        let mut frames = Vec::new();
        loop {
            let type_start = self.current_span();
            if let TokenKind::I64 | TokenKind::F64 | TokenKind::Bool = self.peek_kind() {
                let name = match self.peek_kind() {
                    TokenKind::I64 => "i64",
                    TokenKind::F64 => "f64",
                    _ => "bool",
                };
                self.type_uses.push(TypeUse {
                    name: name.into(),
                    span: type_start,
                });
            }
            let mut value = match self.peek_kind().clone() {
                TokenKind::I64 => {
                    self.advance();
                    Type::I64
                }
                TokenKind::F64 => {
                    self.advance();
                    Type::F64
                }
                TokenKind::Bool => {
                    self.advance();
                    Type::Bool
                }
                TokenKind::LParen if self.peek_kind_at(1) == &TokenKind::RParen => {
                    self.advance();
                    self.advance();
                    Type::Void
                }
                TokenKind::Fn => {
                    self.advance();
                    self.expect(TokenKind::LParen)?;
                    if !self.check(TokenKind::RParen) && !self.at_eof() {
                        frames.push(Frame::Params(false, Vec::new()));
                        continue;
                    }
                    self.expect(TokenKind::RParen)?;
                    if self.eat(TokenKind::Arrow) {
                        frames.push(Frame::Return(false, Vec::new()));
                        continue;
                    }
                    callable(false, Vec::new(), Type::Void)
                }
                TokenKind::Ident(name)
                    if name == "closure" && self.peek_kind_at(1) == &TokenKind::LParen =>
                {
                    self.advance();
                    self.expect(TokenKind::LParen)?;
                    if !self.check(TokenKind::RParen) && !self.at_eof() {
                        frames.push(Frame::Params(true, Vec::new()));
                        continue;
                    }
                    self.expect(TokenKind::RParen)?;
                    if self.eat(TokenKind::Arrow) {
                        frames.push(Frame::Return(true, Vec::new()));
                        continue;
                    }
                    callable(true, Vec::new(), Type::Void)
                }
                TokenKind::Ident(name) => {
                    self.advance();
                    let mut parts = vec![name];
                    while self.eat(TokenKind::ColonColon) {
                        parts.push(self.expect_ident()?);
                    }
                    let name = parts.join("::");
                    self.type_uses.push(TypeUse {
                        name: name.clone(),
                        span: type_start.to(self.tokens[self.pos - 1].span),
                    });
                    if self.eat(TokenKind::Lt) {
                        if !self.at_type_gt() && !self.at_eof() {
                            frames.push(Frame::Generic(name, Vec::new()));
                            continue;
                        }
                        self.expect_type_gt()?;
                        generic(name, Vec::new())
                    } else if name == "String" {
                        Type::String
                    } else if name == "void" {
                        Type::Void
                    } else {
                        Type::Named(name)
                    }
                }
                _ => return Err(self.err(
                    ErrorCode::E0107,
                    "expected type (`i64`, `f64`, `bool`, `void` / `()`, `fn(...)`, `closure(...)`, or type name)",
                )),
            };
            loop {
                while self.eat(TokenKind::Question) {
                    value = Type::Generic("Option".into(), vec![value]);
                }
                match frames.pop() {
                    None => return Ok(value),
                    Some(Frame::Return(closure, params)) => {
                        value = callable(closure, params, value);
                    }
                    Some(Frame::Generic(name, mut args)) => {
                        args.push(value);
                        if self.eat(TokenKind::Comma) && !self.at_type_gt() && !self.at_eof() {
                            frames.push(Frame::Generic(name, args));
                            break;
                        }
                        self.expect_type_gt()?;
                        value = generic(name, args);
                    }
                    Some(Frame::Params(closure, mut params)) => {
                        params.push(value);
                        if self.eat(TokenKind::Comma)
                            && !self.check(TokenKind::RParen)
                            && !self.at_eof()
                        {
                            frames.push(Frame::Params(closure, params));
                            break;
                        }
                        self.expect(TokenKind::RParen)?;
                        if self.eat(TokenKind::Arrow) {
                            frames.push(Frame::Return(closure, params));
                            break;
                        }
                        value = callable(closure, params, Type::Void);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{diagnostics::Diagnostic, lexer::Lexer, parser::Parser, semantic::TypeChecker};

    #[test]
    fn generic_closer_touching_equals_regression() {
        for ty in ["Option<i64>", "Option<Option<i64>>"] {
            let source = format!("fn main() {{ let x: {ty}= None; }}");
            let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{source}: {errors:?}");
        }
    }

    fn check(source: &str) -> Vec<Diagnostic> {
        let source = format!("import std::collections::Array; {source}");
        let tokens = Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        if !errors.is_empty() {
            return errors;
        }
        let mut checker = TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        checker
            .errors
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == crate::diagnostics::Severity::Error)
            .collect()
    }

    #[test]
    fn unit_task_annotation_has_no_cascades() {
        let errors = check(
            "async fn work() {} async fn main() { let ts: Array<Task<()>> = []; ts.push(work()); for t in ts { await t; } }",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn frozen_iteration_has_actionable_help_and_no_cascades() {
        let errors = check(
            "class Cell { pub n: i64; pub init(self) { self.n = 1; } } fn main() { let edits = [new Cell()].freeze(); for e in edits { println(e.n); } }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .helps
                .iter()
                .any(|help| help.contains("0..") && help.contains("[i]")),
            "{errors:?}"
        );
    }

    #[test]
    fn unit_type_spellings_and_positions() {
        for source in [
            "fn main() -> () {}",
            "fn main() -> void {}",
            "async fn main() -> () {}",
            "async fn work() -> () {} async fn main() { let t: Task<()> = work(); await t; }",
            "async fn work() {} async fn main() { let t: Task<void> = work(); await t; }",
            "async fn work() {} async fn main() { let t: JoinHandle<()> = work(); await t; }",
            "fn consume(t: Task<()>) {} fn main() {}",
            "fn consume(ts: Array<Task<()>>) {} fn main() {}",
            "fn consume(ts: Array<Array<Task<()>>>) {} fn main() {}",
            "fn work() {} fn main() { let f: fn() -> () = work; f(); }",
            "fn consume(f: closure() -> ()) {} fn main() {}",
            "fn consume(f: fn(Task<()>) -> ()) {} fn main() {}",
            "fn consume(t: Task< ( /* empty */ ) >) {} fn main() {}",
            "fn consume(t: Task<(),>) {} fn main() {}",
        ] {
            let errors = check(source);
            assert!(errors.is_empty(), "{source}: {errors:?}");
        }
    }

    #[test]
    fn nonempty_parenthesized_types_stay_unsupported() {
        for ty in ["(i64)", "(i64, bool)", "(void)", "(,)"] {
            let errors = check(&format!("fn consume(t: {ty}) {{}} fn main() {{}}"));
            assert!(!errors.is_empty(), "{ty}");
            assert!(
                errors[0].message.contains("`void` / `()`"),
                "{ty}: {errors:?}"
            );
        }
    }

    #[test]
    fn invalid_iteration_keeps_independent_errors() {
        for (ty, help) in [
            ("FrozenArray<i64>", "values[i]"),
            ("FrozenMap<i64, i64>", "values.get(key)"),
            ("i64", "Array<T>"),
        ] {
            for use_item in [
                "println(item.field);",
                "item.method();",
                "let x: bool = item;",
                "for nested in item { println(nested); }",
            ] {
                let source = format!(
                    "fn consume(values: {ty}) {{ for item in values {{ {use_item} missing(); }} }} fn main() {{}}"
                );
                let errors = check(&source);
                assert_eq!(errors.len(), 2, "{source}: {errors:?}");
                assert!(
                    errors[0].helps.iter().any(|text| text.contains(help)),
                    "{errors:?}"
                );
                assert!(errors[1].message.contains("missing"), "{errors:?}");
            }
        }
    }

    #[test]
    fn bare_task_declaration_does_not_cascade() {
        let errors = check(
            "async fn work() {} async fn main() { let ts: Array<Task> = []; ts.push(work()); for t in ts { await t; } }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn invalid_annotations_and_await_keep_independent_errors() {
        for (source, messages) in [
            (
                "async fn main() { let t: Task = unknown(); await t; }",
                vec!["cannot find type `Task`", "cannot find function `unknown`"],
            ),
            (
                "fn main() { let t: Task = 1; await t; }",
                vec![
                    "cannot find type `Task`",
                    "`await` can only be used inside an async function",
                ],
            ),
            (
                "async fn main() -> i64 { let t: Task = 1; return await t; }",
                vec!["cannot find type `Task`"],
            ),
            (
                "async fn main() { await missing; }",
                vec!["cannot find variable `missing`"],
            ),
        ] {
            let errors = check(source);
            assert_eq!(errors.len(), messages.len(), "{source}: {errors:?}");
            for (error, message) in errors.iter().zip(messages) {
                assert!(error.message.contains(message), "{errors:?}");
            }
        }
    }

    #[test]
    fn unit_alias_has_identical_type_tree() {
        for (unit, void) in [
            ("()", "void"),
            ("Task<()>", "Task<void>"),
            ("Array<Task<()>>", "Array<Task<void>>"),
            ("fn() -> ()", "fn() -> void"),
            ("closure(Task<()>) -> ()", "closure(Task<void>) -> void"),
            ("()?", "void?"),
        ] {
            let parse = |text| {
                Parser::new(Lexer::new(text).tokenize().unwrap())
                    .parse_type()
                    .unwrap()
            };
            assert_eq!(parse(unit), parse(void), "{unit}");
        }
    }

    #[test]
    fn unit_type_parser_work_scales_linearly() {
        use crate::parser::PARSER_TOKEN_READS;
        for deep in [false, true] {
            let mut samples = Vec::new();
            for count in [8, 16, 32, 64] {
                let source = if deep {
                    format!(
                        "fn consume(t: {}Task<()>{}) {{}}",
                        "Array<".repeat(count),
                        ">".repeat(count)
                    )
                } else {
                    format!(
                        "fn main() {{ {} }}",
                        "let ts: Array<Task<()>> = [];".repeat(count)
                    )
                };
                let tokens = Lexer::new(&source).tokenize().unwrap();
                PARSER_TOKEN_READS.with(|reads| reads.set(0));
                let (_, errors) = Parser::new(tokens).parse();
                assert!(errors.is_empty(), "{errors:?}");
                samples.push((count, PARSER_TOKEN_READS.with(|reads| reads.get())));
            }
            let slope = (samples[1].1 - samples[0].1) / 8;
            for pair in samples.windows(2) {
                assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
            }
            eprintln!("unit types deep={deep}: {samples:?}");
        }
    }
}

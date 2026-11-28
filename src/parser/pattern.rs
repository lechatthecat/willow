use super::Parser;
use super::ast::*;
use super::import_usage::ReferenceKind;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::token::TokenKind;

/// Deepest payload nesting a pattern may have. Real patterns nest a few
/// levels; the bound keeps every recursive pattern walk (checking, coverage,
/// lowering, cloning, dropping) within a fixed native-stack budget.
pub(crate) const MAX_PATTERN_DEPTH: usize = 64;

#[willow_continuations::parser]
impl Parser {
    pub(super) fn parse_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        let span = self.current_span();
        match self.peek_kind().clone() {
            TokenKind::Ident(ref name) if name == "_" => {
                self.advance();
                Ok(Pattern::Wildcard(span, PatternId::fresh()))
            }
            TokenKind::LParen => {
                self.advance();
                let mut fields = Vec::new();
                loop {
                    fields.push(self.parse_sub_pattern()?);
                    if fields.len() == 1 {
                        self.expect(TokenKind::Comma)?;
                    } else if !self.eat(TokenKind::Comma) {
                        break;
                    }
                    if matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
                self.tuple_arities.insert(fields.len());
                Ok(Pattern::EnumVariantTuple {
                    enum_name: super::tuples::name(fields.len()),
                    variant: super::tuples::VARIANT.into(),
                    fields,
                    variant_span: span,
                    span: span.to(self.previous_span()),
                    id: PatternId::fresh(),
                })
            }
            TokenKind::True => {
                self.advance();
                Ok(Pattern::LiteralBool(true, span, PatternId::fresh()))
            }
            TokenKind::False => {
                self.advance();
                Ok(Pattern::LiteralBool(false, span, PatternId::fresh()))
            }
            TokenKind::Integer(n) => {
                self.advance();
                Ok(Pattern::LiteralInt(n, span, PatternId::fresh()))
            }
            TokenKind::CharLiteral(n) => {
                self.advance();
                Ok(Pattern::LiteralChar(n, span, PatternId::fresh()))
            }
            TokenKind::StringLiteral(_) => Err(self
                .err(
                    ErrorCode::E0102,
                    "string literal patterns are not supported",
                )
                .with_help("use if/else with string equality (==) instead")),
            TokenKind::Float(_) | TokenKind::Nil => Err(self.unsupported_literal_pattern(span)),
            TokenKind::Minus => {
                self.advance();
                match self.peek_kind().clone() {
                    TokenKind::Integer(n) => {
                        let end = self.current_span();
                        self.advance();
                        let merged = span.to(end);
                        Ok(Pattern::LiteralInt(-n, merged, PatternId::fresh()))
                    }
                    TokenKind::Float(_) => Err(self.unsupported_literal_pattern(span)),
                    _ => Err(self.err(ErrorCode::E0102, "expected integer after '-' in pattern")),
                }
            }
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.advance();
                if matches!(self.peek_kind(), TokenKind::ColonColon) {
                    self.default_import_usage
                        .reference(&name, ReferenceKind::Type);
                    self.advance(); // consume ::
                    // Collect all `::`-separated segments; the last is the
                    // variant, the rest (joined) form the enum name. This
                    // accepts a module-qualified enum, e.g. `palette::Color::Red`
                    // (enum `palette::Color`, variant `Red`).
                    let mut segments = vec![name, self.expect_ident()?];
                    while self.eat(TokenKind::ColonColon) {
                        segments.push(self.expect_ident()?);
                    }
                    let variant_span = self.previous_span();
                    let variant = segments.pop().unwrap();
                    let name = segments.join("::");
                    if matches!(self.peek_kind(), TokenKind::LParen) {
                        self.advance(); // consume (
                        let mut fields = Vec::new();
                        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                            fields.push(self.parse_sub_pattern()?);
                            if !self.eat(TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(TokenKind::RParen)?;
                        let merged = span.to(self.previous_span());
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: name,
                            variant,
                            variant_span,
                            fields,
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    } else {
                        let merged = span.to(self.previous_span());
                        Ok(Pattern::EnumVariant {
                            enum_name: name,
                            variant,
                            variant_span,
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    }
                } else if matches!(self.peek_kind(), TokenKind::LParen) {
                    self.default_import_usage
                        .reference(&name, ReferenceKind::Type);
                    // One plain name or `_` remains ambiguous with a class
                    // downcast; anything else can only be an enum pattern
                    // whose enum the checker takes from the scrutinee type.
                    self.advance();
                    let mut fields = Vec::new();
                    while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                        fields.push(self.parse_sub_pattern()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    let merged = span.to(self.previous_span());
                    let downcast = match fields.as_slice() {
                        [Pattern::Binding { name, .. }] => Some(name.clone()),
                        [Pattern::Wildcard(..)] => Some("_".to_string()),
                        _ => None,
                    };
                    if let Some(binding) = downcast {
                        Ok(Pattern::ClassDowncast {
                            class_name: name,
                            binding,
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    } else {
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: String::new(),
                            variant: name,
                            variant_span: span,
                            fields,
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    }
                } else {
                    Ok(Pattern::Binding {
                        name,
                        span,
                        id: PatternId::fresh(),
                    })
                }
            }
            _ => Err(self.err(ErrorCode::E0102, "expected pattern")),
        }
    }

    /// One payload or tuple element: a full pattern one level deeper.
    fn parse_sub_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        if self.pattern_depth + 1 >= MAX_PATTERN_DEPTH {
            return Err(self
                .err(
                    ErrorCode::E0102,
                    format!("pattern nests more than {MAX_PATTERN_DEPTH} levels deep"),
                )
                .with_help("bind an inner payload to a name and match it separately"));
        }
        self.pattern_depth += 1;
        // Alternatives parse at this depth too, so `Some(1 | Some(1 | ..))`
        // still counts toward the limit.
        let pattern = match self.parse_pattern() {
            Ok(pattern) => self.reject_or_pattern(pattern, false),
            Err(error) => Err(error),
        };
        self.pattern_depth -= 1;
        pattern
    }

    fn unsupported_literal_pattern(&self, start: crate::diagnostics::Span) -> Diagnostic {
        let what = match self.peek_kind() {
            TokenKind::Nil => "`nil` patterns are not supported",
            _ => "float literal patterns are not supported",
        };
        Diagnostic::new(Severity::Error, ErrorCode::E0102, what)
            .with_label(Label::primary(start, "unsupported literal pattern"))
            .with_help("bind the value to a name and compare it in a match guard, for example `Value(x) if x == 1.5`")
    }
}

#[willow_continuations::parser]
impl Parser {
    /// Willow has no or-patterns. After `first`, consume every `| pattern`
    /// alternative, report them once, and keep `first` so parsing continues.
    /// `match_arm` says whether the guard rewrite applies.
    pub(super) fn reject_or_pattern(
        &mut self,
        first: Pattern,
        match_arm: bool,
    ) -> Result<Pattern, Diagnostic> {
        if !self.check(TokenKind::Pipe) {
            return Ok(first);
        }
        let mut alternatives = vec![first];
        while self.eat(TokenKind::Pipe) {
            alternatives.push(self.parse_pattern()?);
        }
        let span = alternatives[0]
            .span()
            .to(alternatives[alternatives.len() - 1].span());
        let written = alternatives
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        let mut error = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0102,
            format!("or-patterns are not supported: `{written}`"),
        )
        .with_label(Label::primary(span, "or-pattern"));
        error = if match_arm {
            let help = "write a separate arm for each alternative";
            match literal_guard(&alternatives) {
                Some(guard) => error.with_help(format!(
                    "{help}, or bind the value and test it in a guard: `v if {guard} =>`"
                )),
                None => error.with_help(format!(
                    "{help}, sharing the body through a helper function"
                )),
            }
        } else {
            error.with_help(
                "match each alternative separately, or use a `match` with one arm per alternative",
            )
        };
        self.recovered_errors.push(error);
        Ok(alternatives.swap_remove(0))
    }
}

/// `v == 1 || v == 2` for alternatives that are all integer or character
/// literals; `None` otherwise.
fn literal_guard(alternatives: &[Pattern]) -> Option<String> {
    let mut tests = Vec::with_capacity(alternatives.len());
    for alternative in alternatives {
        let literal = match alternative {
            Pattern::LiteralInt(n, ..) => n.to_string(),
            Pattern::LiteralChar(c, ..) if !c.is_control() && !matches!(c, '\\' | '\'') => {
                format!("'{c}'")
            }
            _ => return None,
        };
        tests.push(format!("v == {literal}"));
    }
    Some(tests.join(" || "))
}

#[cfg(test)]
mod tests {
    use crate::{lexer::Lexer, parser::Parser};

    /// Operators weaker than `&&` regroup silently next to a let chain, so
    /// every unparenthesized `||`, `? :` and `..` in or around a chain is a
    /// diagnostic; parenthesized, the same operands parse.
    #[test]
    fn weak_operators_in_let_chains_need_parentheses() {
        for (head, operator) in [
            ("let Some(x) = o && x > 0 || flag", "||"),
            ("let Some(x) = o || p", "||"),
            ("a || b && let Some(x) = o", "||"),
            ("let Some(x) = o && let Some(y) = p || q", "||"),
            ("let Some(x) = o && c ? a : b", "? :"),
            ("c ? a : b && let Some(x) = o", "? :"),
            ("let Some(x) = o && 0..x", ".."),
        ] {
            for form in ["if", "while"] {
                let source = format!("fn f() {{ {form} {head} {{ println(1); }} }}");
                let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                assert_eq!(
                    errors[0].message,
                    format!("`{operator}` cannot appear unparenthesized next to a `let` chain"),
                    "{source}"
                );
            }
        }
        for head in [
            "let Some(x) = o && (x > 0 || flag)",
            "let Some(x) = (o || p)",
            "(a || b) && let Some(x) = o",
            "let Some(x) = o && (c ? a : b)",
            "a || b",
            "c ? a : b",
            "let Some(x) = o && x > 0 && flag",
        ] {
            let source = format!("fn f() {{ if {head} {{ println(1); }} }}");
            let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{source}: {errors:?}");
        }
    }
    #[test]
    fn string_pattern_diagnostic_perspectives() {
        for literal in [
            r#""""#,
            r#""a""#,
            r#""hello world""#,
            r#""日本語""#,
            r#""a\nb""#,
        ] {
            for body in [
                format!("match value {{ {literal} => 1, _ => 0 }};"),
                format!("match value {{ _ if false => 0, {literal} => 1, _ => 2 }};"),
                format!("if let {literal} = value {{ println(1); }}"),
                format!("while let {literal} = value {{ break; }}"),
            ] {
                let source = format!("fn f() {{ {body} }}\nasync fn main() {{ await sleep(0); }}");
                let (program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                let error = &errors[0];
                assert_eq!(error.code, crate::diagnostics::ErrorCode::E0102);
                assert_eq!(error.message, "string literal patterns are not supported");
                assert!(
                    error
                        .helps
                        .iter()
                        .any(|help| help.contains("if/else") && help.contains("=="))
                );
                let span = error.primary_span().unwrap();
                assert_eq!(span.start, source.find(literal).unwrap());
                assert_eq!(span.end - span.start, literal.len());
                assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async())));
            }
        }
    }

    #[test]
    fn string_pattern_diagnostic_linear_token_reads() {
        use crate::parser::PARSER_TOKEN_READS;
        for count in [16, 64, 256] {
            let arms = "0 => 0, ".repeat(count);
            let source = format!("fn f() {{ match value {{ {arms}\"text\" => 1, _ => 0 }}; }}");
            let tokens = Lexer::new(&source).tokenize().unwrap();
            PARSER_TOKEN_READS.with(|reads| reads.set(0));
            let (_, errors) = Parser::new(tokens).parse();
            let reads = PARSER_TOKEN_READS.with(|reads| reads.get());
            assert_eq!(errors.len(), 1);
            assert_eq!(
                errors[0].message,
                "string literal patterns are not supported"
            );
            assert!(reads < 100 * count + 200, "{count}: {reads}");
            eprintln!("string pattern prefix arms={count}, token reads={reads}");
        }
    }

    #[test]
    fn literal_payload_diagnostic_linear_token_reads() {
        use crate::parser::PARSER_TOKEN_READS;
        for count in [16, 64, 256] {
            let prefix = "binding, ".repeat(count);
            let source = format!("fn f() {{ match value {{ Value({prefix}-1.5) => 1, }} }}");
            let tokens = Lexer::new(&source).tokenize().unwrap();
            PARSER_TOKEN_READS.with(|reads| reads.set(0));
            let (_, errors) = Parser::new(tokens).parse();
            let reads = PARSER_TOKEN_READS.with(|reads| reads.get());
            assert_eq!(errors.len(), 1);
            assert!(errors[0].message.contains("literal patterns"));
            assert!(reads < 20 * count + 200, "{count}: {reads}");
            eprintln!("literal payload prefix bindings={count}, token reads={reads}");
        }
    }

    /// The first arm pattern of the `match` that is `f`'s first statement.
    fn first_arm_pattern(program: &crate::parser::ast::Program) -> &crate::parser::ast::Pattern {
        use crate::parser::ast::{Expr, Item, Stmt};
        let f = program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Function(f) if f.name == "f" => Some(f),
                _ => None,
            })
            .expect("fn f");
        // A trailing `match` without `;` is the function's tail expression.
        let expr = match f.body.stmts.first() {
            Some(Stmt::Expr(stmt)) => &stmt.expr,
            Some(Stmt::Return(ret)) => ret.value.as_ref().expect("tail match"),
            _ => panic!("f starts with a match statement"),
        };
        let Expr::Match(m) = expr else {
            panic!("f starts with a match");
        };
        &m.arms[0].pattern
    }

    #[test]
    fn literal_payload_diagnostic_perspectives() {
        for outer in [
            "Condition::Fracture",
            "Fracture",
            "pkg::Condition::Fracture",
            "Value",
        ] {
            for literal in ["1.5", "-1.5", "\"text\"", "nil"] {
                for prefix in ["", "first, "] {
                    let source = format!(
                        "fn f() {{ match value {{\n{outer}({prefix}{literal}) => println(1),\n}} }}\nasync fn main() {{ await sleep(0); }}"
                    );
                    let (program, errors) =
                        Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                    assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                    let error = &errors[0];
                    assert_eq!(error.code, crate::diagnostics::ErrorCode::E0102);
                    assert!(
                        error.message.contains("literal patterns")
                            || error.message.contains("`nil` patterns"),
                        "{source}: {error:?}"
                    );
                    assert!(!error.message.contains("reserved keyword"));
                    assert!(error.helps.iter().all(|help| !help.contains("rename")));
                    assert_eq!(
                        error.primary_span().unwrap().start,
                        source.find(&format!("{literal})")).unwrap()
                    );
                    assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async())));
                }
            }
            // `bool` and integer literals are ordinary nested patterns.
            for literal in ["true", "false", "0", "42", "-42"] {
                for prefix in ["", "first, "] {
                    let source = format!(
                        "fn f() {{ match value {{\n{outer}({prefix}{literal}) => println(1),\n}} }}"
                    );
                    let (program, errors) =
                        Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                    assert!(errors.is_empty(), "{source}: {errors:?}");
                    let pattern = first_arm_pattern(&program);
                    assert!(pattern.is_nested(), "{source}");
                    assert_eq!(pattern.to_string(), format!("{outer}({prefix}{literal})"));
                }
            }
        }
    }

    #[test]
    fn nested_pattern_twenty_parse_perspectives() {
        for outer in ["Result::Err", "Err", "pkg::Result::Err", "Wrap::Value"] {
            for inner in [
                "E::Bad(n)",
                "E::Bad(_)",
                "Bad(n, m)",
                "E::Empty",
                "E::Bad(E::Bad(n))",
            ] {
                let source = format!(
                    "fn f() {{ match value {{\n{outer}({inner}) => println(1),\n}} }}\nasync fn main() {{ await sleep(0); }}"
                );
                let (program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async())), "{outer}/{inner}: following async modifier was lost");
                assert!(errors.is_empty(), "{source}: {errors:?}");
                let pattern = first_arm_pattern(&program);
                assert!(pattern.is_nested(), "{outer}/{inner}");
                assert_eq!(pattern.to_string(), format!("{outer}({inner})"));
                assert_eq!(pattern.fields().len(), 1);
                assert!(
                    pattern.fields()[0].span().start > pattern.span().start,
                    "{outer}/{inner}: inner span lies inside the outer pattern"
                );
            }
        }
    }

    #[test]
    fn pattern_depth_limit_is_a_diagnostic_not_a_stack_overflow() {
        use super::MAX_PATTERN_DEPTH;
        for depth in [MAX_PATTERN_DEPTH - 2, MAX_PATTERN_DEPTH + 10, 10_000] {
            let pattern = format!("{}x{}", "Some(".repeat(depth), ")".repeat(depth));
            let source = format!("fn f() {{ match value {{ {pattern} => 1, _ => 0 }}; }}");
            let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            if depth < MAX_PATTERN_DEPTH {
                assert!(errors.is_empty(), "{depth}: {errors:?}");
            } else {
                assert!(
                    errors[0].message.contains("levels deep"),
                    "{depth}: {errors:?}"
                );
            }
        }
    }
}

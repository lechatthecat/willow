use super::Parser;
use super::ast::*;
use super::import_usage::ReferenceKind;
use crate::diagnostics::{Diagnostic, ErrorCode};
use crate::lexer::token::TokenKind;

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
                let mut bindings = vec![self.expect_ident()?];
                self.expect(TokenKind::Comma)?;
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    bindings.push(self.expect_ident()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
                let mut seen = std::collections::HashSet::new();
                for binding in &bindings {
                    if binding != "_" && !seen.insert(binding) {
                        return Err(
                            self.err(ErrorCode::E0102, "duplicate binding in tuple pattern")
                        );
                    }
                }
                self.tuple_arities.insert(bindings.len());
                Ok(Pattern::EnumVariantTuple {
                    enum_name: super::tuples::name(bindings.len()),
                    variant: super::tuples::VARIANT.into(),
                    bindings,
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
            TokenKind::StringLiteral(_) => Err(self
                .err(
                    ErrorCode::E0102,
                    "string literal patterns are not supported",
                )
                .with_help("use if/else with string equality (==) instead")),
            TokenKind::Minus => {
                self.advance();
                if let TokenKind::Integer(n) = self.peek_kind().clone() {
                    let end = self.current_span();
                    self.advance();
                    let merged = span.to(end);
                    Ok(Pattern::LiteralInt(-n, merged, PatternId::fresh()))
                } else {
                    Err(self.err(ErrorCode::E0102, "expected integer after '-' in pattern"))
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
                    // (enum `palette::Color`, variant `Red`) (willow-64gs).
                    let mut segments = vec![name, self.expect_ident()?];
                    while self.eat(TokenKind::ColonColon) {
                        segments.push(self.expect_ident()?);
                    }
                    let variant_span = self.previous_span();
                    let variant = segments.pop().unwrap();
                    let name = segments.join("::");
                    if matches!(self.peek_kind(), TokenKind::LParen) {
                        self.advance(); // consume (
                        let mut bindings = Vec::new();
                        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                            bindings.push(self.parse_payload_binding(&name, Some(&variant))?);
                            if matches!(self.peek_kind(), TokenKind::Comma) {
                                self.advance();
                            }
                        }
                        self.expect(TokenKind::RParen)?;
                        let end = self.current_span();
                        let merged = span.to(end);
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: name,
                            variant,
                            variant_span,
                            bindings,
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    } else {
                        let end = self.current_span();
                        let merged = span.to(end);
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
                    // One binding remains ambiguous with a class downcast;
                    // zero or multiple bindings can only be an enum pattern.
                    self.advance();
                    let mut bindings = Vec::new();
                    if !matches!(self.peek_kind(), TokenKind::RParen) {
                        loop {
                            bindings.push(self.parse_payload_binding(&name, None)?);
                            if !self.eat(TokenKind::Comma)
                                || matches!(self.peek_kind(), TokenKind::RParen)
                            {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    let merged = span.to(self.current_span());
                    if bindings.len() == 1 {
                        Ok(Pattern::ClassDowncast {
                            class_name: name,
                            binding: bindings.pop().unwrap(),
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    } else {
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: String::new(),
                            variant: name,
                            variant_span: span,
                            bindings,
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

    fn parse_payload_binding(
        &mut self,
        outer: &str,
        variant: Option<&str>,
    ) -> Result<String, Diagnostic> {
        let start = self.current_span();
        let literal = matches!(
            self.peek_kind(),
            TokenKind::True
                | TokenKind::False
                | TokenKind::Nil
                | TokenKind::Integer(_)
                | TokenKind::Float(_)
                | TokenKind::StringLiteral(_)
        ) || (matches!(self.peek_kind(), TokenKind::Minus)
            && matches!(
                self.peek_kind_at(1),
                TokenKind::Integer(_) | TokenKind::Float(_)
            ));
        if literal {
            return Err(Diagnostic::new(
                crate::diagnostics::Severity::Error,
                ErrorCode::E0102,
                "literal patterns are not supported in variant payloads",
            )
            .with_label(crate::diagnostics::Label::primary(start, "literal payload pattern"))
            .with_help("bind the payload to a variable and use a match guard to test its value; for example, `Fracture(o) if o` for a true boolean payload, or `Value(n) if n == 42` for an integer payload"));
        }
        let name = self.expect_ident()?;
        if matches!(self.peek_kind(), TokenKind::ColonColon | TokenKind::LParen) {
            let outer = variant.map_or_else(
                || outer.to_string(),
                |variant| format!("{outer}::{variant}"),
            );
            let mut inner = name.clone();
            let mut offset = 0;
            while self.peek_kind_at(offset) == &TokenKind::ColonColon {
                if let TokenKind::Ident(segment) = self.peek_kind_at(offset + 1) {
                    inner.push_str("::");
                    inner.push_str(segment);
                    offset += 2;
                } else {
                    break;
                }
            }
            return Err(Diagnostic::new(
                crate::diagnostics::Severity::Error,
                ErrorCode::E0102,
                "nested patterns are not supported; bind and match again",
            )
            .with_label(crate::diagnostics::Label::primary(
                start,
                "nested constructor pattern",
            ))
            .with_help(format!("bind the payload of `{outer}` to a local variable, then match that variable against `{inner}` in the arm body")));
        }
        Ok(name)
    }

    // --- helpers ---
}

#[cfg(test)]
mod tests {
    use crate::{lexer::Lexer, parser::Parser};
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
                assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async)));
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
            let source = format!("fn f() {{ match value {{ Value({prefix}-42) => 1, }} }}");
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

    #[test]
    fn literal_payload_diagnostic_perspectives() {
        for outer in [
            "Condition::Fracture",
            "Fracture",
            "pkg::Condition::Fracture",
            "Value",
        ] {
            for literal in [
                "true", "false", "0", "42", "-42", "1.5", "-1.5", "\"text\"", "nil",
            ] {
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
                        error.message.contains("literal patterns"),
                        "{source}: {error:?}"
                    );
                    assert!(!error.message.contains("reserved keyword"));
                    assert!(error.helps.iter().all(|help| !help.contains("rename")));
                    assert!(error.helps[0].contains("bind the payload"));
                    assert!(error.helps[0].contains("match guard"));
                    assert!(error.helps[0].contains("Fracture(o) if o"));
                    assert_eq!(
                        error.primary_span().unwrap().start,
                        source.find(&format!("{literal})")).unwrap()
                    );
                    assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async)));
                }
            }
        }
    }

    #[test]
    fn nested_pattern_twenty_diagnostic_perspectives() {
        for outer in ["Result::Err", "Err", "pkg::Result::Err", "Wrap::Value"] {
            for inner in [
                "E::Bad(n)",
                "E::Bad(_)",
                "Bad(n)",
                "E::Empty",
                "E::Bad(E::Bad(n))",
            ] {
                let source = format!(
                    "fn f() {{ match value {{\n{outer}({inner}) => println(1),\n}} }}\nasync fn main() {{ await sleep(0); }}"
                );
                let (program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(program.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(f) if f.name == "main" && f.is_async)), "{outer}/{inner}: following async modifier was lost");
                assert!(
                    errors[0]
                        .message
                        .contains("nested patterns are not supported; bind and match again"),
                    "{outer}/{inner}: {errors:?}"
                );
                assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                assert!(errors[0].helps[0].contains(outer));
                assert!(errors[0].helps[0].contains(inner.split('(').next().unwrap()));
                assert_eq!(errors[0].primary_span().unwrap().line, 2);
                assert!(
                    errors
                        .windows(2)
                        .all(|pair| pair[0].primary_span().unwrap().start
                            <= pair[1].primary_span().unwrap().start)
                );
            }
        }
    }
}

use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Span};
use crate::lexer::token::TokenKind;

impl Parser {
    pub(super) fn parse_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        let span = self.current_span();
        match self.peek_kind().clone() {
            TokenKind::Ident(ref name) if name == "_" => {
                self.advance();
                Ok(Pattern::Wildcard(span))
            }
            TokenKind::True => {
                self.advance();
                Ok(Pattern::LiteralBool(true, span))
            }
            TokenKind::False => {
                self.advance();
                Ok(Pattern::LiteralBool(false, span))
            }
            TokenKind::Integer(n) => {
                self.advance();
                Ok(Pattern::LiteralInt(n, span))
            }
            TokenKind::Minus => {
                self.advance();
                if let TokenKind::Integer(n) = self.peek_kind().clone() {
                    let end = self.current_span();
                    self.advance();
                    let merged = Span::new(span.start, end.end, span.line, span.col);
                    Ok(Pattern::LiteralInt(-n, merged))
                } else {
                    Err(self.err(ErrorCode::E0102, "expected integer after '-' in pattern"))
                }
            }
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.advance();
                if matches!(self.peek_kind(), TokenKind::ColonColon) {
                    self.advance(); // consume ::
                    // Collect all `::`-separated segments; the last is the
                    // variant, the rest (joined) form the enum name. This
                    // accepts a module-qualified enum, e.g. `palette::Color::Red`
                    // (enum `palette::Color`, variant `Red`) (willow-64gs).
                    let mut segments = vec![name, self.expect_ident()?];
                    while self.eat(TokenKind::ColonColon) {
                        segments.push(self.expect_ident()?);
                    }
                    let variant = segments.pop().unwrap();
                    let name = segments.join("::");
                    if matches!(self.peek_kind(), TokenKind::LParen) {
                        self.advance(); // consume (
                        let mut bindings = Vec::new();
                        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                            bindings.push(self.expect_ident()?);
                            if matches!(self.peek_kind(), TokenKind::Comma) {
                                self.advance();
                            }
                        }
                        self.expect(TokenKind::RParen)?;
                        let end = self.current_span();
                        let merged = Span::new(span.start, end.end, span.line, span.col);
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: name,
                            variant,
                            bindings,
                            span: merged,
                        })
                    } else {
                        let end = self.current_span();
                        let merged = Span::new(span.start, end.end, span.line, span.col);
                        Ok(Pattern::EnumVariant {
                            enum_name: name,
                            variant,
                            span: merged,
                        })
                    }
                } else if matches!(self.peek_kind(), TokenKind::LParen) {
                    // `Dog(d)` — interface->concrete downcast pattern (willow-1js.4).
                    // (Enum variants are always `::`-qualified, so an unqualified
                    // name with `(` is unambiguously a class downcast.)
                    self.advance(); // consume (
                    let binding = self.expect_ident()?; // `_` lexes as an identifier
                    self.expect(TokenKind::RParen)?;
                    let end = self.current_span();
                    let merged = Span::new(span.start, end.end, span.line, span.col);
                    Ok(Pattern::ClassDowncast {
                        class_name: name,
                        binding,
                        span: merged,
                    })
                } else {
                    Ok(Pattern::Binding { name, span })
                }
            }
            _ => Err(self.err(ErrorCode::E0102, "expected pattern")),
        }
    }

    // --- helpers ---
}

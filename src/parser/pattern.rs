use super::Parser;
use super::ast::*;
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
                        let merged = span.to(end);
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: name,
                            variant,
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
                            span: merged,
                            id: PatternId::fresh(),
                        })
                    }
                } else if matches!(self.peek_kind(), TokenKind::LParen) {
                    // One binding remains ambiguous with a class downcast;
                    // zero or multiple bindings can only be an enum pattern.
                    self.advance();
                    let mut bindings = Vec::new();
                    if !matches!(self.peek_kind(), TokenKind::RParen) {
                        loop {
                            bindings.push(self.expect_ident()?);
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

    // --- helpers ---
}

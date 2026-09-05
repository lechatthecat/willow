use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode};
use crate::lexer::token::TokenKind;

impl Parser {
    pub(super) fn parse_type(&mut self) -> Result<Type, Diagnostic> {
        let mut ty = match self.peek_kind().clone() {
            TokenKind::I64 => {
                self.advance();
                Ok(Type::I64)
            }
            TokenKind::F64 => {
                self.advance();
                Ok(Type::F64)
            }
            TokenKind::Bool => {
                self.advance();
                Ok(Type::Bool)
            }
            // `closure(T1, T2) -> R` — the GC-managed callable
            // (willow-0g8j.2.12). Spelled as an identifier rather than a
            // keyword so that no existing program loses the name `closure`;
            // only an identifier followed directly by `(` in TYPE position is
            // read this way, and no type may be called.
            TokenKind::Ident(name)
                if name == "closure" && self.peek_kind_at(1) == &TokenKind::LParen =>
            {
                self.advance();
                let (params, ret) = self.parse_callable_type_tail()?;
                Ok(Type::Closure(params, Box::new(ret)))
            }
            TokenKind::Ident(name) => {
                self.advance();
                let mut parts = vec![name];
                while self.eat(TokenKind::ColonColon) {
                    parts.push(self.expect_ident()?);
                }
                let name = parts.join("::");
                if self.eat(TokenKind::Lt) {
                    let mut args = Vec::new();
                    while !self.check(TokenKind::Gt) && !self.at_eof() {
                        args.push(self.parse_type()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::Gt)?;
                    if name == "Array" && args.len() == 1 {
                        Ok(Type::Array(Box::new(args.remove(0))))
                    } else if name == "JoinHandle" && args.len() == 1 {
                        // `JoinHandle<T>` is the legacy spelling of the handle an
                        // async call already returns; it is one type, not two, so
                        // normalize it here rather than teaching every later stage
                        // to accept both (willow-qrj9). Without this, no expression
                        // in the language can produce a `JoinHandle<T>`, and the
                        // E0812 migration help that says to `await task` would name
                        // a type users could not obtain.
                        Ok(Type::Generic("Task".to_string(), args))
                    } else {
                        Ok(Type::Generic(name, args))
                    }
                } else if name == "String" {
                    Ok(Type::String)
                } else if name == "void" {
                    // `void` is a writable spelling of the unit/no-value type
                    // (e.g. `fn f() -> void`, `Result<void, E>`).
                    Ok(Type::Void)
                } else {
                    Ok(Type::Named(name))
                }
            }
            // `fn(T1, T2) -> R` — function pointer type
            TokenKind::Fn => {
                self.advance();
                let (params, ret) = self.parse_callable_type_tail()?;
                Ok(Type::Fn(params, Box::new(ret)))
            }
            _ => Err(self.err(
                ErrorCode::E0107,
                "expected type (`i64`, `f64`, `bool`, `fn(...)`, `closure(...)`, or type name)",
            )),
        }?;

        // `T?` is parser-only sugar for `Option<T>`. Repetition is
        // compositional: `T??` means `Option<Option<T>>`, not a collapsed
        // separate optional type (willow-glaj.4).
        while self.eat(TokenKind::Question) {
            ty = Type::Generic("Option".to_string(), vec![ty]);
        }
        Ok(ty)
    }

    /// `(T1, T2) -> R` after the `fn` or `closure` head. A missing `-> R` is
    /// `void`, which is what both callable types mean by an absent return.
    fn parse_callable_type_tail(&mut self) -> Result<(Vec<Type>, Type), Diagnostic> {
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        while !self.check(TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_type()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        let ret = if self.eat(TokenKind::Arrow) {
            self.parse_type()?
        } else {
            Type::Void
        };
        Ok((params, ret))
    }
}

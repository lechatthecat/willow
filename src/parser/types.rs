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
                    if self.eat(TokenKind::Lt) {
                        if !self.check(TokenKind::Gt) && !self.at_eof() {
                            frames.push(Frame::Generic(name, Vec::new()));
                            continue;
                        }
                        self.expect(TokenKind::Gt)?;
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
                    "expected type (`i64`, `f64`, `bool`, `fn(...)`, `closure(...)`, or type name)",
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
                        if self.eat(TokenKind::Comma)
                            && !self.check(TokenKind::Gt)
                            && !self.at_eof()
                        {
                            frames.push(Frame::Generic(name, args));
                            break;
                        }
                        self.expect(TokenKind::Gt)?;
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

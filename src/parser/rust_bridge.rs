use super::{Parser, ast::*};
use crate::{
    diagnostics::{Diagnostic, ErrorCode},
    lexer::token::TokenKind,
    rust_bridge::{RustBridgeSymbol, Scalar},
};
impl Parser {
    pub(super) fn parse_rust_bridge(&mut self) -> Result<Vec<Item>, Diagnostic> {
        self.advance();
        if self.expect_ident()? != "rust" {
            return Err(self.err(ErrorCode::E0102, "expected `extern rust`"));
        }
        let namespace = if self.check(TokenKind::LBrace) {
            None
        } else {
            Some(self.expect_ident()?)
        };
        if namespace.as_deref() == Some("std") {
            return Err(self.err(ErrorCode::E0102, "`std` is a reserved namespace"));
        }
        self.expect(TokenKind::LBrace)?;
        let mut items = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let span = self.current_span();
            self.expect(TokenKind::Fn)?;
            let name = self.expect_ident()?;
            if name == "main" {
                return Err(self.err(ErrorCode::E0102, "Rust bridge cannot declare main"));
            }
            let name = namespace
                .as_ref()
                .map_or(name.clone(), |ns| format!("{ns}::{name}"));
            self.expect(TokenKind::LParen)?;
            let mut params = Vec::new();
            let mut inputs = Vec::new();
            while !self.check(TokenKind::RParen) && !self.at_eof() {
                let param = self.parse_param()?;
                let ty = Scalar::from_type(&param.ty).filter(|ty| *ty != Scalar::Void);
                if ty.is_none() || !matches!(param.mode, ParamMode::Value) {
                    return Err(self.err(ErrorCode::E0102, "rust_bridge_signature_mismatch: unsupported value parameter (expected scalar, String, Array<i64>, Option or Result)"));
                }
                inputs.push(ty.unwrap());
                params.push(param);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen)?;
            let return_type = if self.eat(TokenKind::Arrow) {
                self.parse_type()?
            } else {
                Type::Void
            };
            let output = Scalar::from_type(&return_type).ok_or_else(|| {
                self.err(
                    ErrorCode::E0102,
                    "rust_bridge_signature_mismatch: unsupported return type (expected scalar, String, Array<i64>, Option or Result)",
                )
            })?;
            self.expect(TokenKind::Semicolon)?;
            let span = span.to(self.previous_span());
            items.push(Item::Function(FunctionDecl {
                rust_bridge: Some(RustBridgeSymbol::new(name.clone(), inputs, output)),
                name,
                public: true,
                is_async: false,
                params,
                return_type,
                body: Block {
                    id: BodyId::fresh(),
                    stmts: vec![],
                    span,
                },
                span,
                constant: None,
            }));
        }
        self.expect(TokenKind::RBrace)?;
        Ok(items)
    }
}

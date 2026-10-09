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
        let mut opaque = std::collections::HashSet::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let span = self.current_span();
            if matches!(self.peek_kind(), TokenKind::Ident(word) if word == "opaque") {
                self.advance();
                let name = self.expect_ident()?;
                self.expect(TokenKind::Semicolon)?;
                if matches!(
                    name.as_str(),
                    "String" | "Array" | "Option" | "Result" | "i64" | "f64" | "bool" | "void"
                ) {
                    return Err(self.err(
                        ErrorCode::E0102,
                        "opaque name conflicts with a bridge value type",
                    ));
                }
                if !opaque.insert(name.clone()) {
                    return Err(self.err(ErrorCode::E0102, "duplicate opaque declaration"));
                }
                // A private, unspellable variant reuses nominal enum layout. Its
                // sole payload is an integer ID, never a native Rust pointer.
                items.push(Item::Enum(EnumDecl {
                    name: name.clone(),
                    public: true,
                    type_params: vec![],
                    variants: vec![EnumVariant {
                        name: "$rust_handle".into(),
                        payload: vec![Type::I64],
                        span,
                    }],
                    span,
                }));
                let close_name = namespace.as_ref().map_or_else(
                    || format!("{name}_close"),
                    |ns| format!("{ns}::{name}_close"),
                );
                let mut symbol = RustBridgeSymbol::new(
                    close_name.clone(),
                    vec![Scalar::Opaque(name.clone())],
                    Scalar::Void,
                );
                symbol.close_handle = true;
                items.push(Item::Function(FunctionDecl {
                    name: close_name,
                    public: true,
                    is_async: false,
                    params: vec![Param {
                        name: "handle".into(),
                        ty: Type::Named(name),
                        mode: ParamMode::Value,
                        span,
                        type_span: span,
                    }],
                    return_type: Type::Void,
                    body: Block {
                        id: BodyId::fresh(),
                        stmts: vec![],
                        span,
                    },
                    span,
                    constant: None,
                    rust_bridge: Some(symbol),
                }));
                continue;
            }
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
                let ty = Scalar::from_type_with_opaque(&param.ty, &self.rust_opaque_names)
                    .filter(|ty| *ty != Scalar::Void);
                if ty.is_none() || !matches!(param.mode, ParamMode::Value) {
                    return Err(self.err(ErrorCode::E0102, "rust_bridge_signature_mismatch: unsupported value parameter (expected scalar, String, Array<i64>, opaque, Option or Result)"));
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
            let output = Scalar::from_type_with_opaque(&return_type, &self.rust_opaque_names).ok_or_else(|| {
                self.err(
                    ErrorCode::E0102,
                    "rust_bridge_signature_mismatch: unsupported return type (expected scalar, String, Array<i64>, opaque, Option or Result)",
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

// One source-wide pass permits forward references and multiple extern blocks.
// O(tokens), without rescanning the source for each declaration or type use.
pub(super) fn opaque_names(
    tokens: &[crate::lexer::token::Token],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut in_rust = false;
    for window in tokens.windows(3) {
        if matches!((&window[0].kind, &window[1].kind), (TokenKind::Ident(a), TokenKind::Ident(b)) if a == "extern" && b == "rust")
        {
            in_rust = true;
        }
        if matches!(window[0].kind, TokenKind::RBrace) {
            in_rust = false;
        }
        if in_rust
            && let (TokenKind::Ident(keyword), TokenKind::Ident(name), TokenKind::Semicolon) =
                (&window[0].kind, &window[1].kind, &window[2].kind)
            && keyword == "opaque"
        {
            names.insert(name.clone());
        }
    }
    names
}

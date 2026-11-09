//! Parsing and opaque-name discovery for `extern rust` declarations.
use super::{Parser, ast::*};
use crate::{
    diagnostics::{Diagnostic, ErrorCode},
    lexer::token::TokenKind,
    rust_bridge_ir::{RustBridgeSymbol, Scalar},
};
impl Parser {
    fn validate_rust_record_identifier(&self, name: &str, role: &str) -> Result<(), Diagnostic> {
        // These spellings cannot use Rust's r# escape. Other Rust keywords can.
        if matches!(name, "crate" | "self" | "Self" | "super" | "_") {
            return Err(self.err(
                ErrorCode::E0102,
                format!("rust_bridge_signature_mismatch: {role} name `{name}` cannot be a Rust raw identifier"),
            ));
        }
        Ok(())
    }

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
            if self.check(TokenKind::Class) {
                let record = self.parse_class(true, false)?;
                self.validate_rust_record_identifier(&record.name, "record")?;
                if !record.type_params.is_empty()
                    || record.base_class.is_some()
                    || !record.implements.is_empty()
                    || !record.methods.is_empty()
                    || !record.constructors.is_empty()
                    || record
                        .fields
                        .iter()
                        .any(|f| !f.public || f.is_static || f.initializer.is_some())
                {
                    return Err(self.err(
                        ErrorCode::E0102,
                        "rust_bridge_signature_mismatch: records require public value fields only",
                    ));
                }
                if self.rust_opaque_names.contains(&record.name)
                    || self.rust_records.contains_key(&record.name)
                    || matches!(
                        record.name.as_str(),
                        "String" | "i64" | "f64" | "bool" | "void"
                    )
                    || crate::semantic::builtin_types::BuiltinTypeId::from_name(&record.name)
                        .is_some()
                {
                    return Err(self.err(
                        ErrorCode::E0102,
                        "rust_bridge_signature_mismatch: duplicate or reserved record name",
                    ));
                }
                let mut fields = Vec::new();
                let mut names = std::collections::HashSet::new();
                for field in &record.fields {
                    self.validate_rust_record_identifier(&field.name, "record field")?;
                    let ty = Scalar::from_type_with_records(&field.ty, &self.rust_opaque_names, &self.rust_records)
                        .filter(|ty| *ty != Scalar::Void)
                        .ok_or_else(|| self.err(ErrorCode::E0102, "rust_bridge_signature_mismatch: unsupported record field (declare nested records first)"))?;
                    if !names.insert(&field.name) {
                        return Err(self.err(
                            ErrorCode::E0102,
                            "rust_bridge_signature_mismatch: duplicate record field",
                        ));
                    }
                    fields.push((field.name.clone(), ty));
                }
                self.rust_records.insert(
                    record.name.clone(),
                    crate::rust_bridge_ir::Record {
                        name: record.name.clone(),
                        fields,
                    },
                );
                items.push(Item::Class(record));
                continue;
            }
            let nonblocking = if matches!(self.peek_kind(), TokenKind::Ident(word) if word == "nonblocking")
            {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let TokenKind::StringLiteral(fingerprint) = self.peek_kind().clone() else {
                    return Err(self.err(
                        ErrorCode::E0102,
                        "nonblocking requires a review fingerprint string",
                    ));
                };
                if fingerprint.len() != 64
                    || !fingerprint
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    return Err(self.err(
                        ErrorCode::E0102,
                        "nonblocking requires a lowercase SHA-256 review fingerprint",
                    ));
                }
                self.advance();
                self.expect(TokenKind::RParen)?;
                Some(fingerprint)
            } else {
                None
            };
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
                if let Some(fingerprint) = nonblocking {
                    symbol.declare_nonblocking(fingerprint);
                }
                items.push(Item::Function(FunctionDecl {
                    requires: Vec::new(),
                    ensures: Vec::new(),
                    effect_contracts: Vec::new(),
                    return_type_span: None,
                    type_params: Vec::new(),
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
                    rust_bridge: Some(Box::new(symbol)),
                }));
                continue;
            }
            let is_async = self.eat(TokenKind::Async);
            if is_async && nonblocking.is_some() {
                return Err(self.err(
                    ErrorCode::E0102,
                    "async Rust bridge cannot declare nonblocking",
                ));
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
                let ty = Scalar::from_type_with_records(
                    &param.ty,
                    &self.rust_opaque_names,
                    &self.rust_records,
                )
                .filter(|ty| *ty != Scalar::Void);
                if ty.is_none() || !matches!(param.mode, ParamMode::Value) {
                    return Err(self.err(ErrorCode::E0102, "rust_bridge_signature_mismatch: unsupported value parameter (expected scalar, String, scalar/String Array, declared record, opaque, Option or Result)"));
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
            let output = Scalar::from_type_with_records(&return_type, &self.rust_opaque_names, &self.rust_records).ok_or_else(|| {
                self.err(
                    ErrorCode::E0102,
                    "rust_bridge_signature_mismatch: unsupported return type (expected scalar, String, scalar/String Array, declared record, opaque, Option or Result)",
                )
            })?;
            self.expect(TokenKind::Semicolon)?;
            let span = span.to(self.previous_span());
            let mut symbol = RustBridgeSymbol::new(name.clone(), inputs, output);
            symbol.is_async = is_async;
            if is_async {
                symbol.effects &= !willow_abi::RuntimeEffects::MAY_BLOCK.bits();
                symbol.effects |= willow_abi::RuntimeEffects::MAY_SUSPEND.bits();
            }
            symbol.resolve_records(&self.rust_records);
            if let Some(fingerprint) = nonblocking {
                symbol.declare_nonblocking(fingerprint);
            }
            items.push(Item::Function(FunctionDecl {
                requires: Vec::new(),
                ensures: Vec::new(),
                effect_contracts: Vec::new(),
                return_type_span: None,
                type_params: Vec::new(),
                rust_bridge: Some(Box::new(symbol)),
                name,
                public: true,
                is_async,
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
    let mut rust_depth = None;
    let mut depth = 0usize;
    for window in tokens.windows(3) {
        if matches!((&window[0].kind, &window[1].kind), (TokenKind::Ident(a), TokenKind::Ident(b)) if a == "extern" && b == "rust")
        {
            rust_depth = Some(depth + 1);
        }
        if matches!(window[0].kind, TokenKind::LBrace) {
            depth += 1;
        }
        if matches!(window[0].kind, TokenKind::RBrace) {
            if rust_depth == Some(depth) {
                rust_depth = None;
            }
            depth = depth.saturating_sub(1);
        }
        if rust_depth == Some(depth)
            && let (TokenKind::Ident(keyword), TokenKind::Ident(name), TokenKind::Semicolon) =
                (&window[0].kind, &window[1].kind, &window[2].kind)
            && keyword == "opaque"
        {
            names.insert(name.clone());
        }
    }
    names
}

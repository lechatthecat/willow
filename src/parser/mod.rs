pub mod ast;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::{Token, TokenKind};
use ast::*;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Parse the token stream. Returns the (possibly partial) program and any diagnostics.
    /// Callers check `diagnostics.is_empty()` to know if parsing succeeded.
    /// Items that failed to parse are omitted from the returned program; successfully
    /// parsed items are always included so downstream stages can report more errors.
    pub fn parse(&mut self) -> (Program, Vec<Diagnostic>) {
        let mut imports = Vec::new();
        let mut items = Vec::new();
        let mut errors = Vec::new();

        // Imports must come before any items.
        while !self.at_eof() && matches!(self.peek_kind(), TokenKind::Import) {
            match self.parse_import() {
                Ok(decl) => imports.push(decl),
                Err(e) => {
                    errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }

        while !self.at_eof() {
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(e) => {
                    errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }

        (Program { imports, items }, errors)
    }

    fn parse_import(&mut self) -> Result<ImportDecl, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Import)?;
        let path = self.parse_import_path()?;
        let alias = if self.eat(TokenKind::As) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(TokenKind::Semicolon)?;
        Ok(ImportDecl { path, alias, span })
    }

    fn parse_import_path(&mut self) -> Result<String, Diagnostic> {
        let mut parts = vec![self.expect_ident()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_ident()?);
        }
        Ok(parts.join("::"))
    }

    fn parse_item(&mut self) -> Result<Item, Diagnostic> {
        let public = self.eat(TokenKind::Pub);
        let is_open = self.eat(TokenKind::Open);
        let is_async = self.eat(TokenKind::Async);
        match self.peek_kind() {
            TokenKind::Fn => Ok(Item::Function(self.parse_fn(public, is_async)?)),
            TokenKind::Class => Ok(Item::Class(self.parse_class(public, is_open)?)),
            TokenKind::Enum => Ok(Item::Enum(self.parse_enum_decl(public)?)),
            _ if is_async => Err(self.err(ErrorCode::E0105, "`async` can only be used on `fn`")),
            _ => Err(self.err(ErrorCode::E0105, "expected `fn`, `class`, or `enum`")),
        }
    }

    fn parse_enum_decl(&mut self, public: bool) -> Result<EnumDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let v_start = self.current_span();
            let v_name = self.expect_ident()?;
            let mut payload = Vec::new();
            if matches!(self.peek_kind(), TokenKind::LParen) {
                self.advance(); // consume (
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    payload.push(self.parse_type()?);
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.advance();
                    }
                }
                self.expect(TokenKind::RParen)?;
            }
            let v_end = self.current_span();
            let v_span = Span::new(v_start.start, v_end.end, v_start.line, v_start.col);
            variants.push(EnumVariant { name: v_name, payload, span: v_span });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.advance();
            }
        }
        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = Span::new(start.start, end.end, start.line, start.col);
        Ok(EnumDecl { name, public, variants, span })
    }

    fn parse_class(&mut self, public: bool, is_open: bool) -> Result<ClassDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Class)?;
        let name = self.expect_ident()?;

        let base_class = if self.eat(TokenKind::Extends) {
            Some(self.parse_type_path()?)
        } else {
            None
        };

        self.expect(TokenKind::LBrace)?;

        let mut fields = Vec::new();
        let mut methods = Vec::new();

        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let member_public = self.eat(TokenKind::Pub);
            let member_open = self.eat(TokenKind::Open);
            let member_override = self.eat(TokenKind::Override);
            let member_async = self.eat(TokenKind::Async);

            if self.check(TokenKind::Fn) {
                methods.push(self.parse_method(
                    member_public,
                    member_async,
                    member_open,
                    member_override,
                )?);
            } else {
                if member_open || member_override || member_async {
                    return Err(self.err(
                        ErrorCode::E0105,
                        "`open`, `override`, and `async` can only be used on methods",
                    ));
                }
                fields.push(self.parse_field(member_public)?);
            }
        }

        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = Span::new(start.start, end.end, start.line, start.col);

        Ok(ClassDecl {
            name,
            public,
            is_open,
            base_class,
            fields,
            methods,
            span,
        })
    }

    fn parse_type_path(&mut self) -> Result<TypePath, Diagnostic> {
        let first = self.expect_ident()?;
        if self.eat(TokenKind::ColonColon) {
            let mut parts = vec![first];
            parts.push(self.expect_ident()?);
            // allow more segments: a::b::C
            while self.eat(TokenKind::ColonColon) {
                parts.push(self.expect_ident()?);
            }
            Ok(TypePath::Qualified(parts))
        } else {
            Ok(TypePath::Local(first))
        }
    }

    fn parse_field(&mut self, public: bool) -> Result<FieldDecl, Diagnostic> {
        let span = self.current_span();
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(FieldDecl {
            name,
            ty,
            public,
            span,
        })
    }

    fn parse_method(
        &mut self,
        public: bool,
        is_async: bool,
        is_open: bool,
        is_override: bool,
    ) -> Result<MethodDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;

        let mut has_self = false;
        let mut params = Vec::new();

        if self.check(TokenKind::SelfKw) {
            has_self = true;
            self.advance();
            if self.eat(TokenKind::Comma) && !self.check(TokenKind::RParen) {
                // parse remaining params after self
                loop {
                    params.push(self.parse_param()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
            }
        } else {
            while !self.check(TokenKind::RParen) && !self.at_eof() {
                params.push(self.parse_param()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }

        self.expect(TokenKind::RParen)?;
        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type()?
        } else {
            Type::Void
        };
        let body = self.parse_block()?;
        let span = Span::new(start.start, body.span.end, start.line, start.col);

        Ok(MethodDecl {
            name,
            public,
            is_async,
            is_open,
            is_override,
            params,
            has_self,
            return_type,
            body,
            span,
        })
    }

    fn parse_fn(&mut self, public: bool, is_async: bool) -> Result<FunctionDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;

        let mut params = Vec::new();
        while !self.check(TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_param()?);
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

        let body = self.parse_block()?;
        let span = Span::new(start.start, body.span.end, start.line, start.col);
        Ok(FunctionDecl {
            name,
            public,
            is_async,
            params,
            return_type,
            body,
            span,
        })
    }

    fn parse_param(&mut self) -> Result<Param, Diagnostic> {
        let span = self.current_span();
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let mode = if self.check(TokenKind::Ampersand) {
            let ampersand_span = self.current_span();
            self.advance();
            let mut_span = if self.check(TokenKind::Mut) {
                let span = self.current_span();
                self.advance();
                Some(span)
            } else {
                None
            };
            ParamMode::Reference {
                mutable: mut_span.is_some(),
                ampersand_span,
                mut_span,
            }
        } else {
            ParamMode::Value
        };
        let type_start = self.current_span();
        let ty = self.parse_type()?;
        let type_end = self.previous_span();
        let type_span = Span::new(
            type_start.start,
            type_end.end,
            type_start.line,
            type_start.col,
        );
        Ok(Param {
            name,
            ty,
            mode,
            span,
            type_span,
        })
    }

    fn parse_type(&mut self) -> Result<Type, Diagnostic> {
        let ty = match self.peek_kind().clone() {
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
                    } else {
                        Ok(Type::Generic(name, args))
                    }
                } else if name == "String" {
                    Ok(Type::String)
                } else {
                    Ok(Type::Named(name))
                }
            }
            // `fn(T1, T2) -> R` — function pointer type
            TokenKind::Fn => {
                self.advance();
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
                Ok(Type::Fn(params, Box::new(ret)))
            }
            _ => Err(self.err(
                ErrorCode::E0107,
                "expected type (`i64`, `f64`, `bool`, `fn(...)`, or type name)",
            )),
        }?;

        if self.eat(TokenKind::Question) {
            Ok(Type::Nullable(Box::new(ty)))
        } else {
            Ok(ty)
        }
    }

    fn parse_block(&mut self) -> Result<Block, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            stmts.push(self.parse_stmt()?);
        }
        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        Ok(Block {
            stmts,
            span: Span::new(start.start, end.end, start.line, start.col),
        })
    }

    fn parse_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        match self.peek_kind().clone() {
            TokenKind::Let => self.parse_let(),
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Ident(name) if self.is_assign_ahead() => self.parse_assign(name),
            _ => self.parse_expr_stmt(),
        }
    }

    fn is_assign_ahead(&self) -> bool {
        matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind), Some(TokenKind::Eq))
        // not ==
        && !matches!(self.tokens.get(self.pos + 2).map(|t| &t.kind), Some(TokenKind::Eq))
    }

    fn parse_let(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Let)?;
        let mutable = self.eat(TokenKind::Mut);
        let name = self.expect_ident()?;
        let ty = if self.eat(TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(TokenKind::Eq)?;
        let init = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Let(LetStmt {
            name,
            mutable,
            ty,
            init,
            span,
        }))
    }

    fn parse_assign(&mut self, name: String) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.advance(); // consume ident
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Assign(AssignStmt { name, value, span }))
    }

    fn parse_if(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::If)?;
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let else_block = if self.eat(TokenKind::Else) {
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok(Stmt::If(IfStmt {
            cond,
            then_block,
            else_block,
            span,
        }))
    }

    fn parse_while(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::While(WhileStmt { cond, body, span }))
    }

    fn parse_return(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Return)?;
        let value = if !self.check(TokenKind::Semicolon) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Return(ReturnStmt { value, span }))
    }

    fn parse_expr_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        let expr = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Expr(ExprStmt { expr, span }))
    }

    fn parse_expr(&mut self) -> Result<Expr, Diagnostic> {
        self.parse_ternary()
    }

    // condition ? then_expr : else_expr  (right-associative, lower than ||)
    fn parse_ternary(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();
        let cond = self.parse_or()?;
        if !self.eat(TokenKind::Question) {
            return Ok(cond);
        }
        let then_expr = self.parse_ternary()?; // right-associative: recurse for then
        if !self.eat(TokenKind::Colon) {
            return Err(self
                .err(ErrorCode::E0903, "expected `:` in ternary expression")
                .with_help("write the ternary as `condition ? then_value : else_value`"));
        }
        let else_expr = self.parse_ternary()?; // right-associative: recurse for else
        let end = else_expr.span();
        let span = Span::new(span.start, end.end, span.line, span.col);
        Ok(Expr::Ternary(Box::new(TernaryExpr {
            condition: cond,
            then_expr,
            else_expr,
            span,
        })))
    }

    fn parse_or(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_and()?;
        while self.check(TokenKind::Or) {
            let span = self.current_span();
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op: BinOp::Or,
                lhs,
                rhs,
                span,
            }));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_cmp()?;
        while self.check(TokenKind::And) {
            let span = self.current_span();
            self.advance();
            let rhs = self.parse_cmp()?;
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op: BinOp::And,
                lhs,
                rhs,
                span,
            }));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::BangEq => BinOp::Ne,
                TokenKind::Lt => BinOp::Lt,
                TokenKind::LtEq => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::GtEq => BinOp::Ge,
                _ => break,
            };
            let span = self.current_span();
            self.advance();
            let rhs = self.parse_add()?;
            lhs = Expr::Binary(Box::new(BinaryExpr { op, lhs, rhs, span }));
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            let span = self.current_span();
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = Expr::Binary(Box::new(BinaryExpr { op, lhs, rhs, span }));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Rem,
                _ => break,
            };
            let span = self.current_span();
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(Box::new(BinaryExpr { op, lhs, rhs, span }));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, Diagnostic> {
        match self.peek_kind().clone() {
            TokenKind::Minus => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_postfix()?;
                Ok(Expr::Unary(Box::new(UnaryExpr {
                    op: UnaryOp::Neg,
                    expr,
                    span,
                })))
            }
            TokenKind::Bang => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_postfix()?;
                Ok(Expr::Unary(Box::new(UnaryExpr {
                    op: UnaryOp::Not,
                    expr,
                    span,
                })))
            }
            TokenKind::Await => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::Await(Box::new(AwaitExpr { expr, span })))
            }
            _ => self.parse_postfix(),
        }
    }

    /// Parse a primary expression then consume any postfix `.field` / `.method(args)` chains.
    fn parse_postfix(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_primary()?;
        loop {
            if self.eat(TokenKind::Dot) {
                let span = self.current_span();
                let member = self.expect_ident()?;
                if self.eat(TokenKind::LParen) {
                    let args = self.parse_call_args_after_lparen()?;
                    lhs = Expr::MethodCall(Box::new(MethodCallExpr {
                        object: lhs,
                        method: member,
                        args,
                        span,
                    }));
                } else {
                    lhs = Expr::FieldAccess(Box::new(lhs), member, span);
                }
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self) -> Result<Expr, Diagnostic> {
        match self.peek_kind().clone() {
            TokenKind::Integer(n) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Integer(n, span))
            }
            TokenKind::Float(f) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Float(f, span))
            }
            TokenKind::True => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Bool(true, span))
            }
            TokenKind::False => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Bool(false, span))
            }
            TokenKind::Nil => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Nil(span))
            }
            TokenKind::StringLiteral(value) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::String(value, span))
            }
            TokenKind::Print => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::LParen)?;
                let arg = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(Expr::Print(Box::new(arg), false, span))
            }
            TokenKind::Println => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::LParen)?;
                let arg = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(Expr::Print(Box::new(arg), true, span))
            }
            TokenKind::SelfKw => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Var("self".to_string(), span))
            }
            TokenKind::Spawn => self.parse_spawn(),
            TokenKind::Select => self.parse_select(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::F64 => {
                let span = self.current_span();
                self.advance();
                if self.eat(TokenKind::ColonColon) {
                    self.parse_static_call("f64".to_string(), span)
                } else {
                    Err(self.err(
                        ErrorCode::E0102,
                        "expected `::` after type name in expression",
                    ))
                }
            }
            TokenKind::Ident(name) => {
                let span = self.current_span();
                self.advance();
                if let Some(expr) = self.try_parse_generic_static_call(name.clone(), span)? {
                    Ok(expr)
                } else if self.eat(TokenKind::ColonColon) {
                    let member = self.expect_ident()?;
                    if is_type_constructor_name(&member) && self.eat(TokenKind::LBrace) {
                        self.parse_object_literal_fields(format!("{name}::{member}"), span)
                    } else if is_type_constructor_name(&member)
                        && !matches!(self.peek_kind(), TokenKind::LParen)
                    {
                        // Enum variant used as a value (no args): e.g. `Color::Red`
                        Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                            class: name,
                            type_args: vec![],
                            method: member,
                            args: vec![],
                            span,
                        })))
                    } else {
                        self.expect(TokenKind::LParen)?;
                        let args = self.parse_call_args_after_lparen()?;
                        Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                            class: name,
                            type_args: vec![],
                            method: member,
                            args,
                            span,
                        })))
                    }
                } else if self.eat(TokenKind::LParen) {
                    let args = self.parse_call_args_after_lparen()?;
                    Ok(Expr::Call(Box::new(CallExpr {
                        callee: name,
                        args,
                        span,
                    })))
                } else if is_type_constructor_name(&name) && self.eat(TokenKind::LBrace) {
                    self.parse_object_literal_fields(name, span)
                } else {
                    Ok(Expr::Var(name, span))
                }
            }
            TokenKind::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(expr)
            }
            // Lambda: `|params| expr` or `|params| { block }`
            TokenKind::Pipe => self.parse_lambda(),
            // Zero-param lambda: `|| expr` or `|| { block }`
            TokenKind::Or => self.parse_lambda(),
            TokenKind::Ampersand => {
                Err(self.err(ErrorCode::E0102, "`&` is only valid before a call argument"))
            }
            _ => Err(self.err(ErrorCode::E0102, "expected expression")),
        }
    }

    fn parse_static_call(&mut self, class: String, span: Span) -> Result<Expr, Diagnostic> {
        let method = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Expr::StaticCall(Box::new(StaticCallExpr {
            class,
            type_args: vec![],
            method,
            args,
            span,
        })))
    }

    fn try_parse_generic_static_call(
        &mut self,
        class: String,
        span: Span,
    ) -> Result<Option<Expr>, Diagnostic> {
        if !self.check(TokenKind::Lt) {
            return Ok(None);
        }

        let saved = self.pos;
        self.advance();
        let mut type_args = Vec::new();
        while !self.check(TokenKind::Gt) && !self.at_eof() {
            match self.parse_type() {
                Ok(ty) => type_args.push(ty),
                Err(_) => {
                    self.pos = saved;
                    return Ok(None);
                }
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }

        if !self.eat(TokenKind::Gt) || !self.eat(TokenKind::ColonColon) {
            self.pos = saved;
            return Ok(None);
        }

        let method = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Some(Expr::StaticCall(Box::new(StaticCallExpr {
            class,
            type_args,
            method,
            args,
            span,
        }))))
    }

    fn parse_call_args_after_lparen(&mut self) -> Result<Vec<CallArg>, Diagnostic> {
        let mut args = Vec::new();
        while !self.check(TokenKind::RParen) && !self.at_eof() {
            args.push(self.parse_call_arg()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok(args)
    }

    fn parse_call_arg(&mut self) -> Result<CallArg, Diagnostic> {
        if self.check(TokenKind::Ampersand) {
            let ampersand_span = self.current_span();
            self.advance();
            let expr = self.parse_expr()?;
            let expr_span = expr.span();
            return Ok(CallArg {
                expr,
                mode: CallArgMode::Reference { ampersand_span },
                span: Span::new(
                    ampersand_span.start,
                    expr_span.end,
                    ampersand_span.line,
                    ampersand_span.col,
                ),
            });
        }

        Ok(CallArg::value(self.parse_expr()?))
    }

    fn parse_object_literal_fields(
        &mut self,
        class: String,
        span: Span,
    ) -> Result<Expr, Diagnostic> {
        let mut fields = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let field_span = self.current_span();
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let value = self.parse_expr()?;
            fields.push(ObjectLiteralField {
                name,
                value,
                span: field_span,
            });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::ObjectLiteral(Box::new(ObjectLiteralExpr {
            class,
            fields,
            span,
        })))
    }

    fn parse_spawn(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Spawn)?;
        let callee = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Expr::Spawn(Box::new(SpawnExpr { callee, args, span })))
    }

    fn parse_select(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Select)?;
        self.expect(TokenKind::LBrace)?;

        let mut depth = 1usize;
        while depth > 0 && !self.at_eof() {
            match self.peek_kind() {
                TokenKind::LBrace => {
                    depth += 1;
                    self.advance();
                }
                TokenKind::RBrace => {
                    depth -= 1;
                    self.advance();
                }
                _ => self.advance(),
            }
        }

        if depth != 0 {
            return Err(self.err(ErrorCode::E0103, "expected `}` to close select block"));
        }

        Ok(Expr::Select(SelectExpr { span: start }))
    }

    fn parse_lambda(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();

        // Consume opening delimiter. `||` = zero-param lambda, `|` = params follow.
        let params = if self.eat(TokenKind::Or) {
            // `||` — zero params
            vec![]
        } else {
            // `|` — parse params until closing `|`
            self.expect(TokenKind::Pipe)?;
            let mut params = Vec::new();
            while !self.check(TokenKind::Pipe) && !self.at_eof() {
                let p_span = self.current_span();
                let name = self.expect_ident()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(LambdaParam {
                    name,
                    ty,
                    span: p_span,
                });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::Pipe)?;
            params
        };

        // Optional return type annotation: `-> R`
        let return_type = if self.eat(TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        // Body: `{ block }` or expression
        let body = if self.check(TokenKind::LBrace) {
            LambdaBody::Block(self.parse_block()?)
        } else {
            LambdaBody::Expr(Box::new(self.parse_expr()?))
        };

        Ok(Expr::Lambda(Box::new(LambdaExpr {
            params,
            return_type,
            body,
            span,
        })))
    }

    fn parse_match_expr(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Match)?;
        let scrutinee = self.parse_expr()?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let arm_start = self.current_span();
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::FatArrow)?;
            let body = if matches!(self.peek_kind(), TokenKind::LBrace) {
                let block = self.parse_block()?;
                MatchBody::Block(block)
            } else {
                let expr = self.parse_expr()?;
                MatchBody::Expr(Box::new(expr))
            };
            let arm_end = self.current_span();
            let arm_span = Span::new(arm_start.start, arm_end.end, arm_start.line, arm_start.col);
            arms.push(MatchArm { pattern, body, span: arm_span });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.advance();
            }
        }
        let end_span = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = Span::new(start.start, end_span.end, start.line, start.col);
        Ok(Expr::Match(Box::new(MatchExpr {
            scrutinee: Box::new(scrutinee),
            arms,
            span,
        })))
    }

    fn parse_pattern(&mut self) -> Result<Pattern, Diagnostic> {
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
                    let variant = self.expect_ident()?;
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
                        Ok(Pattern::EnumVariantTuple { enum_name: name, variant, bindings, span: merged })
                    } else {
                        let end = self.current_span();
                        let merged = Span::new(span.start, end.end, span.line, span.col);
                        Ok(Pattern::EnumVariant { enum_name: name, variant, span: merged })
                    }
                } else {
                    Ok(Pattern::Binding { name, span })
                }
            }
            _ => Err(self.err(ErrorCode::E0102, "expected pattern")),
        }
    }

    // --- helpers ---

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn current_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn previous_span(&self) -> Span {
        if self.pos == 0 {
            self.current_span()
        } else {
            self.tokens[self.pos - 1].span
        }
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.tokens[self.pos].kind, TokenKind::Eof)
    }

    fn check(&self, kind: TokenKind) -> bool {
        self.tokens[self.pos].kind == kind
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Span, Diagnostic> {
        if self.check(kind.clone()) {
            let span = self.current_span();
            self.advance();
            Ok(span)
        } else if self.check(TokenKind::Ampersand) {
            Err(self.err(ErrorCode::E0102, "`&` is only valid before a call argument"))
        } else {
            let (code, msg) = token_expect_message(&kind);
            Err(self.err(code, msg))
        }
    }

    fn expect_ident(&mut self) -> Result<String, Diagnostic> {
        if let TokenKind::Ident(name) = self.peek_kind().clone() {
            self.advance();
            Ok(name)
        } else {
            Err(self.err(ErrorCode::E0102, "expected identifier"))
        }
    }

    fn err(&self, code: ErrorCode, msg: impl Into<String>) -> Diagnostic {
        let msg = msg.into();
        let span = self.current_span();
        Diagnostic::new(Severity::Error, code, msg.clone()).with_label(Label::primary(span, msg))
    }

    fn recover_to_next_item(&mut self) {
        if !self.at_eof() {
            self.advance();
        }

        while !self.at_eof() {
            if matches!(
                self.peek_kind(),
                TokenKind::Fn | TokenKind::Class | TokenKind::Pub | TokenKind::Import | TokenKind::Enum
            ) {
                break;
            }
            self.advance();
        }
    }
}

fn is_type_constructor_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
}

fn token_expect_message(kind: &TokenKind) -> (ErrorCode, &'static str) {
    match kind {
        TokenKind::Semicolon => (ErrorCode::E0101, "expected `;` after statement"),
        TokenKind::LParen => (ErrorCode::E0102, "expected `(`"),
        TokenKind::RParen => (ErrorCode::E0104, "expected `)` to close parenthesis"),
        TokenKind::LBrace => (ErrorCode::E0102, "expected `{` to start block"),
        TokenKind::RBrace => (ErrorCode::E0103, "expected `}` to close block"),
        TokenKind::Colon => (ErrorCode::E0102, "expected `:` after parameter name"),
        TokenKind::Fn => (ErrorCode::E0105, "expected `fn`"),
        _ => (ErrorCode::E0102, "unexpected token"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse_ok(source: &str) -> Program {
        let tokens = Lexer::new(source).tokenize().expect("lexing failed");
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "parse errors: {errors:#?}");
        program
    }

    fn parse_errors(source: &str) -> Vec<Diagnostic> {
        let tokens = Lexer::new(source).tokenize().expect("lexing failed");
        let (_, errors) = Parser::new(tokens).parse();
        errors
    }

    fn first_function(program: &Program) -> &FunctionDecl {
        match &program.items[0] {
            Item::Function(function) => function,
            _ => panic!("expected first item to be a function"),
        }
    }

    fn function_named<'a>(program: &'a Program, name: &str) -> &'a FunctionDecl {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Function(function) if function.name == name => Some(function),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected function `{name}`"))
    }

    fn assert_reference_param(param: &Param, ty: Type, mutable: bool) {
        assert_eq!(param.ty, ty);
        assert!(matches!(
            &param.mode,
            ParamMode::Reference { mutable: actual, .. } if *actual == mutable
        ));
    }

    fn assert_reference_arg(arg: &CallArg) {
        assert!(matches!(&arg.mode, CallArgMode::Reference { .. }));
    }

    #[test]
    fn parses_mutable_reference_parameter_with_marker_and_type_spans() {
        let source = "fn bump(x: &mut i64, y: bool) {}";
        let program = parse_ok(source);
        let function = first_function(&program);

        assert_eq!(function.params.len(), 2);
        assert_eq!(function.params[0].name, "x");
        assert_eq!(function.params[0].ty, Type::I64);
        assert_eq!(
            &source[function.params[0].type_span.start..function.params[0].type_span.end],
            "i64"
        );
        match &function.params[0].mode {
            ParamMode::Reference {
                mutable,
                ampersand_span,
                mut_span,
            } => {
                assert!(*mutable);
                assert_eq!(&source[ampersand_span.start..ampersand_span.end], "&");
                let mut_span = mut_span.expect("expected mut span");
                assert_eq!(&source[mut_span.start..mut_span.end], "mut");
            }
            ParamMode::Value => panic!("expected first parameter to be a mutable reference"),
        }

        assert_eq!(function.params[1].name, "y");
        assert_eq!(function.params[1].ty, Type::Bool);
        assert!(matches!(&function.params[1].mode, ParamMode::Value));
    }

    #[test]
    fn parses_immutable_reference_method_parameter_after_self() {
        let source = "class Box { fn get(self, value: & String?) {} }";
        let program = parse_ok(source);
        let class = match &program.items[0] {
            Item::Class(class) => class,
            _ => panic!("expected first item to be a class"),
        };
        let method = &class.methods[0];

        assert!(method.has_self);
        assert_eq!(method.params.len(), 1);
        assert_eq!(method.params[0].name, "value");
        assert_eq!(
            &source[method.params[0].type_span.start..method.params[0].type_span.end],
            "String?"
        );
        assert!(matches!(
            &method.params[0].mode,
            ParamMode::Reference { mutable: false, .. }
        ));
    }

    #[test]
    fn parses_immutable_i64_reference_parameter() {
        let program = parse_ok("fn read(x: & i64) {}");
        let function = first_function(&program);

        assert_reference_param(&function.params[0], Type::I64, false);
    }

    #[test]
    fn parses_mutable_bool_reference_parameter() {
        let program = parse_ok("fn flip(x: &mut bool) {}");
        let function = first_function(&program);

        assert_reference_param(&function.params[0], Type::Bool, true);
    }

    #[test]
    fn parses_mutable_f64_reference_parameter() {
        let program = parse_ok("fn add(x: &mut f64) {}");
        let function = first_function(&program);

        assert_reference_param(&function.params[0], Type::F64, true);
    }

    #[test]
    fn parses_nullable_named_reference_parameter() {
        let program = parse_ok("fn visit(node: & Node?) {}");
        let function = first_function(&program);

        assert_reference_param(
            &function.params[0],
            Type::Nullable(Box::new(Type::Named("Node".to_string()))),
            false,
        );
    }

    #[test]
    fn parses_multiple_reference_parameters() {
        let program = parse_ok("fn mix(a: &mut i64, b: & bool, c: &mut f64) {}");
        let function = first_function(&program);

        assert_reference_param(&function.params[0], Type::I64, true);
        assert_reference_param(&function.params[1], Type::Bool, false);
        assert_reference_param(&function.params[2], Type::F64, true);
    }

    #[test]
    fn parses_value_reference_value_parameter_order() {
        let program = parse_ok("fn mix(prefix: String, n: & i64, enabled: bool) {}");
        let function = first_function(&program);

        assert!(matches!(&function.params[0].mode, ParamMode::Value));
        assert_reference_param(&function.params[1], Type::I64, false);
        assert!(matches!(&function.params[2].mode, ParamMode::Value));
    }

    #[test]
    fn parses_mutable_reference_method_parameter_after_self() {
        let program = parse_ok("class Box { fn set(self, value: &mut i64) {} }");
        let class = match &program.items[0] {
            Item::Class(class) => class,
            _ => panic!("expected first item to be a class"),
        };

        assert_reference_param(&class.methods[0].params[0], Type::I64, true);
    }

    #[test]
    fn parses_ampersand_only_as_call_argument_marker() {
        let source = "fn main() { f(&x, y); }";
        let program = parse_ok(source);
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::Call(call),
                ..
            }) => call,
            other => panic!("expected call expression, got {other:#?}"),
        };

        assert_eq!(call.args.len(), 2);
        assert_eq!(
            &source[call.args[0].span.start..call.args[0].span.end],
            "&x"
        );
        assert!(matches!(
            &call.args[0].mode,
            CallArgMode::Reference { ampersand_span } if &source[ampersand_span.start..ampersand_span.end] == "&"
        ));
        assert!(matches!(&call.args[0].expr, Expr::Var(name, _) if name == "x"));
        assert!(matches!(&call.args[1].mode, CallArgMode::Value));
        assert!(matches!(&call.args[1].expr, Expr::Var(name, _) if name == "y"));
    }

    #[test]
    fn parses_reference_argument_in_method_call() {
        let program =
            parse_ok("class Box { fn set(self, value: &mut i64) {} } fn main() { box.set(&n); }");
        let function = function_named(&program, "main");
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::MethodCall(call),
                ..
            }) => call,
            other => panic!("expected method call expression, got {other:#?}"),
        };

        assert_reference_arg(&call.args[0]);
    }

    #[test]
    fn parses_reference_argument_in_static_call() {
        let program = parse_ok("fn main() { Math::set(&n); }");
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_reference_arg(&call.args[0]);
    }

    #[test]
    fn parses_generic_static_call_type_arguments() {
        let program = parse_ok("fn main() { Channel<i64>::new(); }");
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_eq!(call.class, "Channel");
        assert_eq!(call.method, "new");
        assert_eq!(call.type_args, vec![Type::I64]);
    }

    #[test]
    fn parses_reference_argument_inside_nested_call() {
        let program = parse_ok("fn main() { outer(inner(&n)); }");
        let function = first_function(&program);
        let outer = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::Call(call),
                ..
            }) => call,
            other => panic!("expected outer call expression, got {other:#?}"),
        };
        let inner = match &outer.args[0].expr {
            Expr::Call(call) => call,
            other => panic!("expected inner call expression, got {other:#?}"),
        };

        assert_reference_arg(&inner.args[0]);
    }

    #[test]
    fn rejects_ampersand_as_general_reference_expression() {
        let errors = parse_errors("fn main() { let y = &x; }");
        assert!(
            errors.iter().any(|error| error.code == ErrorCode::E0102),
            "expected parser error for reference expression, got {errors:#?}"
        );
    }

    #[test]
    fn rejects_legacy_inout_parameter_syntax() {
        let errors = parse_errors("fn bump(x: inout i64) {}");
        assert!(
            !errors.is_empty(),
            "expected parser error for legacy inout syntax"
        );
    }

    #[test]
    fn rejects_reference_parameter_without_type() {
        let errors = parse_errors("fn read(x: &) {}");
        assert!(
            !errors.is_empty(),
            "expected parser error for missing reference parameter type"
        );
    }

    #[test]
    fn rejects_mutable_reference_parameter_without_type() {
        let errors = parse_errors("fn read(x: &mut) {}");
        assert!(
            !errors.is_empty(),
            "expected parser error for missing mutable reference parameter type"
        );
    }
}

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

    pub fn parse(&mut self) -> Result<Program, Vec<Diagnostic>> {
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

        if errors.is_empty() {
            Ok(Program { imports, items })
        } else {
            Err(errors)
        }
    }

    fn parse_import(&mut self) -> Result<ImportDecl, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Import)?;
        let path = self.expect_ident()?;
        let alias = if self.eat(TokenKind::As) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(TokenKind::Semicolon)?;
        Ok(ImportDecl { path, alias, span })
    }

    fn parse_item(&mut self) -> Result<Item, Diagnostic> {
        let public = self.eat(TokenKind::Pub);
        let is_open = self.eat(TokenKind::Open);
        match self.peek_kind() {
            TokenKind::Fn => Ok(Item::Function(self.parse_fn(public)?)),
            TokenKind::Class => Ok(Item::Class(self.parse_class(public, is_open)?)),
            _ => Err(self.err(ErrorCode::E0105, "expected `fn` or `class`")),
        }
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

            if self.check(TokenKind::Fn) {
                methods.push(self.parse_method(member_public, member_open, member_override)?);
            } else {
                if member_open || member_override {
                    return Err(self.err(
                        ErrorCode::E0105,
                        "`open` and `override` can only be used on methods",
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
                    let p_span = self.current_span();
                    let p_name = self.expect_ident()?;
                    self.expect(TokenKind::Colon)?;
                    let ty = self.parse_type()?;
                    params.push(Param {
                        name: p_name,
                        ty,
                        span: p_span,
                    });
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
            }
        } else {
            while !self.check(TokenKind::RParen) && !self.at_eof() {
                let p_span = self.current_span();
                let p_name = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let ty = self.parse_type()?;
                params.push(Param {
                    name: p_name,
                    ty,
                    span: p_span,
                });
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
            is_open,
            is_override,
            params,
            has_self,
            return_type,
            body,
            span,
        })
    }

    fn parse_fn(&mut self, public: bool) -> Result<FunctionDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;

        let mut params = Vec::new();
        while !self.check(TokenKind::RParen) && !self.at_eof() {
            let p_span = self.current_span();
            let p_name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            params.push(Param {
                name: p_name,
                ty,
                span: p_span,
            });
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
            params,
            return_type,
            body,
            span,
        })
    }

    fn parse_type(&mut self) -> Result<Type, Diagnostic> {
        match self.peek_kind().clone() {
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
                Ok(Type::Named(name))
            }
            _ => Err(self.err(
                ErrorCode::E0107,
                "expected type (`i64`, `f64`, `bool`, or type name)",
            )),
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
        self.parse_or()
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
                    let mut args = Vec::new();
                    while !self.check(TokenKind::RParen) && !self.at_eof() {
                        args.push(self.parse_expr()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen)?;
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
            TokenKind::Ident(name) => {
                let span = self.current_span();
                self.advance();
                if self.eat(TokenKind::ColonColon) {
                    // ClassName::method(args) — static call
                    let method = self.expect_ident()?;
                    self.expect(TokenKind::LParen)?;
                    let mut args = Vec::new();
                    while !self.check(TokenKind::RParen) && !self.at_eof() {
                        args.push(self.parse_expr()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                        class: name,
                        method,
                        args,
                        span,
                    })))
                } else if self.eat(TokenKind::LParen) {
                    let mut args = Vec::new();
                    while !self.check(TokenKind::RParen) && !self.at_eof() {
                        args.push(self.parse_expr()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    Ok(Expr::Call(Box::new(CallExpr {
                        callee: name,
                        args,
                        span,
                    })))
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
            _ => Err(self.err(ErrorCode::E0102, "expected expression")),
        }
    }

    // --- helpers ---

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn current_span(&self) -> Span {
        self.tokens[self.pos].span
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
                TokenKind::Fn | TokenKind::Class | TokenKind::Pub | TokenKind::Import
            ) {
                break;
            }
            self.advance();
        }
    }
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

use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Span};
use crate::lexer::token::TokenKind;

impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<Expr, Diagnostic> {
        self.parse_range()
    }

    pub(super) fn parse_range(&mut self) -> Result<Expr, Diagnostic> {
        let lhs = self.parse_ternary()?;
        if !self.eat(TokenKind::DotDot) {
            return Ok(lhs);
        }
        let rhs = self.parse_ternary()?;
        let start = lhs.span();
        let end = rhs.span();
        let span = Span::new(start.start, end.end, start.line, start.col);
        Ok(Expr::Range(Box::new(RangeExpr {
            start: lhs,
            end: rhs,
            span,
        })))
    }

    // condition ? then_expr : else_expr  (right-associative, lower than ||)
    pub(super) fn parse_ternary(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_or(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_and(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_cmp(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_add(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_mul(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_unary(&mut self) -> Result<Expr, Diagnostic> {
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
    pub(super) fn parse_postfix(&mut self) -> Result<Expr, Diagnostic> {
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
            } else if matches!(self.peek_kind(), TokenKind::Question)
                && self.is_try_propagate_question()
            {
                let span = self.current_span();
                self.advance();
                lhs = Expr::TryPropagate(Box::new(lhs), span);
            } else if matches!(self.peek_kind(), TokenKind::LBracket) {
                let span = self.current_span(); // the `[`
                self.advance();
                let index = self.parse_expr()?;
                self.expect(TokenKind::RBracket)?;
                lhs = Expr::Index(Box::new(lhs), Box::new(index), span);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    pub(super) fn parse_primary(&mut self) -> Result<Expr, Diagnostic> {
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
                if self.eat(TokenKind::ColonColon) {
                    self.parse_static_call("Self".to_string(), span)
                } else {
                    Ok(Expr::Var("self".to_string(), span))
                }
            }
            TokenKind::LBracket => {
                let span = self.current_span();
                self.advance();
                let mut elements = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RBracket | TokenKind::Eof) {
                    elements.push(self.parse_expr()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RBracket)?;
                Ok(Expr::ArrayLiteral(elements, span))
            }
            TokenKind::New => self.parse_new(),
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
            TokenKind::Ident(name) if name == "std" => self.parse_std_qualified_expr(),
            TokenKind::Ident(name) => {
                let span = self.current_span();
                self.advance();
                if let Some(expr) = self.try_parse_generic_static_call(name.clone(), span)? {
                    Ok(expr)
                } else if self.eat(TokenKind::ColonColon) {
                    let member = self.expect_ident()?;
                    if self.eat(TokenKind::ColonColon) {
                        let method = self.expect_ident()?;
                        let class = format!("{name}::{member}");
                        if super::is_type_constructor_name(&method)
                            && !matches!(self.peek_kind(), TokenKind::LParen)
                        {
                            // Module-qualified enum variant used as a value:
                            // e.g. `geom::Color::Red`.
                            Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                                class,
                                type_args: vec![],
                                method,
                                args: vec![],
                                span,
                            })))
                        } else if !matches!(self.peek_kind(), TokenKind::LParen) {
                            // `mod::Class::property` — static property read.
                            Ok(Expr::StaticField(StaticFieldExpr {
                                class,
                                field: method,
                                span,
                            }))
                        } else {
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
                    } else if super::is_type_constructor_name(&member)
                        && self.eat(TokenKind::LBrace)
                    {
                        self.parse_object_literal_fields(format!("{name}::{member}"), span)
                    } else if super::is_type_constructor_name(&member)
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
                    } else if !matches!(self.peek_kind(), TokenKind::LParen) {
                        // `Class::property` — static property read (no parens).
                        Ok(Expr::StaticField(StaticFieldExpr {
                            class: name,
                            field: member,
                            span,
                        }))
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
                } else if super::is_type_constructor_name(&name) && self.eat(TokenKind::LBrace) {
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

    pub(super) fn parse_static_call(
        &mut self,
        class: String,
        span: Span,
    ) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_std_qualified_expr(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();
        self.advance(); // std
        let mut parts = vec!["std".to_string()];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }

        if self.eat(TokenKind::LParen) {
            let args = self.parse_call_args_after_lparen()?;
            if parts.as_slice() == ["std", "io", "print"]
                || parts.as_slice() == ["std", "io", "println"]
            {
                let newline = parts.last().is_some_and(|name| name == "println");
                if args.len() != 1 {
                    return Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                        class: "std::io".to_string(),
                        type_args: vec![],
                        method: parts.last().cloned().unwrap_or_default(),
                        args,
                        span,
                    })));
                }
                let mut args = args;
                return Ok(Expr::Print(Box::new(args.remove(0).expr), newline, span));
            }

            let Some(method) = parts.pop() else {
                return Err(self.err(ErrorCode::E0102, "expected std item path"));
            };
            if parts.is_empty() {
                return Err(self.err(ErrorCode::E0102, "expected std item path"));
            }
            return Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                class: parts.join("::"),
                type_args: vec![],
                method,
                args,
                span,
            })));
        }

        if parts.len() >= 4 {
            let method = parts.pop().unwrap();
            return Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                class: parts.join("::"),
                type_args: vec![],
                method,
                args: vec![],
                span,
            })));
        }

        Err(self.err(ErrorCode::E0102, "expected fully qualified std item call"))
    }

    pub(super) fn try_parse_generic_static_call(
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

    pub(super) fn parse_call_args_after_lparen(&mut self) -> Result<Vec<CallArg>, Diagnostic> {
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

    pub(super) fn parse_call_arg(&mut self) -> Result<CallArg, Diagnostic> {
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

    pub(super) fn parse_object_literal_fields(
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

    pub(super) fn parse_new(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::New)?;
        // Class path: `Class` or module-qualified `mod::Class`.
        let mut class_name = self.expect_ident()?;
        while self.eat(TokenKind::ColonColon) {
            class_name.push_str("::");
            class_name.push_str(&self.expect_ident()?);
        }
        // Optional generic type args: `new Box<i64>(...)`.
        let mut type_args = Vec::new();
        if self.eat(TokenKind::Lt) {
            while !matches!(self.peek_kind(), TokenKind::Gt | TokenKind::Eof) {
                type_args.push(self.parse_type()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::Gt)?;
        }
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Expr::New(Box::new(NewExpr {
            class_name,
            type_args,
            args,
            span,
        })))
    }

    pub(super) fn parse_select(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Select)?;
        self.expect(TokenKind::LBrace)?;

        let mut cases = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let case_span = self.current_span();
            let kind = if self.check(TokenKind::Let) {
                // `let v = ch.recv() => { ... }`
                self.advance();
                let binding = self.expect_ident()?;
                self.expect(TokenKind::Eq)?;
                match self.parse_expr()? {
                    Expr::MethodCall(m) if m.method == "recv" => SelectCaseKind::Recv {
                        binding,
                        channel: m.object,
                    },
                    _ => {
                        return Err(self.err(
                            ErrorCode::E0103,
                            "select `let` case must bind a `ch.recv()`",
                        ));
                    }
                }
            } else if matches!(self.peek_kind(), TokenKind::Ident(name) if name == "default") {
                self.advance();
                SelectCaseKind::Default
            } else {
                // `ch.recv() => ...` (discarded value) or `ch.send(x) => ...`
                match self.parse_expr()? {
                    Expr::MethodCall(m) if m.method == "recv" => SelectCaseKind::Recv {
                        binding: "_".to_string(),
                        channel: m.object,
                    },
                    Expr::MethodCall(m) if m.method == "send" && m.args.len() == 1 => {
                        SelectCaseKind::Send {
                            channel: m.object,
                            value: m.args.into_iter().next().unwrap().expr,
                        }
                    }
                    _ => {
                        return Err(self.err(
                            ErrorCode::E0103,
                            "select case must be `let v = ch.recv()`, `ch.recv()`, `ch.send(x)`, or `default`",
                        ));
                    }
                }
            };
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_block()?;
            cases.push(SelectCase {
                kind,
                body,
                span: case_span,
            });
        }

        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Select(SelectExpr { cases, span: start }))
    }

    pub(super) fn parse_lambda(&mut self) -> Result<Expr, Diagnostic> {
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

    pub(super) fn parse_match_expr(&mut self) -> Result<Expr, Diagnostic> {
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
            } else if matches!(self.peek_kind(), TokenKind::Return) {
                // `Pattern => return [expr]` — sugar for a single-statement
                // block arm, so match works in statement position with early
                // returns (willow-zvkv). No trailing `;` inside an arm.
                let ret_span = self.current_span();
                self.advance(); // consume `return`
                let value = if matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RBrace) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                let end = self.previous_span();
                let block_span = Span::new(ret_span.start, end.end, ret_span.line, ret_span.col);
                MatchBody::Block(Block {
                    stmts: vec![Stmt::Return(ReturnStmt {
                        value,
                        span: ret_span,
                    })],
                    span: block_span,
                })
            } else {
                let expr = self.parse_expr()?;
                MatchBody::Expr(Box::new(expr))
            };
            let arm_end = self.current_span();
            let arm_span = Span::new(arm_start.start, arm_end.end, arm_start.line, arm_start.col);
            arms.push(MatchArm {
                pattern,
                body,
                span: arm_span,
            });
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
}

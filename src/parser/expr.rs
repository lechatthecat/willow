use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::TokenKind;

#[willow_continuations::parser]
impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<Expr, Diagnostic> {
        // Keep this wrapper direct: an extra continuation per expression would
        // allocate two more boxes for every ordinary expression.
        let saved = std::mem::replace(&mut self.allow_object_literals, true);
        let result = self.parse_range();
        self.allow_object_literals = saved;
        let expr = result?;
        self.reject_as_cast()?;
        Ok(expr)
    }

    /// A lock target ends at its binding `as`; nested expressions still reject casts.
    pub(super) fn parse_expr_before_as(&mut self) -> Result<Expr, Diagnostic> {
        let saved = std::mem::replace(&mut self.allow_object_literals, true);
        let result = self.parse_range();
        self.allow_object_literals = saved;
        result
    }

    /// At a control-flow head, an unparenthesized `{` starts the body.
    /// Nested expression delimiters re-enable object literals via `parse_expr`.
    pub(super) fn parse_control_head(&mut self) -> Result<Expr, Diagnostic> {
        let saved = std::mem::replace(&mut self.allow_object_literals, false);
        let result = self.parse_range();
        self.allow_object_literals = saved;
        let expr = result?;
        self.reject_as_cast()?;
        Ok(expr)
    }

    fn reject_as_cast(&self) -> Result<(), Diagnostic> {
        if matches!(self.peek_kind(), TokenKind::As) {
            return Err(self
                .err_at(ErrorCode::E0102, "`as` casts are not supported", self.current_span())
                .with_help("for a numeric constant, write an f64 literal such as `5.0`; runtime i64/f64 conversion is not yet available"));
        }
        Ok(())
    }

    pub(super) fn parse_range(&mut self) -> Result<Expr, Diagnostic> {
        let lhs = self.parse_ternary()?;
        if !self.eat(TokenKind::DotDot) {
            return Ok(lhs);
        }
        let rhs = self.parse_ternary()?;
        let start = lhs.span();
        let end = rhs.span();
        let span = start.to(end);
        Ok(Expr::Range(Box::new(RangeExpr {
            start: lhs,
            end: rhs,
            span,
            id: ExprId::fresh(),
        })))
    }

    // condition ? then_expr : else_expr  (right-associative, lower than ||)
    pub(super) fn parse_ternary(&mut self) -> Result<Expr, Diagnostic> {
        enum Frame {
            Then {
                condition: Expr,
                span: Span,
            },
            Else {
                condition: Expr,
                then_expr: Expr,
                span: Span,
            },
        }
        let mut frames = Vec::new();
        loop {
            let span = self.current_span();
            let mut value = self.parse_or()?;
            if self.eat(TokenKind::Question) {
                frames.push(Frame::Then {
                    condition: value,
                    span,
                });
                continue;
            }
            loop {
                match frames.pop() {
                    None => return Ok(value),
                    Some(Frame::Then { condition, span }) => {
                        self.reject_as_cast()?;
                        if !self.eat(TokenKind::Colon) {
                            return Err(self
                                .err(ErrorCode::E0903, "expected `:` in ternary expression")
                                .with_help(
                                    "write the ternary as `condition ? then_value : else_value`",
                                ));
                        }
                        frames.push(Frame::Else {
                            condition,
                            then_expr: value,
                            span,
                        });
                        break;
                    }
                    Some(Frame::Else {
                        condition,
                        then_expr,
                        span,
                    }) => {
                        let span = span.to(value.span());
                        value = Expr::Ternary(Box::new(TernaryExpr {
                            condition,
                            then_expr,
                            else_expr: value,
                            span,
                            id: ExprId::fresh(),
                        }));
                    }
                }
            }
        }
    }

    /// Expression-form conditionals share the ternary AST and all its typing,
    /// short-circuit lowering and contextual conversions. Braces delimit one
    /// branch expression; statement-form `if` continues to use `parse_if`.
    /// Collect and fold else-if rungs once, without recursive ladder parsing.
    pub(super) fn parse_if_expr(&mut self) -> Result<Expr, Diagnostic> {
        let mut rungs = Vec::new();
        let mut value = loop {
            let start = self.current_span();
            self.expect(TokenKind::If)?;
            let condition = self.parse_control_head()?;
            let then_expr = self.parse_if_value()?;
            rungs.push((start, condition, then_expr));
            if !self.eat(TokenKind::Else) {
                return Err(self
                    .err(
                        ErrorCode::E0102,
                        "an `if` expression requires an `else` branch",
                    )
                    .with_help("write `if condition { value } else { other_value }`"));
            }
            if !self.check(TokenKind::If) {
                break self.parse_if_value()?;
            }
        };
        let end = self.previous_span();
        while let Some((start, condition, then_expr)) = rungs.pop() {
            value = Expr::Ternary(Box::new(TernaryExpr {
                condition,
                then_expr,
                else_expr: value,
                span: start.to(end),
                id: ExprId::fresh(),
            }));
        }
        Ok(value)
    }

    pub(super) fn parse_if_value(&mut self) -> Result<Expr, Diagnostic> {
        self.expect(TokenKind::LBrace)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::RBrace)?;
        Ok(value)
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
                id: ExprId::fresh(),
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
                id: ExprId::fresh(),
            }));
        }
        Ok(lhs)
    }

    pub(super) fn parse_cmp(&mut self) -> Result<Expr, Diagnostic> {
        let mut lhs = self.parse_bitwise()?;
        loop {
            // `<<=` / `>>=` end the expression: the statement parser reads
            // them as compound assignments.
            if self.shift_assign_op().is_some() {
                break;
            }
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
            let rhs = self.parse_bitwise()?;
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op,
                lhs,
                rhs,
                span,
                id: ExprId::fresh(),
            }));
        }
        Ok(lhs)
    }

    /// `|`, `^`, `&`, `<<` and `>>`, all left-associative. They bind tighter
    /// than comparisons and looser than arithmetic, from loosest to tightest
    /// `|`, `^`, `&`, then the shifts (Rust's order), so `x & mask == 0` is
    /// `(x & mask) == 0` and `1 << n - 1` is `1 << (n - 1)` (willow-jz15.8).
    ///
    /// One operator-precedence loop covers all four levels: a function per
    /// level would add three continuations to every expression parse.
    pub(super) fn parse_bitwise(&mut self) -> Result<Expr, Diagnostic> {
        fn reduce(operands: &mut Vec<Expr>, (op, span, _): (BinOp, Span, u8)) {
            let rhs = operands.pop().expect("bitwise right operand");
            let lhs = operands.pop().expect("bitwise left operand");
            operands.push(Expr::Binary(Box::new(BinaryExpr {
                op,
                lhs,
                rhs,
                span,
                id: ExprId::fresh(),
            })));
        }
        let mut operands = vec![self.parse_add()?];
        let mut operators: Vec<(BinOp, Span, u8)> = Vec::new();
        loop {
            let (op, precedence, width) = match self.peek_kind() {
                TokenKind::Pipe => (BinOp::BitOr, 0, 1),
                TokenKind::Caret => (BinOp::BitXor, 1, 1),
                TokenKind::Ampersand => (BinOp::BitAnd, 2, 1),
                _ => match self.shift_op() {
                    Some(op) => (op, 3, 2),
                    None => break,
                },
            };
            while operators
                .last()
                .is_some_and(|&(_, _, top)| top >= precedence)
            {
                reduce(&mut operands, operators.pop().expect("checked operator"));
            }
            let mut span = self.current_span();
            for _ in 0..width {
                span = span.to(self.current_span());
                self.advance();
            }
            operators.push((op, span, precedence));
            operands.push(self.parse_add()?);
        }
        while let Some(operator) = operators.pop() {
            reduce(&mut operands, operator);
        }
        Ok(operands.pop().expect("bitwise operand"))
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
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op,
                lhs,
                rhs,
                span,
                id: ExprId::fresh(),
            }));
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
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op,
                lhs,
                rhs,
                span,
                id: ExprId::fresh(),
            }));
        }
        Ok(lhs)
    }

    /// Prefix `-` / `!`, then exponentiation.
    ///
    /// `**` binds tighter than a prefix sign, so the operand of `-` is a whole
    /// power expression: `-2 ** 2` parses as `-(2 ** 2)`, and `(-2) ** 2` needs
    /// the parentheses. The operand is [`Self::parse_pow`] rather than
    /// `parse_unary`, which keeps `- -x` a parse error exactly as before this
    /// operator existed.
    pub(super) fn parse_unary(&mut self) -> Result<Expr, Diagnostic> {
        match self.peek_kind().clone() {
            TokenKind::Minus => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_pow()?;
                Ok(Expr::Unary(Box::new(UnaryExpr {
                    op: UnaryOp::Neg,
                    expr,
                    span,
                    id: ExprId::fresh(),
                })))
            }
            TokenKind::Bang => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_pow()?;
                Ok(Expr::Unary(Box::new(UnaryExpr {
                    op: UnaryOp::Not,
                    expr,
                    span,
                    id: ExprId::fresh(),
                })))
            }
            _ => self.parse_pow(),
        }
    }

    /// `**`, right-associative.
    ///
    /// The left operand is [`Self::parse_await`], not `parse_unary`: prefix
    /// `await` binds tighter than `**`, so `await task ** 2` is
    /// `(await task) ** 2` and a task never reaches the exponent operator. The
    /// right operand is `parse_unary`, which both gives right associativity
    /// (`2 ** 3 ** 2` == `2 ** (3 ** 2)`) and lets the exponent carry a sign
    /// (`2.0 ** -3.0`).
    pub(super) fn parse_pow(&mut self) -> Result<Expr, Diagnostic> {
        let mut operands = Vec::new();
        let mut lhs = self.parse_await()?;
        while self.check(TokenKind::StarStar) {
            let span = self.current_span();
            self.advance();
            let prefix = match self.peek_kind() {
                TokenKind::Minus => Some(UnaryOp::Neg),
                TokenKind::Bang => Some(UnaryOp::Not),
                _ => None,
            };
            let prefix_span = self.current_span();
            if prefix.is_some() {
                self.advance();
            }
            operands.push((lhs, span, prefix, prefix_span));
            lhs = self.parse_await()?;
        }
        while let Some((base, span, prefix, prefix_span)) = operands.pop() {
            if let Some(op) = prefix {
                lhs = Expr::Unary(Box::new(UnaryExpr {
                    op,
                    expr: lhs,
                    span: prefix_span,
                    id: ExprId::fresh(),
                }));
            }
            lhs = Expr::Binary(Box::new(BinaryExpr {
                op: BinOp::Pow,
                lhs: base,
                rhs: lhs,
                span,
                id: ExprId::fresh(),
            }));
        }
        Ok(lhs)
    }

    /// Prefix `await`, which binds tighter than `**` and looser than postfix.
    pub(super) fn parse_await(&mut self) -> Result<Expr, Diagnostic> {
        let mut spans = Vec::new();
        while self.check(TokenKind::Await) {
            spans.push(self.current_span());
            self.advance();
        }
        let mut expr = self.parse_postfix()?;
        for span in spans.into_iter().rev() {
            expr = Expr::Await(Box::new(AwaitExpr {
                expr,
                span,
                id: ExprId::fresh(),
            }));
        }
        Ok(expr)
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
                        id: ExprId::fresh(),
                    }));
                } else {
                    lhs = Expr::FieldAccess(Box::new(lhs), member, span, ExprId::fresh());
                }
            } else if matches!(self.peek_kind(), TokenKind::Question)
                && self.is_try_propagate_question()
            {
                let span = self.current_span();
                self.advance();
                lhs = Expr::TryPropagate(Box::new(lhs), span, ExprId::fresh());
            } else if matches!(self.peek_kind(), TokenKind::LBracket) {
                let span = self.current_span(); // the `[`
                self.advance();
                let index = self.parse_expr()?;
                self.expect(TokenKind::RBracket)?;
                lhs = Expr::Index(Box::new(lhs), Box::new(index), span, ExprId::fresh());
            } else if matches!(self.peek_kind(), TokenKind::LParen)
                && matches!(
                    lhs,
                    Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
                )
            {
                let callee = match &lhs {
                    Expr::Call(call) => call.callee.as_str(),
                    Expr::MethodCall(call) => call.method.as_str(),
                    Expr::StaticCall(call) => call.method.as_str(),
                    _ => unreachable!(),
                };
                return Err(self
                    .err(ErrorCode::E0102, "calling a call result is not supported")
                    .with_help(format!("bind the result of `{callee}` to a local variable first, then call that variable with the second argument list")));
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
                Ok(Expr::Integer(n, span, ExprId::fresh()))
            }
            TokenKind::Float(f) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Float(f, span, ExprId::fresh()))
            }
            TokenKind::True => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Bool(true, span, ExprId::fresh()))
            }
            TokenKind::False => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::Bool(false, span, ExprId::fresh()))
            }
            TokenKind::Nil => {
                let span = self.current_span();
                self.advance();
                self.recovered_errors.push(
                    Diagnostic::new(Severity::Error, ErrorCode::E0201, "`nil` has been removed")
                        .with_label(Label::primary(span, "removed absence literal"))
                        .with_help("replace `nil` with `None`"),
                );
                // Keep parsing and type checking deterministic after the hard
                // removal error. The recovery expression is the replacement
                // spelling, not a hidden nil value or type; the accumulated
                // parser diagnostic still makes compilation fail.
                Ok(Expr::Var("None".to_string(), span, ExprId::fresh()))
            }
            TokenKind::StringLiteral(value) => {
                let span = self.current_span();
                self.advance();
                Ok(Expr::String(value, span, ExprId::fresh()))
            }
            TokenKind::Print => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::LParen)?;
                let arg = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(Expr::Print(Box::new(arg), false, span, ExprId::fresh()))
            }
            TokenKind::Println => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::LParen)?;
                let arg = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(Expr::Print(Box::new(arg), true, span, ExprId::fresh()))
            }
            TokenKind::SelfKw => {
                let span = self.current_span();
                self.advance();
                if self.eat(TokenKind::ColonColon) {
                    self.parse_static_call("Self".to_string(), span)
                } else {
                    Ok(Expr::Var("self".to_string(), span, ExprId::fresh()))
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
                Ok(Expr::ArrayLiteral(elements, span, ExprId::fresh()))
            }
            TokenKind::New => self.parse_new(),
            TokenKind::Select => self.parse_select(),
            TokenKind::If => self.parse_if_expr(),
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
                    let member_span = self.current_span();
                    let member = self.expect_ident()?;
                    if self.eat(TokenKind::ColonColon) {
                        let method_span = self.current_span();
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
                                span: span.to(self.previous_span()),
                                id: ExprId::fresh(),
                                method_span,
                            })))
                        } else if !matches!(self.peek_kind(), TokenKind::LParen) {
                            // `mod::Class::property` — static property read.
                            Ok(Expr::StaticField(StaticFieldExpr {
                                class,
                                field: method,
                                span: span.to(self.previous_span()),
                                id: ExprId::fresh(),
                            }))
                        } else {
                            self.expect(TokenKind::LParen)?;
                            let args = self.parse_call_args_after_lparen()?;
                            Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                                class,
                                type_args: vec![],
                                method,
                                args,
                                span: span.to(self.previous_span()),
                                id: ExprId::fresh(),
                                method_span,
                            })))
                        }
                    } else if self.allow_object_literals
                        && super::is_type_constructor_name(&member)
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
                            span: span.to(self.previous_span()),
                            id: ExprId::fresh(),
                            method_span: member_span,
                        })))
                    } else if !matches!(self.peek_kind(), TokenKind::LParen) {
                        // `Class::property` — static property read (no parens).
                        Ok(Expr::StaticField(StaticFieldExpr {
                            class: name,
                            field: member,
                            span: span.to(self.previous_span()),
                            id: ExprId::fresh(),
                        }))
                    } else {
                        self.expect(TokenKind::LParen)?;
                        let args = self.parse_call_args_after_lparen()?;
                        Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                            class: name,
                            type_args: vec![],
                            method: member,
                            args,
                            span: span.to(self.previous_span()),
                            id: ExprId::fresh(),
                            method_span: member_span,
                        })))
                    }
                } else if self.eat(TokenKind::LParen) {
                    let args = self.parse_call_args_after_lparen()?;
                    Ok(Expr::Call(Box::new(CallExpr {
                        callee: name,
                        args,
                        span,
                        id: ExprId::fresh(),
                    })))
                } else if self.allow_object_literals
                    && super::is_type_constructor_name(&name)
                    && self.eat(TokenKind::LBrace)
                {
                    self.parse_object_literal_fields(name, span)
                } else {
                    Ok(Expr::Var(name, span, ExprId::fresh()))
                }
            }
            TokenKind::LParen => {
                let span = self.current_span();
                self.advance();
                let expr = self.parse_expr()?;
                if !self.eat(TokenKind::Comma) {
                    self.expect(TokenKind::RParen)?;
                    return Ok(expr);
                }
                let mut args = vec![CallArg::value(expr)];
                while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                    args.push(CallArg::value(self.parse_expr()?));
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
                self.tuple_arities.insert(args.len());
                Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                    class: super::tuples::name(args.len()),
                    type_args: vec![],
                    method: super::tuples::VARIANT.into(),
                    args,
                    span: span.to(self.previous_span()),
                    method_span: span,
                    id: ExprId::fresh(),
                })))
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
        let method_span = self.current_span();
        let method = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Expr::StaticCall(Box::new(StaticCallExpr {
            class,
            type_args: vec![],
            method,
            args,
            span: span.to(self.previous_span()),
            id: ExprId::fresh(),
            method_span,
        })))
    }

    pub(super) fn parse_std_qualified_expr(&mut self) -> Result<Expr, Diagnostic> {
        let span = self.current_span();
        self.advance(); // std
        let mut parts = vec!["std".to_string()];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }

        let method_span = self.previous_span();
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
                        span: span.to(self.previous_span()),
                        id: ExprId::fresh(),
                        method_span,
                    })));
                }
                let mut args = args;
                return Ok(Expr::Print(
                    Box::new(args.remove(0).expr),
                    newline,
                    span,
                    ExprId::fresh(),
                ));
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
                span: span.to(self.previous_span()),
                id: ExprId::fresh(),
                method_span,
            })));
        }

        if parts.len() >= 4 {
            let method = parts.pop().unwrap();
            return Ok(Expr::StaticCall(Box::new(StaticCallExpr {
                class: parts.join("::"),
                type_args: vec![],
                method,
                args: vec![],
                span: span.to(self.previous_span()),
                id: ExprId::fresh(),
                method_span,
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
        let saved_brace_depth = self.brace_depth;
        let saved_pending_type_eq = self.pending_type_eq;
        let saved_last_span = self.last_span;
        let saved_type_uses = self.type_uses.len();
        self.advance();
        let mut type_args = Vec::new();
        while !self.check(TokenKind::Gt) && !self.at_eof() {
            match self.parse_type() {
                Ok(ty) => type_args.push(ty),
                Err(_) => {
                    self.pos = saved;
                    self.brace_depth = saved_brace_depth;
                    self.pending_type_eq = saved_pending_type_eq;
                    self.last_span = saved_last_span;
                    self.type_uses.truncate(saved_type_uses);
                    return Ok(None);
                }
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }

        if !self.eat(TokenKind::Gt) || !self.eat(TokenKind::ColonColon) {
            self.pos = saved;
            self.brace_depth = saved_brace_depth;
            self.pending_type_eq = saved_pending_type_eq;
            self.last_span = saved_last_span;
            self.type_uses.truncate(saved_type_uses);
            return Ok(None);
        }

        let method_span = self.current_span();
        let method = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Some(Expr::StaticCall(Box::new(StaticCallExpr {
            class,
            type_args,
            method,
            args,
            span: span.to(self.previous_span()),
            id: ExprId::fresh(),
            method_span,
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
                span: ampersand_span.to(expr_span),
            });
        }

        let start = self.current_span();
        let mut argument = CallArg::value(self.parse_expr()?);
        argument.span = start.to(self.previous_span());
        Ok(argument)
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
            id: ExprId::fresh(),
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
            while !self.at_type_gt() && !self.at_eof() {
                type_args.push(self.parse_type()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect_type_gt()?;
        }
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        Ok(Expr::New(Box::new(NewExpr {
            class_name,
            type_args,
            args,
            span: span.to(self.previous_span()),
            id: ExprId::fresh(),
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
                match &mut self.parse_expr()? {
                    // `recv` takes no arguments. Matching on the method name
                    // alone would silently discard `ch.recv(f())` — both the
                    // arity error and the argument's side effects.
                    Expr::MethodCall(m)
                        if matches!(m.method.as_str(), "recv" | "recv_opt")
                            && m.args.is_empty() =>
                    {
                        SelectCaseKind::Recv {
                            binding,
                            channel: m.object.take(),
                            closed_aware: m.method == "recv_opt",
                        }
                    }
                    Expr::Await(a) => Self::select_await_case(binding, a.expr.take()),
                    other => return Err(self.select_let_case_error(other)),
                }
            } else if matches!(self.peek_kind(), TokenKind::Ident(name) if name == "default") {
                self.advance();
                SelectCaseKind::Default
            } else {
                // `ch.recv() => ...` (discarded value) or `ch.send(x) => ...`
                match &mut self.parse_expr()? {
                    Expr::MethodCall(m)
                        if matches!(m.method.as_str(), "recv" | "recv_opt")
                            && m.args.is_empty() =>
                    {
                        SelectCaseKind::Recv {
                            binding: "_".to_string(),
                            channel: m.object.take(),
                            closed_aware: m.method == "recv_opt",
                        }
                    }
                    Expr::Await(a) => Self::select_await_case("_".to_string(), a.expr.take()),
                    // The select case forms drop the `CallArgMode`, so a
                    // reference argument would be silently downgraded to a value
                    // one — rejected here with the same code a plain
                    // `ch.send(&v)` / `sleep(&ms)` gets from the checker.
                    Expr::MethodCall(m) if m.method == "send" && m.args.len() == 1 => {
                        let arg = m.args.remove(0);
                        if let CallArgMode::Reference { ampersand_span } = arg.mode {
                            return Err(self.err_at(
                                ErrorCode::E1703,
                                "unexpected reference argument: `send` takes its value by value",
                                ampersand_span,
                            ));
                        }
                        SelectCaseKind::Send {
                            channel: m.object.take(),
                            value: arg.expr,
                        }
                    }
                    Expr::Call(c) if c.callee == "sleep" && c.args.len() == 1 => {
                        let arg = c.args.remove(0);
                        if let CallArgMode::Reference { ampersand_span } = arg.mode {
                            return Err(self.err_at(
                                ErrorCode::E1703,
                                "unexpected reference argument: `sleep` takes `i64` by value",
                                ampersand_span,
                            ));
                        }
                        SelectCaseKind::Timeout { millis: arg.expr }
                    }
                    other => return Err(self.select_case_error(other)),
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
        Ok(Expr::Select(SelectExpr {
            cases,
            span: start,
            id: ExprId::fresh(),
        }))
    }

    /// Build the task-completion case behind `await <expr>` inside a `select`
    /// (willow-qrj9). Keep the expression intact: cancellation-awareness is a
    /// property of its checked type, not of whether the parser happens to see
    /// an inline method named `result`.
    fn select_await_case(binding: String, awaited: Expr) -> SelectCaseKind {
        SelectCaseKind::Join {
            binding,
            task: awaited,
        }
    }

    /// Error for a `select` `let` case that binds something unsupported. A
    /// Removed Task completion methods get dedicated migration diagnostics.
    fn select_let_case_error(&self, bound: &Expr) -> Diagnostic {
        if let Some(d) = self.select_removed_wait_error(bound) {
            return d;
        }
        self.err(
            ErrorCode::E0103,
            "select `let` case must bind a `ch.recv()`, a `ch.recv_opt()` or an `await` (`let v = await t`, `let r = await t.result()`)",
        )
    }

    /// Error for a `select` case that is not a recognised wait condition.
    fn select_case_error(&self, case: &Expr) -> Diagnostic {
        if let Some(d) = self.select_removed_wait_error(case) {
            return d;
        }
        self.err(
            ErrorCode::E0103,
            "select case must be `let v = ch.recv()`, `let v = ch.recv_opt()`, `ch.recv()`, `ch.send(x)`, `let v = await t`, `let r = await t.result()`, `sleep(ms)`, or `default`",
        )
    }

    /// The two removed Task-waiting shapes, reported with their own migration
    /// text (willow-qrj9).
    fn select_removed_wait_error(&self, expr: &Expr) -> Option<Diagnostic> {
        let Expr::MethodCall(m) = expr else {
            return None;
        };
        if !m.args.is_empty() {
            return None;
        }
        match m.method.as_str() {
            "join" => Some(
                self.err(
                    ErrorCode::E0812,
                    "`join()` has been removed: a select task case is now `let v = await t => { ... }`",
                ),
            ),
            "try_join" => Some(self.err(
                ErrorCode::E0813,
                "`try_join()` has been removed: use `let v = await t` or `let r = await t.result()`",
            )),
            _ => None,
        }
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
            span: span.to(self.previous_span()),
            id: ExprId::fresh(),
        })))
    }

    pub(super) fn parse_match_expr(&mut self) -> Result<Expr, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Match)?;
        let scrutinee = self.parse_control_head()?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let arm_start = self.current_span();
            let pattern = self.parse_pattern()?;
            // `pattern if guard => body` (willow-jz15.6).
            let guard = if self.eat(TokenKind::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
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
                let block_span = ret_span.to(end);
                MatchBody::Block(Block {
                    id: crate::parser::ast::BodyId::fresh(),
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
            let arm_span = arm_start.to(arm_end);
            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span: arm_span,
            });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.advance();
            }
        }
        let end_span = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = start.to(end_span);
        Ok(Expr::Match(Box::new(MatchExpr {
            scrutinee: Box::new(scrutinee),
            arms,
            span,
            id: ExprId::fresh(),
            source: MatchSource::Match,
        })))
    }
}

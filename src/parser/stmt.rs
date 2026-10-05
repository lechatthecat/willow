use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::TokenKind;

#[willow_continuations::parser]
impl Parser {
    pub(super) fn parse_block(&mut self) -> Result<Block, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            if matches!(
                self.peek_kind(),
                TokenKind::Fn | TokenKind::Pub | TokenKind::Class | TokenKind::Async
            ) {
                return Err(self
                    .err_at(
                        ErrorCode::E0103,
                        "expected `}` to close block",
                        self.current_span(),
                    )
                    .with_label(Label::secondary(start, "block opened here")));
            }
            match self.parse_stmt() {
                Ok(stmt) => stmts.push(stmt),
                Err(e) => {
                    // Record and resume at the next statement so one bad
                    // statement cannot swallow the rest of the item
                    // (willow-qzxg).
                    self.recovered_errors.push(e);
                    self.recover_to_next_stmt();
                }
            }
        }
        let end = self.current_span();
        self.expect(TokenKind::RBrace)
            .map_err(|error| error.with_label(Label::secondary(start, "block opened here")))?;
        Ok(Block {
            id: crate::parser::ast::BodyId::fresh(),
            stmts,
            span: start.to(end),
        })
    }

    /// Skip to the start of the next statement after a statement-level parse
    /// error: past the next `;` (consumed), or stop before `}`/EOF. Nested
    /// braces are skipped wholesale so a malformed statement containing a block
    /// does not desynchronize the enclosing block.
    fn recover_to_next_stmt(&mut self) {
        let mut brace_depth = 0usize;
        while !self.at_eof() {
            match self.peek_kind() {
                TokenKind::Semicolon if brace_depth == 0 => {
                    self.advance();
                    return;
                }
                TokenKind::LBrace => {
                    brace_depth += 1;
                    self.advance();
                }
                TokenKind::RBrace => {
                    if brace_depth == 0 {
                        return; // enclosing block's close — let parse_block see it
                    }
                    brace_depth -= 1;
                    self.advance();
                }
                _ => self.advance(),
            }
        }
    }

    pub(super) fn parse_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        match self.peek_kind().clone() {
            TokenKind::Let => self.parse_let(),
            TokenKind::Const => Err(self
                .err(
                    ErrorCode::E0105,
                    "`const` declarations are only allowed at module level",
                )
                .with_help("move it outside the function, or use `let` for a local value")),
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::Break => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::Semicolon)?;
                Ok(Stmt::Break(span))
            }
            TokenKind::Continue => {
                let span = self.current_span();
                self.advance();
                self.expect(TokenKind::Semicolon)?;
                Ok(Stmt::Continue(span))
            }
            TokenKind::Defer => {
                let span = self.current_span();
                self.advance();
                let body = match self.peek_kind() {
                    TokenKind::Return | TokenKind::Break | TokenKind::Continue => {
                        let keyword = match self.peek_kind() {
                            TokenKind::Return => "return",
                            TokenKind::Break => "break",
                            TokenKind::Continue => "continue",
                            _ => unreachable!(),
                        };
                        return Err(self.err(
                            ErrorCode::E0905,
                            format!("`{keyword}` is not allowed inside a `defer`"),
                        ));
                    }
                    TokenKind::LBrace => DeferBody::Block(self.parse_block()?),
                    _ => DeferBody::Expr(self.parse_expr()?),
                };
                // Calls remain ordinary semicolon-terminated statements.
                // Match and block bodies are block-like and accept an optional
                // trailing semicolon.
                match &body {
                    DeferBody::Expr(Expr::Match(_)) | DeferBody::Block(_) => {
                        if self.check(TokenKind::Semicolon) {
                            self.advance();
                        }
                    }
                    DeferBody::Expr(_) => {
                        self.expect(TokenKind::Semicolon)?;
                    }
                }
                Ok(Stmt::Defer(DeferStmt { body, span }))
            }
            TokenKind::For => self.parse_for(),
            TokenKind::Return => self.parse_return(),
            // `match s { ... }` is block-like as a statement (arms may use
            // `return`/blocks); tolerate an optional trailing `;` (willow-zvkv).
            TokenKind::Match => {
                let span = self.current_span();
                let expr = self.parse_match_expr()?;
                if self.check(TokenKind::Semicolon) {
                    self.advance();
                }
                Ok(Stmt::Expr(ExprStmt { expr, span }))
            }
            // `select { ... }` is block-like as a statement; tolerate an optional
            // trailing `;`.
            TokenKind::Select => {
                let span = self.current_span();
                let expr = self.parse_select()?;
                if self.check(TokenKind::Semicolon) {
                    self.advance();
                }
                Ok(Stmt::Expr(ExprStmt { expr, span }))
            }
            _ if self.is_lock_stmt_ahead() => self.parse_lock(),
            _ if self.is_super_init_ahead() => self.parse_super_init(),
            _ if self.is_field_assign_ahead() => self.parse_field_assign(),
            TokenKind::Ident(name) if self.is_assign_ahead() => self.parse_assign(name),
            // `self = expr` — parse as assignment for the type checker to reject.
            TokenKind::SelfKw if self.is_assign_ahead() => {
                self.parse_receiver_direct_assign("self")
            }
            _ => self.parse_expr_stmt(),
        }
    }

    /// Detects `lock <target> as ...` at statement position.
    ///
    /// `lock` is a CONTEXTUAL keyword, not a reserved word: it starts a lock
    /// statement only when the next token can begin the lock target and cannot
    /// continue an expression that starts with a variable named `lock`. So
    /// `lock.field = x;`, `lock = x;`, `lock(1);`, `lock[0] = 1;` and `lock;`
    /// all keep parsing as ordinary statements, and `m.lock()` is untouched
    /// because it is never at statement start with `lock` as the first token.
    ///
    /// `Ident Ident` and `Ident self` are not valid statement starts in Willow
    /// today, so this lookahead has no false positives. A parenthesized target
    /// (`lock (m) as v { ... }`) is deliberately NOT recognized, because
    /// `lock(1);` must stay a call to a function named `lock`.
    pub(super) fn is_lock_stmt_ahead(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|t| &t.kind),
            Some(TokenKind::Ident(name)) if name == "lock"
        ) && matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Ident(_) | TokenKind::SelfKw)
        )
    }

    /// `lock <expr> as [mut] <ident> { ... }`
    /// `lock read <expr> as <ident> { ... }`
    /// `lock write <expr> as [mut] <ident> { ... }`
    pub(super) fn parse_lock(&mut self) -> Result<Stmt, Diagnostic> {
        let keyword_span = self.current_span();
        self.advance(); // `lock`

        // `read` / `write` are contextual keywords, and only in this one
        // position. `lock read as v { ... }` therefore still locks a variable
        // NAMED `read`: a following `as` means the word was the target.
        let mode = match self.peek_kind().clone() {
            TokenKind::Ident(word)
                if (word == "read" || word == "write")
                    && !matches!(
                        self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::As)
                    ) =>
            {
                self.advance();
                if word == "read" {
                    LockMode::Read
                } else {
                    LockMode::Write
                }
            }
            _ => LockMode::Mutex,
        };

        let target = self.parse_expr_before_as()?;
        self.expect(TokenKind::As)?;

        let mut_span = self.current_span();
        let mutable = self.eat(TokenKind::Mut);
        if mutable && !mode.allows_mut_binding() {
            return Err(self
                .err_at(
                    ErrorCode::E2601,
                    "a `lock read` binding cannot be `mut`",
                    mut_span,
                )
                .with_help("use `lock write` to mutate the protected value"));
        }

        let binding_span = self.current_span();
        let binding = self.expect_ident()?;
        let body = self.parse_block()?;
        let span = keyword_span.to(body.span);

        Ok(Stmt::Lock(LockStmt {
            mode,
            target,
            binding,
            binding_span,
            mutable,
            body,
            span,
        }))
    }

    pub(super) fn is_super_init_ahead(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|t| &t.kind),
            Some(TokenKind::Ident(name)) if name == "super"
        ) && matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Dot)
        ) && matches!(
            self.tokens.get(self.pos + 2).map(|t| &t.kind),
            Some(TokenKind::Ident(name)) if name == "init"
        ) && matches!(
            self.tokens.get(self.pos + 3).map(|t| &t.kind),
            Some(TokenKind::LParen)
        )
    }

    pub(super) fn is_assign_ahead(&self) -> bool {
        matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind), Some(TokenKind::Eq))
        // not ==
        && !matches!(self.tokens.get(self.pos + 2).map(|t| &t.kind), Some(TokenKind::Eq))
    }

    /// Detects `(self|ident).field = value` — one-level field assignment.
    pub(super) fn is_field_assign_ahead(&self) -> bool {
        let t0_ok = matches!(
            self.tokens.get(self.pos).map(|t| &t.kind),
            Some(TokenKind::SelfKw) | Some(TokenKind::Ident(_))
        );
        t0_ok
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Dot)
            )
            && matches!(
                self.tokens.get(self.pos + 2).map(|t| &t.kind),
                Some(TokenKind::Ident(_))
            )
            && matches!(
                self.tokens.get(self.pos + 3).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
            && !matches!(
                self.tokens.get(self.pos + 4).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
    }

    pub(super) fn parse_field_assign(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        let object = match self.peek_kind().clone() {
            TokenKind::SelfKw => {
                let s = self.current_span();
                self.advance();
                Expr::Var("self".to_string(), s, ExprId::fresh())
            }
            TokenKind::Ident(name) => {
                let s = self.current_span();
                self.advance();
                Expr::Var(name, s, ExprId::fresh())
            }
            _ => unreachable!("is_field_assign_ahead checked"),
        };
        self.expect(TokenKind::Dot)?;
        let target_span = self.current_span();
        let field = self.expect_ident()?;
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        let end = self.previous_span();
        let stmt_span = span.to(end);
        Ok(Stmt::FieldAssign(FieldAssignStmt {
            target_span,
            object,
            field,
            value,
            span: stmt_span,
        }))
    }

    pub(super) fn parse_super_init(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        let super_name = self.expect_ident()?;
        debug_assert_eq!(super_name, "super");
        self.expect(TokenKind::Dot)?;
        let init_name = self.expect_ident()?;
        debug_assert_eq!(init_name, "init");
        self.expect(TokenKind::LParen)?;
        let args = self.parse_call_args_after_lparen()?;
        self.expect(TokenKind::Semicolon)?;
        let end = self.previous_span();
        Ok(Stmt::SuperInit(SuperInitStmt {
            args,
            span: span.to(end),
        }))
    }

    pub(super) fn parse_let(&mut self) -> Result<Stmt, Diagnostic> {
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
        let span = span.to(self.current_span());
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Let(LetStmt {
            name,
            mutable,
            ty,
            init,
            span,
        }))
    }

    pub(super) fn parse_assign(&mut self, name: String) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.advance(); // consume ident
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Assign(AssignStmt { name, value, span }))
    }

    /// Parse `self = expr;` as an AssignStmt so the type checker
    /// can emit "cannot assign to receiver" with a good diagnostic.
    pub(super) fn parse_receiver_direct_assign(&mut self, name: &str) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.advance(); // consume SelfKw
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Assign(AssignStmt {
            name: name.to_string(),
            value,
            span,
        }))
    }

    /// Parse `if c { } [else if c { }]* [else { }]`. `else if` is sugar for
    /// `else { if ... }`: each later rung becomes the only statement of a
    /// synthetic else block, so later phases see ordinary nested `IfStmt`s.
    /// Rungs are collected in a loop and folded from the last one, so a long
    /// ladder costs no parser recursion.
    ///
    /// Any rung may be `if let P = e { }` (willow-jz15.6). It folds into
    /// `match e { P => { then }, _ => { else } }`, so pattern checking,
    /// exhaustiveness, binding scopes and GC roots are the `match` ones.
    pub(super) fn parse_if(&mut self) -> Result<Stmt, Diagnostic> {
        enum Head {
            Cond(Expr),
            Let(Pattern, Expr),
        }
        let mut rungs = Vec::new();
        let mut else_block = loop {
            let span = self.current_span();
            self.expect(TokenKind::If)?;
            let head = if self.eat(TokenKind::Let) {
                let (pattern, scrutinee) = self.parse_let_head()?;
                Head::Let(pattern, scrutinee)
            } else {
                Head::Cond(self.parse_control_head()?)
            };
            let then_block = self.parse_block()?;
            rungs.push((span, head, then_block));
            if !self.eat(TokenKind::Else) {
                break None;
            }
            if !self.check(TokenKind::If) {
                break Some(self.parse_block()?);
            }
        };
        loop {
            let (span, head, then_block) = rungs.pop().expect("parse_if pushes a rung");
            let end = else_block
                .as_ref()
                .map_or(then_block.span, |b: &Block| b.span);
            let stmt = match head {
                Head::Cond(cond) => Stmt::If(IfStmt {
                    cond,
                    then_block,
                    else_block,
                    span,
                }),
                Head::Let(pattern, scrutinee) => {
                    let fallback = else_block.unwrap_or_else(|| Block {
                        id: crate::parser::ast::BodyId::fresh(),
                        stmts: Vec::new(),
                        span: then_block.span,
                    });
                    Self::let_match(
                        MatchSource::IfLet,
                        span.to(end),
                        pattern,
                        scrutinee,
                        then_block,
                        fallback,
                    )
                }
            };
            if rungs.is_empty() {
                return Ok(stmt);
            }
            else_block = Some(Block {
                id: crate::parser::ast::BodyId::fresh(),
                stmts: vec![stmt],
                span: span.to(end),
            });
        }
    }

    /// `while c { }`, or `while let P = e { }` (willow-jz15.6), which becomes
    /// `while true { match e { P => { body }, _ => { break; } } }`: the
    /// scrutinee is re-evaluated before every iteration, `continue` re-tests
    /// it, and `break` leaves the loop.
    pub(super) fn parse_while(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::While)?;
        if self.eat(TokenKind::Let) {
            let (pattern, scrutinee) = self.parse_let_head()?;
            let body = self.parse_block()?;
            let whole = span.to(body.span);
            let exit = Block {
                id: crate::parser::ast::BodyId::fresh(),
                stmts: vec![Stmt::Break(span)],
                span: body.span,
            };
            let matched =
                Self::let_match(MatchSource::WhileLet, whole, pattern, scrutinee, body, exit);
            return Ok(Stmt::While(WhileStmt {
                cond: Expr::Bool(true, span, ExprId::fresh()),
                body: Block {
                    id: crate::parser::ast::BodyId::fresh(),
                    stmts: vec![matched],
                    span: whole,
                },
                span,
            }));
        }
        let cond = self.parse_control_head()?;
        let body = self.parse_block()?;
        Ok(Stmt::While(WhileStmt { cond, body, span }))
    }

    /// `P = e` after `if let` / `while let`. The scrutinee is a control head,
    /// so `{` opens the body rather than an object literal.
    fn parse_let_head(&mut self) -> Result<(Pattern, Expr), Diagnostic> {
        let pattern = self.parse_pattern()?;
        self.expect(TokenKind::Eq)?;
        let scrutinee = self.parse_control_head()?;
        Ok((pattern, scrutinee))
    }

    /// The two-arm `match` an `if let` / `while let` stands for.
    fn let_match(
        source: MatchSource,
        span: Span,
        pattern: Pattern,
        scrutinee: Expr,
        then_block: Block,
        fallback: Block,
    ) -> Stmt {
        let then_span = pattern.span().to(then_block.span);
        let fallback_span = fallback.span;
        Stmt::Expr(ExprStmt {
            expr: Expr::Match(Box::new(MatchExpr {
                id: ExprId::fresh(),
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    MatchArm {
                        pattern,
                        guard: None,
                        body: MatchBody::Block(then_block),
                        span: then_span,
                    },
                    MatchArm {
                        pattern: Pattern::Wildcard(fallback_span, PatternId::fresh()),
                        guard: None,
                        body: MatchBody::Block(fallback),
                        span: fallback_span,
                    },
                ],
                span,
                source,
            })),
            span,
        })
    }

    pub(super) fn parse_for(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::For)?;
        let name_span = self.current_span();
        let name = self.expect_ident()?;
        self.expect(TokenKind::In)?;
        let iterable = self.parse_control_head()?;
        let body = self.parse_block()?;
        Ok(Stmt::For(ForStmt {
            name,
            name_span,
            iterable,
            body,
            span,
        }))
    }

    pub(super) fn parse_return(&mut self) -> Result<Stmt, Diagnostic> {
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

    pub(super) fn parse_expr_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        let mut expr = self.parse_expr()?;
        if let Some(op) = self.shift_assign_op() {
            self.advance();
            self.advance();
            let rhs = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            return self.make_compound_assignment(expr, op, rhs, span);
        }
        if let Some(op) = compound_assignment_op(self.peek_kind()) {
            self.advance();
            let rhs = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            return self.make_compound_assignment(expr, op, rhs, span);
        }
        // `array[index] = value;` — element assignment. Detected after parsing
        // the lvalue expression because the index can be an arbitrary expression
        // (fixed lookahead cannot find the `=`).
        if matches!(expr, Expr::Index(..))
            && matches!(self.peek_kind(), TokenKind::Eq)
            && !matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
        {
            self.advance(); // consume `=`
            let value = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            let Expr::Index(array, index, idx_span, _) = &mut expr else {
                unreachable!("checked Expr::Index above");
            };
            return Ok(Stmt::IndexAssign(IndexAssignStmt {
                array: array.take(),
                index: index.take(),
                value,
                span: *idx_span,
            }));
        }
        // `ClassName::property = value;` — static property assignment. Detected
        // after parsing the lvalue (a StaticField). The type checker enforces
        // mutability (immutable → E0832); codegen stores into global storage.
        if matches!(expr, Expr::StaticField(_))
            && matches!(self.peek_kind(), TokenKind::Eq)
            && !matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
        {
            self.advance(); // consume `=`
            let value = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            let Expr::StaticField(sf) = &mut expr else {
                unreachable!("checked Expr::StaticField above");
            };
            return Ok(Stmt::StaticFieldAssign(StaticFieldAssignStmt {
                class: std::mem::take(&mut sf.class),
                field: std::mem::take(&mut sf.field),
                value,
                span: sf.span,
            }));
        }
        // `obj.field = value;` where `obj` is an arbitrary place expression
        // (`a.b.v`, `ps[0].x`, `make().x`). The one-level `x.f = v` fast path is
        // caught by lookahead in `parse_stmt`; this handles the general case
        // after the lvalue has been parsed (willow-qzxg).
        if matches!(expr, Expr::FieldAccess(..))
            && matches!(self.peek_kind(), TokenKind::Eq)
            && !matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
        {
            self.advance(); // consume `=`
            let value = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            let Expr::FieldAccess(object, field, fa_span, _) = &mut expr else {
                unreachable!("checked Expr::FieldAccess above");
            };
            return Ok(Stmt::FieldAssign(FieldAssignStmt {
                target_span: *fa_span,
                object: object.take(),
                field: std::mem::take(field),
                value,
                span: *fa_span,
            }));
        }
        // Any other expression followed by `=` is an invalid assignment target;
        // report it as such instead of a misleading `expected ;` (willow-qzxg).
        if matches!(self.peek_kind(), TokenKind::Eq)
            && !matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Eq)
            )
        {
            let eq_span = self.current_span();
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0106,
                "invalid assignment target",
            )
            .with_label(Label::primary(
                eq_span,
                "cannot assign to this expression",
            ))
            .with_help(
                "assignable targets are variables, fields (`obj.field`), indexes (`arr[i]`), and static properties (`Class::prop`)",
            ));
        }
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Expr(ExprStmt { expr, span }))
    }

    fn make_compound_assignment(
        &self,
        mut lhs: Expr,
        op: BinOp,
        rhs: Expr,
        span: crate::diagnostics::Span,
    ) -> Result<Stmt, Diagnostic> {
        let binary = |left: Expr| {
            Expr::Binary(Box::new(BinaryExpr {
                id: ExprId::fresh(),
                op,
                lhs: left,
                rhs,
                span,
            }))
        };
        let temp_var = |name: &str| Expr::Var(name.to_owned(), span, ExprId::fresh());
        let temp_let = |name: &str, init| {
            Stmt::Let(LetStmt {
                name: name.to_owned(),
                mutable: false,
                ty: None,
                init,
                span,
            })
        };
        let scoped = |stmts| {
            Stmt::If(IfStmt {
                cond: Expr::Bool(true, span, ExprId::fresh()),
                then_block: Block {
                    id: BodyId::fresh(),
                    stmts,
                    span,
                },
                else_block: None,
                span,
            })
        };
        match &mut lhs {
            Expr::Var(name, _, _) => Ok(Stmt::Assign(AssignStmt {
                value: binary(temp_var(name)),
                name: std::mem::take(name),
                span,
            })),
            Expr::FieldAccess(object, field, target_span, _) => {
                // The receiver is captured before the read and the RHS. `self`
                // is a stable binding; keep constructor field initialization
                // visible to definite-assignment analysis.
                let is_self = matches!(&**object, Expr::Var(name, _, _) if name == "self");
                let receiver = format!("$compound_receiver_{}", span.start);
                let read_object = temp_var(if is_self { "self" } else { &receiver });
                let store_object = temp_var(if is_self { "self" } else { &receiver });
                let read = Expr::FieldAccess(
                    Box::new(read_object),
                    field.clone(),
                    *target_span,
                    ExprId::fresh(),
                );
                let assignment = Stmt::FieldAssign(FieldAssignStmt {
                    target_span: *target_span,
                    object: store_object,
                    field: std::mem::take(field),
                    value: binary(read),
                    span,
                });
                if is_self {
                    Ok(assignment)
                } else {
                    Ok(scoped(vec![temp_let(&receiver, object.take()), assignment]))
                }
            }
            Expr::Index(array, index, target_span, _) => {
                let receiver = format!("$compound_array_{}", span.start);
                let subscript = format!("$compound_index_{}", span.start);
                let read = Expr::Index(
                    Box::new(temp_var(&receiver)),
                    Box::new(temp_var(&subscript)),
                    *target_span,
                    ExprId::fresh(),
                );
                let assignment = Stmt::IndexAssign(IndexAssignStmt {
                    array: temp_var(&receiver),
                    index: temp_var(&subscript),
                    value: binary(read),
                    span,
                });
                Ok(scoped(vec![
                    temp_let(&receiver, array.take()),
                    temp_let(&subscript, index.take()),
                    assignment,
                ]))
            }
            _ => Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0106,
                "invalid compound assignment target",
            )
            .with_label(Label::primary(span, "cannot assign to this expression"))),
        }
    }
}

fn compound_assignment_op(token: &TokenKind) -> Option<BinOp> {
    Some(match token {
        TokenKind::PlusEq => BinOp::Add,
        TokenKind::MinusEq => BinOp::Sub,
        TokenKind::StarEq => BinOp::Mul,
        TokenKind::SlashEq => BinOp::Div,
        TokenKind::PercentEq => BinOp::Rem,
        TokenKind::AmpersandEq => BinOp::BitAnd,
        TokenKind::PipeEq => BinOp::BitOr,
        TokenKind::CaretEq => BinOp::BitXor,
        _ => return None,
    })
}

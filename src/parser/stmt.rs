use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::TokenKind;

impl Parser {
    pub(super) fn parse_block(&mut self) -> Result<Block, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
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
        self.expect(TokenKind::RBrace)?;
        Ok(Block {
            stmts,
            span: Span::new(start.start, end.end, start.line, start.col),
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
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
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
                Expr::Var("self".to_string(), s)
            }
            TokenKind::Ident(name) => {
                let s = self.current_span();
                self.advance();
                Expr::Var(name, s)
            }
            _ => unreachable!("is_field_assign_ahead checked"),
        };
        self.expect(TokenKind::Dot)?;
        let field = self.expect_ident()?;
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        let end = self.previous_span();
        let stmt_span = Span::new(span.start, end.end, span.line, span.col);
        Ok(Stmt::FieldAssign(FieldAssignStmt {
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
            span: Span::new(span.start, end.end, span.line, span.col),
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

    pub(super) fn parse_if(&mut self) -> Result<Stmt, Diagnostic> {
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

    pub(super) fn parse_while(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::While(WhileStmt { cond, body, span }))
    }

    pub(super) fn parse_for(&mut self) -> Result<Stmt, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::For)?;
        let name_span = self.current_span();
        let name = self.expect_ident()?;
        self.expect(TokenKind::In)?;
        let iterable = self.parse_expr()?;
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
        let expr = self.parse_expr()?;
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
            let Expr::Index(array, index, idx_span) = expr else {
                unreachable!("checked Expr::Index above");
            };
            return Ok(Stmt::IndexAssign(IndexAssignStmt {
                array: *array,
                index: *index,
                value,
                span: idx_span,
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
            let Expr::StaticField(sf) = expr else {
                unreachable!("checked Expr::StaticField above");
            };
            return Ok(Stmt::StaticFieldAssign(StaticFieldAssignStmt {
                class: sf.class,
                field: sf.field,
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
            let Expr::FieldAccess(object, field, fa_span) = expr else {
                unreachable!("checked Expr::FieldAccess above");
            };
            return Ok(Stmt::FieldAssign(FieldAssignStmt {
                object: *object,
                field,
                value,
                span: fa_span,
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
}

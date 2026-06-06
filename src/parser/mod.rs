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
        let mut module = None;
        let mut imports = Vec::new();
        let mut items = Vec::new();
        let mut errors = Vec::new();

        // An optional `module path;` declaration must come first.
        if matches!(self.peek_kind(), TokenKind::Module) {
            match self.parse_module_decl() {
                Ok(decl) => module = Some(decl),
                Err(e) => {
                    errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }

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
            // A `module` declaration here is misplaced (after imports/items) or
            // a duplicate; report and skip it rather than treating it as an item.
            if matches!(self.peek_kind(), TokenKind::Module) {
                let span = self.current_span();
                let (code, msg) = if module.is_some() {
                    (ErrorCode::E2009, "duplicate module declaration")
                } else {
                    (
                        ErrorCode::E2008,
                        "module declaration must appear before imports and items",
                    )
                };
                errors.push(
                    Diagnostic::new(Severity::Error, code, msg)
                        .with_label(Label::primary(span, "unexpected `module` declaration")),
                );
                self.recover_to_next_item();
                continue;
            }
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(e) => {
                    errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }

        (
            Program {
                module,
                imports,
                items,
            },
            errors,
        )
    }

    fn parse_module_decl(&mut self) -> Result<ModuleDecl, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Module)?;
        let path = self.parse_module_path()?;
        self.expect(TokenKind::Semicolon)?;
        // `std` is a reserved namespace; user files may not claim it.
        if path == "std" || path.starts_with("std::") {
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E2010,
                "`std` is a reserved namespace and cannot be declared as a module",
            )
            .with_label(Label::primary(span, "reserved module namespace"))
            .with_help("choose a different module name"));
        }
        Ok(ModuleDecl { path, span })
    }

    fn parse_module_path(&mut self) -> Result<String, Diagnostic> {
        // Module paths use `::` only (like imports); `.` is reserved for member
        // access.
        let mut parts = vec![self.expect_path_segment()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }
        Ok(parts.join("::"))
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
        let mut parts = vec![self.expect_path_segment()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }
        Ok(parts.join("::"))
    }

    /// Like [`expect_ident`], but also accepts builtin names that are lexed as
    /// keywords (`print`, `println`) so they can appear as import-path segments,
    /// e.g. `import std::io::println;`.
    fn expect_path_segment(&mut self) -> Result<String, Diagnostic> {
        match self.peek_kind() {
            TokenKind::Print => {
                self.advance();
                Ok("print".to_string())
            }
            TokenKind::Println => {
                self.advance();
                Ok("println".to_string())
            }
            _ => self.expect_ident(),
        }
    }

    fn parse_item(&mut self) -> Result<Item, Diagnostic> {
        let public = self.eat(TokenKind::Pub);
        let is_open = self.eat(TokenKind::Open);
        let is_async = self.eat(TokenKind::Async);
        match self.peek_kind() {
            TokenKind::Fn => Ok(Item::Function(self.parse_fn(public, is_async)?)),
            TokenKind::Class => Ok(Item::Class(self.parse_class(public, is_open)?)),
            TokenKind::Enum => Ok(Item::Enum(self.parse_enum_decl(public)?)),
            TokenKind::Interface if is_open => Err(self.err(
                ErrorCode::E0105,
                "`open` cannot be used on an interface declaration",
            )),
            TokenKind::Interface if is_async => Err(self.err(
                ErrorCode::E0105,
                "`async` cannot be used on an interface declaration",
            )),
            TokenKind::Interface => Ok(Item::Interface(self.parse_interface(public)?)),
            _ if is_async => Err(self.err(ErrorCode::E0105, "`async` can only be used on `fn`")),
            _ => Err(self.err(
                ErrorCode::E0105,
                "expected `fn`, `class`, `enum`, or `interface`",
            )),
        }
    }

    fn parse_enum_decl(&mut self, public: bool) -> Result<EnumDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        // Optional generic type parameters: `<T>` or `<T, E>`
        let mut type_params = Vec::new();
        if self.eat(TokenKind::Lt) {
            while !matches!(self.peek_kind(), TokenKind::Gt | TokenKind::Eof) {
                type_params.push(self.expect_ident()?);
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.advance();
                }
            }
            self.expect(TokenKind::Gt)?;
        }
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
            variants.push(EnumVariant {
                name: v_name,
                payload,
                span: v_span,
            });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.advance();
            }
        }
        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = Span::new(start.start, end.end, start.line, start.col);
        Ok(EnumDecl {
            name,
            public,
            type_params,
            variants,
            span,
        })
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

        // `implements I, J` — comes after `extends` if both are present. Parsed
        // as types so generic interfaces (`implements From<E>`) keep their args.
        let mut implements = Vec::new();
        if self.eat(TokenKind::Implements) {
            implements.push(self.parse_type()?);
            while self.eat(TokenKind::Comma) {
                implements.push(self.parse_type()?);
            }
        }

        self.expect(TokenKind::LBrace)?;

        let mut fields = Vec::new();
        let mut methods = Vec::new();

        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let member_public = self.eat(TokenKind::Pub);
            let member_prot = if !member_public {
                self.eat(TokenKind::Prot)
            } else {
                false
            };
            // `static` marks a class-level member (willow-qsqf). It sits after
            // visibility and before `open`/`override`/`async` for methods, and
            // before `mut` for a mutable static property.
            let member_static = self.eat(TokenKind::Static);
            let member_open = self.eat(TokenKind::Open);
            let member_override = self.eat(TokenKind::Override);
            let member_async = self.eat(TokenKind::Async);

            if self.check(TokenKind::Fn) {
                methods.push(self.parse_method(
                    member_public,
                    member_prot,
                    member_async,
                    member_open,
                    member_override,
                    member_static,
                )?);
            } else {
                if member_open || member_override || member_async {
                    return Err(self.err(
                        ErrorCode::E0105,
                        "`open`, `override`, and `async` can only be used on methods",
                    ));
                }
                fields.push(self.parse_field(member_public, member_prot, member_static)?);
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
            implements,
            fields,
            methods,
            span,
        })
    }

    fn parse_interface(&mut self, public: bool) -> Result<InterfaceDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Interface)?;
        let name = self.expect_ident()?;
        // Optional generic type parameters: `<T>` or `<T, U>` (willow-1js.1).
        let mut type_params = Vec::new();
        if self.eat(TokenKind::Lt) {
            while !matches!(self.peek_kind(), TokenKind::Gt | TokenKind::Eof) {
                type_params.push(self.expect_ident()?);
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.advance();
                }
            }
            self.expect(TokenKind::Gt)?;
        }
        // Optional super-interfaces: `interface B extends A` (willow-1js.2).
        let mut extends = Vec::new();
        if self.eat(TokenKind::Extends) {
            extends.push(self.parse_module_path()?);
            while self.eat(TokenKind::Comma) {
                extends.push(self.parse_module_path()?);
            }
        }
        self.expect(TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            // Interface members carry no visibility/modifier keywords: methods are
            // public by contract. A leading `pub`/`prot`/`open`/etc. is not allowed.
            if !self.check(TokenKind::Fn) {
                // Distinguish a stray field (`value: i64;`) from other junk for a
                // clearer diagnostic.
                let span = self.current_span();
                if matches!(self.peek_kind(), TokenKind::Ident(_)) {
                    return Err(Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0421,
                        "interface fields are not allowed",
                    )
                    .with_label(Label::primary(span, "interfaces declare methods only"))
                    .with_help("move state into the implementing class"));
                }
                return Err(self.err(
                    ErrorCode::E0105,
                    "expected an interface method (`fn name(...) -> Type;`)",
                ));
            }
            methods.push(self.parse_interface_method()?);
        }

        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = Span::new(start.start, end.end, start.line, start.col);

        Ok(InterfaceDecl {
            name,
            public,
            type_params,
            extends,
            methods,
            span,
        })
    }

    fn parse_interface_method(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
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

        // A method with a body (`{ ... }`) is a DEFAULT method (willow-1js.3);
        // a method ending in `;` is signature-only (required). A default method
        // must take `self` — there is no receiver to run a static default on.
        let (default_body, end) = if self.check(TokenKind::LBrace) {
            if !has_self {
                let span = self.current_span();
                return Err(Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0420,
                    "a default interface method must take `self`",
                )
                .with_label(Label::primary(span, "default body needs a `self` receiver"))
                .with_help(
                    "add `self` as the first parameter, or remove the body to make it required",
                ));
            }
            let body = self.parse_block()?;
            let end = self.previous_span();
            (Some(body), end)
        } else {
            let end = self.current_span();
            self.expect(TokenKind::Semicolon)?;
            (None, end)
        };
        let span = Span::new(start.start, end.end, start.line, start.col);

        Ok(InterfaceMethodDecl {
            name,
            params,
            has_self,
            return_type,
            default_body,
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

    fn parse_field(
        &mut self,
        public: bool,
        protected: bool,
        is_static: bool,
    ) -> Result<FieldDecl, Diagnostic> {
        let span = self.current_span();
        // `static mut name: T = expr` — `mut` is only meaningful on a static
        // property (instance fields take their mutability from the binding).
        let is_mut = self.eat(TokenKind::Mut);
        if is_mut && !is_static {
            return Err(self.err(
                ErrorCode::E0105,
                "`mut` on a class field is only allowed on a `static` property",
            ));
        }
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        // Static properties require an initializer in the MVP; instance fields
        // must not have one (they are set by object literals).
        let initializer = if self.eat(TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if is_static && initializer.is_none() {
            return Err(self.err(
                ErrorCode::E0830,
                &format!("static property `{}` requires an initializer", name),
            ));
        }
        if !is_static && initializer.is_some() {
            return Err(self.err(
                ErrorCode::E0105,
                "instance fields cannot have an initializer; set them in an object literal",
            ));
        }
        self.expect(TokenKind::Semicolon)?;
        Ok(FieldDecl {
            name,
            ty,
            public,
            protected,
            is_static,
            is_mut,
            initializer,
            span,
        })
    }

    fn parse_method(
        &mut self,
        public: bool,
        protected: bool,
        is_async: bool,
        is_open: bool,
        is_override: bool,
        is_static: bool,
    ) -> Result<MethodDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;

        let mut has_self = false;
        let mut params = Vec::new();

        if self.check(TokenKind::SelfKw) {
            // A static method has no receiver; an explicit `self` is always wrong
            // (willow-qsqf §9.2). Instance methods take `self` implicitly, but an
            // explicit `self` is still accepted during migration.
            if is_static {
                let span = self.current_span();
                return Err(Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0831,
                    "static methods cannot take `self`",
                )
                .with_label(Label::primary(span, "`self` in a static method"))
                .with_help("remove `self`, or make this an instance method by dropping `static`"));
            }
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
            protected,
            is_async,
            is_open,
            is_override,
            is_static,
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
                } else if name == "void" {
                    // `void` is a writable spelling of the unit/no-value type
                    // (e.g. `fn f() -> void`, `Result<void, E>`).
                    Ok(Type::Void)
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
            TokenKind::For => self.parse_for(),
            TokenKind::Return => self.parse_return(),
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
            _ if self.is_field_assign_ahead() => self.parse_field_assign(),
            TokenKind::Ident(name) if self.is_assign_ahead() => self.parse_assign(name),
            // `self = expr` — parse as assignment for the type checker to reject.
            TokenKind::SelfKw if self.is_assign_ahead() => {
                self.parse_receiver_direct_assign("self")
            }
            _ => self.parse_expr_stmt(),
        }
    }

    fn is_assign_ahead(&self) -> bool {
        matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind), Some(TokenKind::Eq))
        // not ==
        && !matches!(self.tokens.get(self.pos + 2).map(|t| &t.kind), Some(TokenKind::Eq))
    }

    /// Detects `(self|ident).field = value` — one-level field assignment.
    fn is_field_assign_ahead(&self) -> bool {
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

    fn parse_field_assign(&mut self) -> Result<Stmt, Diagnostic> {
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

    /// Parse `self = expr;` as an AssignStmt so the type checker
    /// can emit "cannot assign to receiver" with a good diagnostic.
    fn parse_receiver_direct_assign(&mut self, name: &str) -> Result<Stmt, Diagnostic> {
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

    fn parse_for(&mut self) -> Result<Stmt, Diagnostic> {
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
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Expr(ExprStmt { expr, span }))
    }

    fn parse_expr(&mut self) -> Result<Expr, Diagnostic> {
        self.parse_range()
    }

    fn parse_range(&mut self) -> Result<Expr, Diagnostic> {
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
                        if is_type_constructor_name(&method)
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
                    } else if is_type_constructor_name(&member) && self.eat(TokenKind::LBrace) {
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

    fn parse_std_qualified_expr(&mut self) -> Result<Expr, Diagnostic> {
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
                    // Collect all `::`-separated segments; the last is the
                    // variant, the rest (joined) form the enum name. This
                    // accepts a module-qualified enum, e.g. `palette::Color::Red`
                    // (enum `palette::Color`, variant `Red`) (willow-64gs).
                    let mut segments = vec![name, self.expect_ident()?];
                    while self.eat(TokenKind::ColonColon) {
                        segments.push(self.expect_ident()?);
                    }
                    let variant = segments.pop().unwrap();
                    let name = segments.join("::");
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
                        Ok(Pattern::EnumVariantTuple {
                            enum_name: name,
                            variant,
                            bindings,
                            span: merged,
                        })
                    } else {
                        let end = self.current_span();
                        let merged = Span::new(span.start, end.end, span.line, span.col);
                        Ok(Pattern::EnumVariant {
                            enum_name: name,
                            variant,
                            span: merged,
                        })
                    }
                } else if matches!(self.peek_kind(), TokenKind::LParen) {
                    // `Dog(d)` — interface->concrete downcast pattern (willow-1js.4).
                    // (Enum variants are always `::`-qualified, so an unqualified
                    // name with `(` is unambiguously a class downcast.)
                    self.advance(); // consume (
                    let binding = self.expect_ident()?; // `_` lexes as an identifier
                    self.expect(TokenKind::RParen)?;
                    let end = self.current_span();
                    let merged = Span::new(span.start, end.end, span.line, span.col);
                    Ok(Pattern::ClassDowncast {
                        class_name: name,
                        binding,
                        span: merged,
                    })
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

    /// Returns true if the current `?` is a TryPropagate postfix operator
    /// (not the `?` of a ternary `cond ? then : else`).
    /// A `?` is TryPropagate when the token AFTER it cannot start an expression.
    fn is_try_propagate_question(&self) -> bool {
        // Peek at the token one position ahead of the `?`.
        let next_pos = self.pos + 1;
        let next = self.tokens.get(next_pos).map(|t| &t.kind);
        !matches!(
            next,
            Some(
                TokenKind::Integer(_)
                    | TokenKind::Float(_)
                    | TokenKind::True
                    | TokenKind::False
                    | TokenKind::Ident(_)
                    | TokenKind::LParen
                    | TokenKind::Minus
                    | TokenKind::Bang
                    | TokenKind::Ampersand
                    | TokenKind::Nil
                    | TokenKind::Match
            )
        )
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
            if name == "this" {
                return Err(self.err(
                    ErrorCode::E0102,
                    "identifier `this` is reserved; use `self` as the receiver",
                ));
            }
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
                TokenKind::Fn
                    | TokenKind::Class
                    | TokenKind::Interface
                    | TokenKind::Pub
                    | TokenKind::Prot
                    | TokenKind::Import
                    | TokenKind::Enum
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

    #[test]
    fn for_loop_01_parses_name_iterable_and_body() {
        let p = parse_ok("fn main() { for value in values { println(value); } }");
        let f = first_function(&p);
        let Stmt::For(for_stmt) = &f.body.stmts[0] else {
            panic!("expected a for statement");
        };
        assert_eq!(for_stmt.name, "value");
        assert!(matches!(for_stmt.iterable, Expr::Var(ref name, _) if name == "values"));
        assert_eq!(for_stmt.body.stmts.len(), 1);
    }

    #[test]
    fn for_loop_02_parses_underscore_binding() {
        let p = parse_ok("fn main() { for _ in values { println(1); } }");
        let f = first_function(&p);
        let Stmt::For(for_stmt) = &f.body.stmts[0] else {
            panic!("expected a for statement");
        };
        assert_eq!(for_stmt.name, "_");
        assert!(matches!(for_stmt.iterable, Expr::Var(ref name, _) if name == "values"));
    }

    #[test]
    fn for_loop_03_parses_i64_range_iterable() {
        let p = parse_ok("fn main() { for n in 1..101 { println(n); } }");
        let f = first_function(&p);
        let Stmt::For(for_stmt) = &f.body.stmts[0] else {
            panic!("expected a for statement");
        };
        assert_eq!(for_stmt.name, "n");
        let Expr::Range(range) = &for_stmt.iterable else {
            panic!("expected a range iterable");
        };
        assert!(matches!(range.start, Expr::Integer(1, _)));
        assert!(matches!(range.end, Expr::Integer(101, _)));
    }

    // ── Module declarations (willow-y0o, spec 4.1 / 20.1) ──────────────────

    #[test]
    fn module_decl_simple() {
        let p = parse_ok("module math;\nfn main() {}\n");
        assert_eq!(p.module.as_ref().map(|m| m.path.as_str()), Some("math"));
    }

    #[test]
    fn module_decl_rejects_dot_separator() {
        // Module paths use `::` only (like imports); `.` is for member access.
        let errs = parse_errors("module myapp.util;\nfn main() {}\n");
        assert!(
            !errs.is_empty(),
            "dot-separated module declarations must be rejected (use `::`)"
        );
    }

    #[test]
    fn module_decl_colon_separated() {
        let p = parse_ok("module myapp::util;\nfn main() {}\n");
        assert_eq!(
            p.module.as_ref().map(|m| m.path.as_str()),
            Some("myapp::util")
        );
    }

    #[test]
    fn module_decl_before_imports_ok() {
        let p = parse_ok("module myapp;\nimport math;\nfn main() {}\n");
        assert!(p.module.is_some());
        assert_eq!(p.imports.len(), 1);
    }

    #[test]
    fn module_decl_after_item_rejected() {
        let errs = parse_errors("fn f() {}\nmodule myapp;\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E2008));
    }

    #[test]
    fn module_decl_after_import_rejected() {
        let errs = parse_errors("import math;\nmodule myapp;\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E2008));
    }

    #[test]
    fn module_decl_duplicate_rejected() {
        let errs = parse_errors("module a;\nmodule b;\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E2009));
    }

    #[test]
    fn module_decl_std_rejected() {
        let errs = parse_errors("module std::foo;\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E2010));
    }

    #[test]
    fn module_decl_bare_std_rejected() {
        let errs = parse_errors("module std;\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E2010));
    }

    #[test]
    fn no_module_decl_is_none() {
        let p = parse_ok("fn main() {}\n");
        assert!(p.module.is_none());
    }

    #[test]
    fn import_path_uses_colons_only() {
        // Import paths use `::` exclusively; `.` is reserved for member access.
        let p = parse_ok("import std::collections::Array;\nfn main() {}\n");
        assert_eq!(p.imports.len(), 1);
        assert_eq!(p.imports[0].path, "std::collections::Array");
    }

    #[test]
    fn import_path_rejects_dot_separator() {
        // `import a.b.c;` (dot) is not accepted — only `import a::b::c;`.
        let errs = parse_errors("import std.collections.Array;\nfn main() {}\n");
        assert!(
            !errs.is_empty(),
            "dot-separated import paths must be rejected (use `::`)"
        );
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
    fn parses_module_qualified_static_call() {
        let program = parse_ok("fn main() { geom::Point::new(1, 2); }");
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_eq!(call.class, "geom::Point");
        assert_eq!(call.method, "new");
        assert_eq!(call.args.len(), 2);
    }

    #[test]
    fn parses_upper_self_static_call() {
        let program = parse_ok("fn main() { Self::new(1); }");
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_eq!(call.class, "Self");
        assert_eq!(call.method, "new");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn parses_lower_self_static_call() {
        let program = parse_ok("class C { fn f(self) { self::make(); } }");
        let class = program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Class(class) => Some(class),
                _ => None,
            })
            .expect("expected class");
        let call = match &class.methods[0].body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_eq!(call.class, "Self");
        assert_eq!(call.method, "make");
        assert!(call.args.is_empty());
    }

    #[test]
    fn parses_module_qualified_no_arg_variant_value() {
        let program = parse_ok("fn main() { geom::Color::Red; }");
        let function = first_function(&program);
        let call = match &function.body.stmts[0] {
            Stmt::Expr(ExprStmt {
                expr: Expr::StaticCall(call),
                ..
            }) => call,
            other => panic!("expected static call expression, got {other:#?}"),
        };

        assert_eq!(call.class, "geom::Color");
        assert_eq!(call.method, "Red");
        assert!(call.args.is_empty());
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

    // ── Interface declarations & implements (willow-7kw, spec 4 / 5 / 6) ────

    fn first_interface(program: &Program) -> &InterfaceDecl {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Interface(i) => Some(i),
                _ => None,
            })
            .expect("expected an interface item")
    }

    fn first_class(program: &Program) -> &ClassDecl {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Class(c) => Some(c),
                _ => None,
            })
            .expect("expected a class item")
    }

    #[test]
    fn interface_01_simple_single_method() {
        let p = parse_ok("interface Animal { fn speak(self) -> String; }");
        let i = first_interface(&p);
        assert_eq!(i.name, "Animal");
        assert!(!i.public);
        assert!(i.type_params.is_empty());
        assert_eq!(i.methods.len(), 1);
        assert_eq!(i.methods[0].name, "speak");
        assert_eq!(i.methods[0].return_type, Type::String);
        assert!(i.methods[0].has_self);
    }

    #[test]
    fn interface_extends_single_and_multiple() {
        let p = parse_ok("interface Pet extends Animal { fn owner(self) -> String; }");
        let i = first_interface(&p);
        assert_eq!(i.extends, vec!["Animal".to_string()]);

        let p2 = parse_ok("interface C extends A, B { fn c(self) -> i64; }");
        assert_eq!(
            first_interface(&p2).extends,
            vec!["A".to_string(), "B".to_string()]
        );

        // No extends clause -> empty.
        let p3 = parse_ok("interface Plain { fn f(self) -> i64; }");
        assert!(first_interface(&p3).extends.is_empty());
    }

    #[test]
    fn interface_generic_single_type_param() {
        // `interface Box<T> { fn get(self) -> T; }` (willow-1js.1).
        let p = parse_ok("interface Box<T> { fn get(self) -> T; }");
        let i = first_interface(&p);
        assert_eq!(i.name, "Box");
        assert_eq!(i.type_params, vec!["T".to_string()]);
        assert_eq!(i.methods[0].return_type, Type::Named("T".to_string()));
    }

    #[test]
    fn interface_generic_two_type_params() {
        let p = parse_ok("interface Conv<A, B> { fn run(self, a: A) -> B; }");
        let i = first_interface(&p);
        assert_eq!(i.type_params, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn interface_02_pub_interface() {
        // NB: `print`/`println` are builtin keywords and cannot be method names
        // (a pre-existing limitation that also applies to class methods), so the
        // method here is `render`.
        let p = parse_ok("pub interface Printable { fn render(self); }");
        let i = first_interface(&p);
        assert!(i.public);
        assert_eq!(i.name, "Printable");
    }

    #[test]
    fn interface_03_multiple_methods_preserve_order() {
        let p = parse_ok(
            "interface Animal { fn speak(self) -> String; fn name(self) -> String; fn legs(self) -> i64; }",
        );
        let i = first_interface(&p);
        let names: Vec<&str> = i.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["speak", "name", "legs"]);
    }

    #[test]
    fn interface_04_method_without_return_type_is_void() {
        let p = parse_ok("interface Sink { fn push(self, x: i64); }");
        let i = first_interface(&p);
        assert_eq!(i.methods[0].return_type, Type::Void);
    }

    #[test]
    fn interface_05_method_self_with_extra_params() {
        let p = parse_ok("interface Adder { fn add(self, a: i64, b: i64) -> i64; }");
        let m = &first_interface(&p).methods[0];
        assert!(m.has_self);
        assert_eq!(m.params.len(), 2);
        assert_eq!(m.params[0].ty, Type::I64);
        assert_eq!(m.params[1].ty, Type::I64);
        assert_eq!(m.return_type, Type::I64);
    }

    #[test]
    fn interface_06_method_without_self() {
        let p = parse_ok("interface Factory { fn make(x: i64) -> i64; }");
        let m = &first_interface(&p).methods[0];
        assert!(!m.has_self);
        assert_eq!(m.params.len(), 1);
    }

    #[test]
    fn interface_07_empty_interface() {
        let p = parse_ok("interface Marker {}");
        assert!(first_interface(&p).methods.is_empty());
    }

    #[test]
    fn interface_08_class_implements_single() {
        let p = parse_ok(
            "class Dog implements Animal { pub fn speak(self) -> String { return \"woof\"; } }",
        );
        let c = first_class(&p);
        assert_eq!(c.implements.len(), 1);
        assert_eq!(c.implements[0], Type::Named("Animal".to_string()));
        assert!(c.base_class.is_none());
    }

    #[test]
    fn interface_09_class_implements_multiple() {
        let p = parse_ok("class Dog implements Animal, Printable {}");
        let c = first_class(&p);
        assert_eq!(
            c.implements,
            vec![
                Type::Named("Animal".to_string()),
                Type::Named("Printable".to_string())
            ]
        );
    }

    #[test]
    fn interface_10_extends_then_implements() {
        let p = parse_ok("class Dog extends Mammal implements Animal, Printable {}");
        let c = first_class(&p);
        assert_eq!(c.base_class.as_ref().map(|b| b.name()), Some("Mammal"));
        assert_eq!(c.implements.len(), 2);
    }

    #[test]
    fn interface_11_implements_without_extends() {
        let p = parse_ok("class Dog implements Animal {}");
        let c = first_class(&p);
        assert!(c.base_class.is_none());
        assert_eq!(c.implements.len(), 1);
    }

    #[test]
    fn interface_12_no_implements_is_empty() {
        let p = parse_ok("class Dog {}");
        assert!(first_class(&p).implements.is_empty());
    }

    #[test]
    fn interface_13_qualified_interface_path() {
        let p = parse_ok("class Dog implements animals::Animal {}");
        let c = first_class(&p);
        assert_eq!(c.implements[0], Type::Named("animals::Animal".to_string()));
    }

    #[test]
    fn interface_14_method_body_rejected() {
        // A body WITH `self` is a valid default method (willow-1js.3).
        let p = parse_ok("interface Greet { fn hi(self) { return; } }");
        let i = first_interface(&p);
        assert_eq!(i.methods[0].name, "hi");
        assert!(
            i.methods[0].default_body.is_some(),
            "method with a body should be a default"
        );

        // A required method (no body) has no default.
        let p2 = parse_ok("interface Sig { fn need(self) -> i64; }");
        assert!(first_interface(&p2).methods[0].default_body.is_none());

        // A body WITHOUT `self` is rejected (E0420): there is no receiver.
        let errs = parse_errors("interface Bad { fn f() { return; } }");
        assert!(
            errs.iter().any(|e| e.code == ErrorCode::E0420),
            "expected E0420, got {errs:#?}"
        );
    }

    #[test]
    fn interface_15_field_rejected() {
        let errs = parse_errors("interface Bad { value: i64; }");
        assert!(
            errs.iter().any(|e| e.code == ErrorCode::E0421),
            "expected E0421, got {errs:#?}"
        );
    }

    #[test]
    fn interface_16_implements_before_extends_rejected() {
        // `implements` must come after `extends`; the reverse order is a parse error.
        let errs = parse_errors("class Dog implements Animal extends Mammal {}");
        assert!(
            !errs.is_empty(),
            "expected a parse error for wrong clause order"
        );
    }

    #[test]
    fn interface_17_open_interface_rejected() {
        let errs = parse_errors("open interface Bad { fn f(self); }");
        assert!(
            errs.iter().any(|e| e.code == ErrorCode::E0105),
            "expected E0105, got {errs:#?}"
        );
    }

    #[test]
    fn interface_18_method_missing_semicolon_rejected() {
        let errs = parse_errors("interface Bad { fn f(self) -> i64 }");
        assert!(!errs.is_empty(), "expected error for missing `;`");
    }

    #[test]
    fn interface_19_trailing_comma_in_implements_rejected() {
        let errs = parse_errors("class Dog implements Animal, {}");
        assert!(!errs.is_empty(), "expected error for trailing comma");
    }

    #[test]
    fn interface_20_class_with_body_and_implements() {
        let p = parse_ok(
            "class Dog implements Animal { pub name: String; pub fn speak(self) -> String { return \"woof\"; } }",
        );
        let c = first_class(&p);
        assert_eq!(c.implements.len(), 1);
        assert_eq!(c.fields.len(), 1);
        assert_eq!(c.methods.len(), 1);
    }

    #[test]
    fn interface_21_param_types_preserved() {
        let p = parse_ok("interface Greeter { fn greet(self, who: String) -> String; }");
        let m = &first_interface(&p).methods[0];
        assert_eq!(m.params[0].name, "who");
        assert_eq!(m.params[0].ty, Type::String);
    }

    #[test]
    fn interface_23_future_example_file_parses() {
        // The on-disk example must parse cleanly (codegen lands in willow-xds).
        let src = include_str!("../../example/future/trait_like_interfaces.wi");
        let p = parse_ok(src);
        assert!(p.items.iter().any(|i| matches!(i, Item::Interface(_))));
        let dog = first_class(&p);
        assert_eq!(dog.implements.len(), 2);
    }

    #[test]
    fn interface_22_interface_and_class_coexist() {
        let p = parse_ok(
            "interface Animal { fn speak(self) -> String; } class Dog implements Animal { pub fn speak(self) -> String { return \"woof\"; } } fn main() {}",
        );
        assert!(matches!(p.items[0], Item::Interface(_)));
        assert!(matches!(p.items[1], Item::Class(_)));
        assert!(matches!(p.items[2], Item::Function(_)));
        assert_eq!(
            first_class(&p).implements[0],
            Type::Named("Animal".to_string())
        );
    }
}

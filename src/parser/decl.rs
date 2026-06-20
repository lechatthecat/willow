use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::TokenKind;

impl Parser {
    pub(super) fn parse_module_decl(&mut self) -> Result<ModuleDecl, Diagnostic> {
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

    pub(super) fn parse_module_path(&mut self) -> Result<String, Diagnostic> {
        // Module paths use `::` only (like imports); `.` is reserved for member
        // access.
        let mut parts = vec![self.expect_path_segment()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }
        Ok(parts.join("::"))
    }

    pub(super) fn parse_import(&mut self) -> Result<ImportDecl, Diagnostic> {
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

    pub(super) fn parse_import_path(&mut self) -> Result<String, Diagnostic> {
        let mut parts = vec![self.expect_path_segment()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_path_segment()?);
        }
        Ok(parts.join("::"))
    }

    /// Like [`expect_ident`], but also accepts builtin names that are lexed as
    /// keywords (`print`, `println`) so they can appear as import-path segments,
    /// e.g. `import std::io::println;`.
    pub(super) fn expect_path_segment(&mut self) -> Result<String, Diagnostic> {
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

    pub(super) fn parse_item(&mut self) -> Result<Item, Diagnostic> {
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

    pub(super) fn parse_enum_decl(&mut self, public: bool) -> Result<EnumDecl, Diagnostic> {
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

    pub(super) fn parse_class(
        &mut self,
        public: bool,
        is_open: bool,
    ) -> Result<ClassDecl, Diagnostic> {
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
        let mut constructors = Vec::new();

        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let member_public = self.eat(TokenKind::Pub);
            let member_prot = if !member_public {
                self.eat(TokenKind::Prot)
            } else {
                false
            };
            // `init(...)` constructor — `init` is a contextual keyword: an
            // identifier `init` immediately followed by `(` at member position
            // (willow-scq2). It carries visibility but no other modifiers.
            let is_init = self.is_constructor_init_ahead();
            if is_init {
                constructors.push(self.parse_constructor(member_public, member_prot)?);
                continue;
            }
            // `static` marks a class-level member (willow-qsqf). It sits after
            // visibility and before `open`/`override`/`async` for methods, and
            // before `mut` for a mutable static property.
            let member_static_span = if self.check(TokenKind::Static) {
                let span = self.current_span();
                self.advance();
                Some(span)
            } else {
                None
            };
            if let Some(static_span) = member_static_span {
                if self.is_constructor_init_ahead() {
                    let init_span = self.current_span();
                    return Err(Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0850,
                        "`static` is not allowed on constructor `init`",
                    )
                    .with_label(Label::primary(static_span, "`static` constructor modifier"))
                    .with_label(Label::secondary(
                        init_span,
                        "`init` is always an instance constructor",
                    ))
                    .with_help(
                        "write `init(self, ...)` without `static`, or use a differently named `static fn` factory",
                    ));
                }
            }
            let member_static = member_static_span.is_some();
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
            constructors,
            span,
        })
    }

    pub(super) fn parse_interface(&mut self, public: bool) -> Result<InterfaceDecl, Diagnostic> {
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
            // Static interface members are out of scope (willow-qsqf §17 → E0836).
            if self.check(TokenKind::Static) {
                let span = self.current_span();
                return Err(Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0836,
                    "static interface members are not supported yet",
                )
                .with_label(Label::primary(span, "`static` member in an interface"))
                .with_help("declare static members on a class, not an interface"));
            }
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

    pub(super) fn parse_interface_method(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name_span = self.current_span();
        let name = self.expect_ident()?;
        if name == "init" {
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0850,
                "method name `init` is reserved for constructors",
            )
            .with_label(Label::primary(name_span, "`fn init` is not a method"))
            .with_help("choose a different method name; constructors are class-only `init(self, ...)` declarations"));
        }
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

    pub(super) fn parse_type_path(&mut self) -> Result<TypePath, Diagnostic> {
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

    pub(super) fn parse_field(
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
        // must not have one (they are initialized through `init`/`new`).
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
                "instance fields cannot have an initializer; assign them in `init`",
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

    pub(super) fn parse_method(
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
        let name_span = self.current_span();
        let name = self.expect_ident()?;
        if name == "init" {
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0850,
                "method name `init` is reserved for constructors",
            )
            .with_label(Label::primary(name_span, "`fn init` is not a method"))
            .with_help(
                "write `init(self, ...)` for a constructor, or choose a different method name",
            ));
        }
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
            is_default_injected: false,
        })
    }

    /// Parse an `init(self, params...) { body }` constructor (willow-scq2). No
    /// return type is allowed; the explicit `self` receiver is not stored in
    /// `params`, matching method lowering and `new Class(args...)` arity.
    pub(super) fn parse_constructor(
        &mut self,
        public: bool,
        protected: bool,
    ) -> Result<ConstructorDecl, Diagnostic> {
        let start = self.current_span();
        self.expect_ident()?; // `init`
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();

        if !self.check(TokenKind::SelfKw) {
            let span = self.current_span();
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0849,
                "constructor `init` must declare `self` as its first parameter",
            )
            .with_label(Label::primary(span, "expected `self` here"))
            .with_help("write `init(self, ...)` or `init(self)`"));
        }
        let self_span = self.current_span();
        self.advance();
        if !self.check(TokenKind::RParen) && !self.eat(TokenKind::Comma) {
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0849,
                "constructor `self` parameter must be bare",
            )
            .with_label(Label::primary(self_span, "constructor receiver"))
            .with_help("write `self` without a type, followed by `,` or `)`"));
        }
        while !self.check(TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_param()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        // Constructors must not declare a return type (willow-scq2 §4.2).
        if self.eat(TokenKind::Arrow) {
            return Err(self.err(
                ErrorCode::E0840,
                "constructor `init` must not declare a return type",
            ));
        }
        let body = self.parse_block()?;
        let span = Span::new(start.start, body.span.end, start.line, start.col);
        Ok(ConstructorDecl {
            public,
            protected,
            params,
            body,
            span,
        })
    }

    pub(super) fn is_constructor_init_ahead(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(n) if n == "init")
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::LParen)
            )
    }

    pub(super) fn parse_fn(
        &mut self,
        public: bool,
        is_async: bool,
    ) -> Result<FunctionDecl, Diagnostic> {
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

    pub(super) fn parse_param(&mut self) -> Result<Param, Diagnostic> {
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
}

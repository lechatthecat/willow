use super::Parser;
use super::ast::*;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::lexer::token::TokenKind;

#[willow_continuations::parser]
impl Parser {
    pub(super) fn parse_module_decl(&mut self) -> Result<ModuleDecl, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Module)?;
        let path = self.parse_module_path()?;
        let span = span.to(self.current_span());
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

    /// Parse one import statement. Grouped imports are expanded immediately
    /// into ordinary declarations so every later phase reuses the exact same
    /// resolution, visibility, duplicate, and conflict rules.
    pub(super) fn parse_import(&mut self) -> Result<Vec<ImportDecl>, Diagnostic> {
        let span = self.current_span();
        self.expect(TokenKind::Import)?;
        let mut parts = vec![self.expect_path_segment()?];
        while self.eat(TokenKind::ColonColon) {
            if self.check(TokenKind::Star) {
                return Err(self.glob_import_error());
            }
            if self.eat(TokenKind::LBrace) {
                return self.parse_grouped_import(parts);
            }
            parts.push(self.expect_path_segment()?);
        }

        let path = parts.join("::");
        let alias = if self.eat(TokenKind::As) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        let span = span.to(self.current_span());
        self.expect(TokenKind::Semicolon)?;
        Ok(vec![ImportDecl { path, alias, span }])
    }

    fn parse_grouped_import(&mut self, prefix: Vec<String>) -> Result<Vec<ImportDecl>, Diagnostic> {
        if self.check(TokenKind::RBrace) {
            return Err(self.err(
                ErrorCode::E0102,
                "grouped import must contain at least one item",
            ));
        }

        let prefix = prefix.join("::");
        let mut imports = Vec::new();
        loop {
            if self.check(TokenKind::Star) {
                return Err(self.glob_import_error());
            }

            let span = self.current_span();
            let item = self.expect_path_segment()?;
            let alias = if self.eat(TokenKind::As) {
                Some(self.expect_ident()?)
            } else {
                None
            };
            let span = span.to(self.tokens[self.pos - 1].span);
            imports.push(ImportDecl {
                path: format!("{prefix}::{item}"),
                alias,
                span,
            });

            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.check(TokenKind::RBrace) {
                break;
            }
        }

        self.expect(TokenKind::RBrace)?;
        self.expect(TokenKind::Semicolon)?;
        Ok(imports)
    }

    fn glob_import_error(&self) -> Diagnostic {
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0102,
            "glob imports are not supported",
        )
        .with_label(Label::primary(
            self.current_span(),
            "replace `*` with explicit item names",
        ))
        .with_help("list imported items explicitly, for example `::{Array, Map}`")
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

    /// `Ok(None)`: the item was consumed but rejected, with its diagnostic
    /// already recorded in `recovered_errors` (see [`Self::parse_const`]).
    pub(super) fn parse_item(&mut self) -> Result<Option<Item>, Diagnostic> {
        let public = self.eat(TokenKind::Pub);
        let is_open = self.eat(TokenKind::Open);
        let is_async = self.eat(TokenKind::Async);
        match self.peek_kind() {
            TokenKind::Fn => Ok(Some(Item::Function(self.parse_fn(public, is_async)?))),
            TokenKind::Class => Ok(Some(Item::Class(self.parse_class(public, is_open)?))),
            TokenKind::Enum => Ok(Some(Item::Enum(self.parse_enum_decl(public)?))),
            TokenKind::Const if is_open => Err(self.err(
                ErrorCode::E0105,
                "`open` cannot be used on a `const` declaration",
            )),
            TokenKind::Const if is_async => Err(self.err(
                ErrorCode::E0105,
                "`async` cannot be used on a `const` declaration",
            )),
            TokenKind::Const => Ok(self.parse_const(public)?.map(Item::Function)),
            TokenKind::Interface if is_open => Err(self.err(
                ErrorCode::E0105,
                "`open` cannot be used on an interface declaration",
            )),
            TokenKind::Interface if is_async => Err(self.err(
                ErrorCode::E0105,
                "`async` cannot be used on an interface declaration",
            )),
            TokenKind::Interface => Ok(Some(Item::Interface(self.parse_interface(public)?))),
            _ if is_async => Err(self.err(ErrorCode::E0105, "`async` can only be used on `fn`")),
            _ => Err(self.err(
                ErrorCode::E0105,
                "expected `fn`, `class`, `enum`, `interface`, or `const`",
            )),
        }
    }

    fn parse_type_parameters(&mut self) -> Result<Vec<String>, Diagnostic> {
        let mut params = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if !self.eat(TokenKind::Lt) {
            return Ok(params);
        }
        loop {
            let span = self.current_span();
            let name = self.expect_ident()?;
            #[cfg(test)]
            GENERIC_PARAMETER_LOOKUPS.with(|count| count.set(count.get() + 1));
            if !seen.insert(name.clone()) {
                return Err(Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0102,
                    format!("duplicate type parameter `{name}`"),
                )
                .with_label(Label::primary(
                    span,
                    "already declared in this parameter list",
                )));
            }
            params.push(name);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Gt)?;
        Ok(params)
    }

    pub(super) fn parse_enum_decl(&mut self, public: bool) -> Result<EnumDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_parameters()?;
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
            let v_span = v_start.to(v_end);
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
        let span = start.to(end);
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

        let type_params = match self.parse_type_parameters() {
            Ok(params) => params,
            Err(diagnostic) => {
                // Recover the entire malformed class, without interpreting its
                // fields/constructors as top-level declarations. Item recovery
                // consumes the closing brace and preserves following modifiers.
                let mut depth = 0usize;
                while !self.at_eof() {
                    match self.peek_kind() {
                        TokenKind::LBrace => depth += 1,
                        TokenKind::RBrace if depth <= 1 => break,
                        TokenKind::RBrace => depth -= 1,
                        _ => {}
                    }
                    self.advance();
                }
                return Err(diagnostic);
            }
        };

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
            if let Some(static_span) = member_static_span
                && self.is_constructor_init_ahead()
            {
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
            let member_static = member_static_span.is_some();
            // `open:` is an attempted field name, not a method modifier.
            // Diagnose while the cursor still points at the reserved name.
            if self.check(TokenKind::Open) && matches!(self.peek_kind_at(1), TokenKind::Colon) {
                return Err(self.err(ErrorCode::E0102, "expected identifier"));
            }
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
        let span = start.to(end);

        Ok(ClassDecl {
            type_params,
            name,
            public,
            is_open,
            base_class,
            source_implements_len: implements.len(),
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
            loop {
                let start = self.current_span();
                let name = self.parse_module_path()?;
                self.type_uses.push(TypeUse {
                    name: name.clone(),
                    span: start.to(self.tokens[self.pos - 1].span),
                });
                extends.push(name);
                if self.check(TokenKind::Lt) {
                    let span = self.current_span();
                    // Inheritance currently stores names, not instantiated types.
                    // Skip the rejected declaration, including nested default bodies,
                    // so item recovery cannot mistake its methods for free functions.
                    // Leave the closing brace for recover_to_next_item to consume.
                    let mut depth = 0usize;
                    while !self.at_eof() {
                        match self.peek_kind() {
                            TokenKind::LBrace => depth += 1,
                            TokenKind::RBrace if depth <= 1 => break,
                            TokenKind::RBrace => depth -= 1,
                            _ => {}
                        }
                        self.advance();
                    }
                    return Err(Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0427,
                        "generic interface instantiation in `extends` is not supported yet",
                    )
                    .with_label(Label::primary(span, "type arguments in an interface `extends` clause"))
                    .with_help("use a non-generic super-interface and declare the specialized methods explicitly"));
                }
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::LBrace)?;

        self.default_import_usage.begin(!type_params.is_empty());
        self.default_import_usage.type_parameters(&type_params);
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
        let span = start.to(end);

        Ok(InterfaceDecl {
            default_import_uses: self.default_import_usage.finish(),
            name,
            public,
            type_params,
            extends,
            methods,
            instance_of: None,
            span,
        })
    }

    pub(super) fn parse_interface_method(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
        let scope = self.default_import_usage.scope();
        let result = self.parse_interface_method_inner();
        self.default_import_usage.restore(scope);
        result
    }

    fn parse_interface_method_inner(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Fn)?;
        let name_span = self.current_span();
        let name = self.expect_method_name()?;
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
            #[cfg(test)]
            super::import_usage::DEFAULT_BODY_PARSES.with(|count| count.set(count.get() + 1));
            for param in &params {
                self.default_import_usage.bind(&param.name);
            }
            let body = self.parse_block()?;
            let end = self.previous_span();
            (Some(body), end)
        } else {
            let end = self.current_span();
            self.expect(TokenKind::Semicolon)?;
            (None, end)
        };
        let span = start.to(end);

        Ok(InterfaceMethodDecl {
            name,
            params,
            is_static: !has_self,
            return_type,
            default_body,
            span,
        })
    }

    pub(super) fn parse_type_path(&mut self) -> Result<TypePath, Diagnostic> {
        let start = self.current_span();
        let mut parts = vec![self.expect_ident()?];
        while self.eat(TokenKind::ColonColon) {
            parts.push(self.expect_ident()?);
        }
        self.type_uses.push(TypeUse {
            name: parts.join("::"),
            span: start.to(self.tokens[self.pos - 1].span),
        });
        if parts.len() == 1 {
            Ok(TypePath::Local(parts.remove(0)))
        } else {
            Ok(TypePath::Qualified(parts))
        }
    }

    pub(super) fn parse_field(
        &mut self,
        public: bool,
        protected: bool,
        is_static: bool,
    ) -> Result<FieldDecl, Diagnostic> {
        // `static mut name: T = expr` — `mut` is only meaningful on a static
        // property (instance fields take their mutability from the binding).
        let is_mut = self.eat(TokenKind::Mut);
        if is_mut && !is_static {
            return Err(self.err(
                ErrorCode::E0105,
                "`mut` on a class field is only allowed on a `static` property",
            ));
        }
        let span = self.current_span();
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
                format!("static property `{}` requires an initializer", name),
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
        let name = self.expect_method_name()?;
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
        let span = start.to(body.span);

        Ok(MethodDecl {
            name,
            public,
            protected,
            is_async,
            is_open,
            is_override,
            is_static,
            params,
            return_type,
            body,
            span,
            is_default_injected: false,
            is_interface_default: false,
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
        let span = start.to(body.span);
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
        let type_params = self.parse_type_parameters()?;
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
        let span = start.to(body.span);
        Ok(FunctionDecl {
            type_params,
            name,
            public,
            is_async,
            params,
            return_type,
            body,
            span,
            rust_bridge: None,
            constant: None,
        })
    }

    /// `[pub] const NAME: T = literal;` (willow-jz15.10). The constant is
    /// represented as a zero-parameter function returning the literal and
    /// carrying the value in [`FunctionDecl::constant`]; see there.
    ///
    /// A syntactically complete declaration whose initializer or type is not a
    /// supported constant yields `Ok(None)` with its diagnostic recorded in
    /// `recovered_errors`: the whole declaration, `;` included, is consumed, so
    /// item recovery must not run and skip the following item (willow-jz15.93).
    pub(super) fn parse_const(&mut self, public: bool) -> Result<Option<FunctionDecl>, Diagnostic> {
        let start = self.current_span();
        self.expect(TokenKind::Const)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let type_span = self.current_span();
        let return_type = self.parse_type()?;
        let type_span = type_span.to(self.previous_span());
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        let end = self.current_span();
        self.expect(TokenKind::Semicolon)?;
        let span = start.to(end);
        let constant = match const_value(&name, &return_type, type_span, &value) {
            Ok(constant) => constant,
            Err(error) => {
                self.recovered_errors.push(error);
                return Ok(None);
            }
        };
        let return_span = value.span();
        let body = Block {
            id: BodyId::fresh(),
            stmts: vec![Stmt::Return(ReturnStmt {
                value: Some(value),
                span: return_span,
            })],
            span,
        };
        Ok(Some(FunctionDecl {
            type_params: Vec::new(),
            name,
            public,
            is_async: false,
            params: Vec::new(),
            return_type,
            body,
            span,
            rust_bridge: None,
            constant: Some(constant),
        }))
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
        let type_span = type_start.to(type_end);
        Ok(Param {
            name,
            ty,
            mode,
            span,
            type_span,
        })
    }
}

/// Validate a `const` declaration and return its value, or the E0109/E0110
/// diagnostic rejecting it.
fn const_value(
    name: &str,
    declared: &Type,
    type_span: crate::parser::Span,
    value: &Expr,
) -> Result<ConstValue, Diagnostic> {
    let type_name = crate::semantic::type_checker::types::type_name::<String>;
    let Some(constant) = const_literal(value) else {
        let error = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0109,
            format!("`const {name}` requires an integer, float, boolean, or string literal"),
        )
        .with_label(Label::primary(value.span(), "unsupported constant initializer"))
        .with_help("use an integer, float, `true`/`false`, or string literal; numeric literals may be negative");
        return Err(if is_array_const(declared, value) {
            error.with_help(frozen_array_static_help(declared))
        } else {
            error.with_help("for computed values, use a function that returns the value instead")
        });
    };
    if !matches!(declared, Type::I64 | Type::F64 | Type::Bool | Type::String) {
        let error = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0110,
            format!("`const {name}` cannot have type `{}`", type_name(declared)),
        )
        .with_label(Label::primary(type_span, "unsupported constant type"))
        .with_help("a constant's type must be `i64`, `f64`, `bool`, or `String`");
        return Err(if is_array_const(declared, value) {
            error.with_help(frozen_array_static_help(declared))
        } else {
            error
        });
    }
    if constant.ty() != *declared {
        let declared = type_name(declared);
        return Err(Diagnostic::new(
            Severity::Error,
            ErrorCode::E0110,
            format!(
                "mismatched types: `const {name}` is declared `{declared}`, but its value is `{}`",
                type_name(&constant.ty())
            ),
        )
        .with_label(Label::primary(
            value.span(),
            format!("expected `{declared}`"),
        ))
        .with_label(Label::secondary(type_span, "declared type")));
    }
    Ok(constant)
}

/// A rejected `const` meant to hold an array: an array type or an array literal.
fn is_array_const(declared: &Type, value: &Expr) -> bool {
    matches!(value, Expr::ArrayLiteral(..))
        || match declared {
            Type::Array(_) => true,
            Type::Named(name) => name == "Array" || name == "FrozenArray",
            Type::Generic(name, _) => name == "FrozenArray",
            _ => false,
        }
}

/// The replacement for a constant array: a frozen array built once in a class
/// static property. A function returning an array literal would rebuild the
/// array on every call (willow-jz15.93).
fn frozen_array_static_help(declared: &Type) -> String {
    let element = match declared {
        Type::Array(element) => crate::semantic::type_checker::types::type_name::<String>(element),
        Type::Generic(_, args) if args.len() == 1 => {
            crate::semantic::type_checker::types::type_name::<String>(&args[0])
        }
        _ => "T".to_string(),
    };
    format!(
        "build a constant array once as a frozen class static property, e.g. \
         `class Tables {{ pub static values: FrozenArray<{element}> = [...].freeze(); }}`, \
         and read it as `Tables::values`; a function returning an array literal \
         allocates a new array on every call"
    )
}

/// The value of a `const` initializer: a literal, or a negated numeric literal.
fn const_literal(expr: &Expr) -> Option<ConstValue> {
    match expr {
        Expr::Integer(value, ..) => Some(ConstValue::Integer(*value)),
        Expr::Float(value, ..) => Some(ConstValue::Float(*value)),
        Expr::Bool(value, ..) => Some(ConstValue::Bool(*value)),
        Expr::String(value, ..) => Some(ConstValue::String(value.clone())),
        Expr::Unary(unary) if matches!(unary.op, UnaryOp::Neg) => match &unary.expr {
            Expr::Integer(value, ..) => value.checked_neg().map(ConstValue::Integer),
            Expr::Float(value, ..) => Some(ConstValue::Float(-*value)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
thread_local! {
    static GENERIC_PARAMETER_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod generic_parameter_tests {
    use super::*;
    #[test]
    fn duplicate_detection_has_one_lookup_per_parameter() {
        for count in [1, 16, 256] {
            let parameters = (0..count)
                .map(|i| format!("T{i}"))
                .collect::<Vec<_>>()
                .join(",");
            for duplicate in [false, true] {
                let source = format!(
                    "fn identity<{parameters}{}>(x: i64) -> i64 {{ return x; }}",
                    if duplicate { ",T0" } else { "" }
                );
                GENERIC_PARAMETER_LOOKUPS.with(|visits| visits.set(0));
                let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                let (_, errors) = Parser::new(tokens).parse();
                assert_eq!(errors.is_empty(), !duplicate);
                if duplicate {
                    assert!(
                        errors
                            .iter()
                            .any(|error| error.message.contains("duplicate type parameter"))
                    );
                }
                let lookups = GENERIC_PARAMETER_LOOKUPS.with(|visits| visits.get());
                assert_eq!(lookups, count + usize::from(duplicate));
                println!(
                    "WILLOW_GENERIC_AUDIT {}",
                    serde_json::json!({"case":"parameter_list", "parameters":count, "duplicate":duplicate, "lookups":lookups})
                );
            }
        }
    }
}

#[cfg(test)]
mod const_diagnostic_tests {
    use super::*;

    #[test]
    fn const_initializer_diagnostic_perspectives() {
        // 12 unsupported forms and 10 supported forms: no semantic expansion.
        for (ty, value, accepted) in [
            ("Array", "[]", false),
            ("Array", "[1]", false),
            ("Array", "[\"alpha\", \"beta\"]", false),
            ("Array<i64>", "[1, 2]", false),
            ("FrozenArray<i64>", "[1, 2]", false),
            ("Array", "[[1], [2]]", false),
            ("i64", "1 + 2", false),
            ("i64", "value()", false),
            ("i64", "OTHER", false),
            ("bool", "!true", false),
            ("String", "\"a\" + \"b\"", false),
            ("i64", "-(1 + 2)", false),
            ("i64", "0", true),
            ("i64", "42", true),
            ("i64", "-42", true),
            ("f64", "0.5", true),
            ("f64", "-0.5", true),
            ("bool", "true", true),
            ("bool", "false", true),
            ("String", "\"\"", true),
            ("String", "\"alpha\"", true),
            ("i64", "(1)", true),
        ] {
            for visibility in ["", "pub "] {
                let source = format!("{visibility}const VALUE: {ty} = {value};");
                let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                let (_, errors) = Parser::new(tokens).parse();
                if accepted {
                    assert!(errors.is_empty(), "{source}: {errors:?}");
                } else {
                    assert_eq!(errors.len(), 1, "{source}: {errors:?}");
                    let error = &errors[0];
                    assert_eq!(error.code, ErrorCode::E0109, "{source}");
                    assert_eq!(
                        error.message,
                        "`const VALUE` requires an integer, float, boolean, or string literal"
                    );
                    assert_eq!(error.labels[0].message, "unsupported constant initializer");
                    assert!(
                        error
                            .helps
                            .iter()
                            .any(|help| help.contains("numeric literals may be negative"))
                    );
                    let array = ty.contains("Array") || value.starts_with('[');
                    let expected = if array {
                        "frozen class static property"
                    } else {
                        "use a function that returns the value instead"
                    };
                    assert!(
                        error.helps.iter().any(|help| help.contains(expected)),
                        "{source}: {:?}",
                        error.helps
                    );
                    if array {
                        assert!(
                            !error
                                .helps
                                .iter()
                                .any(|help| help.contains("use a function")),
                            "{source}: array help must not recommend a function"
                        );
                    }
                }
            }
        }
    }

    fn parse_errors(source: &str) -> (Program, Vec<Diagnostic>) {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse()
    }

    fn item_names(program: &Program) -> Vec<&str> {
        program
            .items
            .iter()
            .map(|item| match item {
                Item::Function(f) => f.name.as_str(),
                Item::Class(c) => c.name.as_str(),
                Item::Enum(e) => e.name.as_str(),
                Item::Interface(i) => i.name.as_str(),
            })
            .collect()
    }

    // willow-jz15.93: a rejected `const` is fully consumed, so recovery must not
    // skip the next item. Every rejected const is reported once, in source
    // order, and every following valid item is kept.
    #[test]
    fn rejected_consts_do_not_swallow_the_next_item() {
        // (source, expected codes in order, kept item names)
        let cases: &[(&str, &[ErrorCode], &[&str])] = &[
            // The ticket's reproducer: four consecutive const arrays.
            (
                "const KNIGHT: Array<i64> = [6, 10]; const KING: Array<i64> = [1, 7]; \
                 const DIAG: Array<i64> = [7, 9]; const ORTHO: Array<i64> = [1, 8]; fn main() {}",
                &[ErrorCode::E0109; 4],
                &["main"],
            ),
            (
                "const A: Array<i64> = [1]; const B: Array<i64> = [2];",
                &[ErrorCode::E0109; 2],
                &[],
            ),
            (
                "const A: Array<i64> = [1]; fn main() {}",
                &[ErrorCode::E0109],
                &["main"],
            ),
            (
                "const A: Array<i64> = [1]; pub fn f() {}",
                &[ErrorCode::E0109],
                &["f"],
            ),
            (
                "const A: Array<i64> = [1]; async fn f() {}",
                &[ErrorCode::E0109],
                &["f"],
            ),
            (
                "const A: Array<i64> = [1]; class C {}",
                &[ErrorCode::E0109],
                &["C"],
            ),
            (
                "const A: Array<i64> = [1]; open class C {}",
                &[ErrorCode::E0109],
                &["C"],
            ),
            (
                "const A: Array<i64> = [1]; enum E { X }",
                &[ErrorCode::E0109],
                &["E"],
            ),
            (
                "const A: Array<i64> = [1]; interface I {}",
                &[ErrorCode::E0109],
                &["I"],
            ),
            (
                "const A: Array<i64> = [1]; const B: i64 = 2;",
                &[ErrorCode::E0109],
                &["B"],
            ),
            (
                "const A: Array<i64> = [1]; pub const B: i64 = 2;",
                &[ErrorCode::E0109],
                &["B"],
            ),
            (
                "pub const A: Array<i64> = [1]; const B: i64 = 2;",
                &[ErrorCode::E0109],
                &["B"],
            ),
            (
                "const A: i64 = 1 + 2; const B: i64 = f(); fn main() {}",
                &[ErrorCode::E0109; 2],
                &["main"],
            ),
            (
                "const A: f64 = 1; const B: i64 = 2;",
                &[ErrorCode::E0110],
                &["B"],
            ),
            (
                "const A: i32 = 1; const B: i64 = 2;",
                &[ErrorCode::E0110],
                &["B"],
            ),
            (
                "const A: Array<i64> = 1; fn main() {}",
                &[ErrorCode::E0110],
                &["main"],
            ),
            (
                "const A: f64 = 1; const B: Array<i64> = [1]; const C: i32 = 1; const D: bool = 1;",
                &[
                    ErrorCode::E0110,
                    ErrorCode::E0109,
                    ErrorCode::E0110,
                    ErrorCode::E0110,
                ],
                &[],
            ),
            (
                "const A: i64 = 1; const B: Array<i64> = [1]; const C: i64 = 3; const D: FrozenArray<i64> = [2]; const E: i64 = 5;",
                &[ErrorCode::E0109; 2],
                &["A", "C", "E"],
            ),
            (
                "fn main() {} const A: Array<i64> = [1];",
                &[ErrorCode::E0109],
                &["main"],
            ),
            (
                "const A: Array<i64> = [[1], [2]]; fn main() {}",
                &[ErrorCode::E0109],
                &["main"],
            ),
            // A syntax error inside a const still recovers to the next item.
            (
                "const A: i64 = ; const B: i64 = 2;",
                &[ErrorCode::E0102],
                &["B"],
            ),
            // A missing `;` fails on the next item's first token, which is kept.
            (
                "const A: i64 = 1 const B: i64 = 2;",
                &[ErrorCode::E0101],
                &["B"],
            ),
            (
                "const A: i64 = 1 fn main() {}",
                &[ErrorCode::E0101],
                &["main"],
            ),
            (
                "const A: i64 = 1 pub fn f() {} const B: Array<i64> = [1] class C {}",
                &[ErrorCode::E0101, ErrorCode::E0101],
                &["f", "C"],
            ),
        ];
        for (source, codes, kept) in cases {
            let (program, errors) = parse_errors(source);
            let actual: Vec<_> = errors.iter().map(|error| error.code).collect();
            assert_eq!(&actual, codes, "{source}: {errors:#?}");
            assert_eq!(&item_names(&program), kept, "{source}");
        }
    }

    // Recovery work stays linear: n rejected consts yield n diagnostics and keep
    // every interleaved valid item.
    #[test]
    fn rejected_const_recovery_scales_linearly() {
        for count in [1, 16, 256] {
            let source = (0..count)
                .map(|i| format!("const A{i}: Array<i64> = [{i}]; const B{i}: i64 = {i};"))
                .collect::<String>();
            let (program, errors) = parse_errors(&source);
            assert_eq!(errors.len(), count);
            assert!(errors.iter().all(|error| error.code == ErrorCode::E0109));
            assert_eq!(program.items.len(), count);
        }
    }
}

use super::ast::*;
use super::{Parser, modifiers::MemberHead};
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::TokenKind;

const EFFECT_CONTRACT_NAMES: [&str; 7] = [
    "no_wait",
    "no_block",
    "no_suspend",
    "no_io",
    "no_panic",
    "no_alloc",
    "pure",
];

// Candidates are fixed ASCII names, so the DP row is bounded independently of
// the invalid identifier's length. Ties follow the documented candidate order.
fn closest_effect_contract(name: &str) -> &'static str {
    EFFECT_CONTRACT_NAMES
        .into_iter()
        .min_by_key(|candidate| {
            let mut row: Vec<usize> = (0..=candidate.len()).collect();
            for (i, ch) in name.chars().enumerate() {
                let mut diagonal = row[0];
                row[0] = i + 1;
                for (j, byte) in candidate.bytes().enumerate() {
                    #[cfg(test)]
                    effect_contract_diagnostic_tests::EDIT_CELLS.with(|n| n.set(n.get() + 1));
                    let above = row[j + 1];
                    row[j + 1] = (row[j] + 1)
                        .min(above + 1)
                        .min(diagonal + usize::from(ch != char::from(byte)));
                    diagonal = above;
                }
            }
            row[candidate.len()]
        })
        .unwrap()
}

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

    pub(super) fn expect_path_segment(&mut self) -> Result<String, Diagnostic> {
        self.expect_ident()
    }

    /// Resolve the closed set of built-in contract names at declaration time.
    pub(super) fn parse_effect_contracts(&mut self) -> Result<Vec<EffectContract>, Diagnostic> {
        let mut contracts = Vec::<EffectContract>::new();
        // Contracts form a set. The closed vocabulary bounds this scan to nine
        // entries, including under arbitrarily repeated attributes.
        let mut push = |contract: EffectContract| {
            if !contracts.iter().any(|c| {
                #[cfg(test)]
                effect_contract_diagnostic_tests::CONTRACT_COMPARISONS.with(|n| n.set(n.get() + 1));
                c.kind == contract.kind
            }) {
                contracts.push(contract);
            }
        };
        while self.eat(TokenKind::At) {
            let start = self.previous_span();
            let attribute = self.expect_ident()?;
            let simple = match attribute.as_str() {
                "pure" => Some(EffectContractKind::Pure),
                "verified" => Some(EffectContractKind::Verified),
                "sending" => Some(EffectContractKind::Sending),
                _ => None,
            };
            if let Some(kind) = simple {
                push(EffectContract {
                    kind,
                    span: start.to(self.previous_span()),
                });
                continue;
            }
            if attribute != "effects" {
                return Err(self.err(
                    ErrorCode::E0102,
                    "unknown declaration attribute; expected `effects`, `pure`, `sending`, or `verified`",
                ));
            }
            self.expect(TokenKind::LParen)?;
            loop {
                let name = self.expect_ident()?;
                let kind = match name.as_str() {
                    "pure" => EffectContractKind::Pure,
                    "no_wait" => EffectContractKind::NoWait,
                    "no_block" => EffectContractKind::NoBlock,
                    "no_suspend" => EffectContractKind::NoSuspend,
                    "no_io" => EffectContractKind::NoIo,
                    "no_panic" => EffectContractKind::NoPanic,
                    "no_alloc" => EffectContractKind::NoAlloc,
                    _ => {
                        return Err(self
                            .err_at(
                                ErrorCode::E0102,
                                format!("unknown effect contract `{name}`"),
                                self.previous_span(),
                            )
                            .with_help(format!(
                                "valid effect contracts: {}; did you mean `{}`?",
                                EFFECT_CONTRACT_NAMES.join(", "),
                                closest_effect_contract(&name),
                            )));
                    }
                };
                push(EffectContract {
                    kind,
                    span: start.to(self.previous_span()),
                });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen)?;
        }
        // Normalize the implication once, before registration, checking, query
        // capture and proof generation. An explicit purity span takes precedence.
        if !contracts.iter().any(|c| c.kind == EffectContractKind::Pure)
            && let Some(index) = contracts
                .iter()
                .position(|c| c.kind == EffectContractKind::Verified)
        {
            // Match the established `@pure @verified` order so adding the
            // implication does not change protected contract digests.
            contracts.insert(
                index,
                EffectContract {
                    kind: EffectContractKind::Pure,
                    span: contracts[index].span,
                },
            );
        }
        Ok(contracts)
    }

    /// `@sending` is a checked ownership contract on concrete top-level
    /// functions. Methods can be overridden or dispatched through interfaces,
    /// and generic bodies are checked before their element types are known,
    /// so neither can carry it soundly yet.
    pub(super) fn reject_sending(
        &self,
        contracts: &[EffectContract],
        params: &[Param],
        place: &str,
    ) -> Result<(), Diagnostic> {
        let span = contracts
            .iter()
            .find(|c| c.kind == EffectContractKind::Sending)
            .map(|c| c.span)
            .or_else(|| params.iter().find(|p| p.sending).map(|p| p.span));
        match span {
            Some(span) => Err(self
                .err_at(
                    ErrorCode::E0105,
                    format!("`@sending` is not supported on {place}"),
                    span,
                )
                .with_help(
                    "declare `@sending` on a concrete top-level function or its parameters; \
                     call such a function from this declaration instead",
                )),
            None => Ok(()),
        }
    }

    /// `Ok(None)`: the item was consumed but rejected, with its diagnostic
    /// already recorded in `recovered_errors` (see [`Self::parse_const`]).
    pub(super) fn parse_item(&mut self) -> Result<Option<Item>, Diagnostic> {
        let contracts = self.parse_effect_contracts()?;
        let modifiers = self.parse_item_modifiers();
        let public = modifiers.contains(Modifiers::PUBLIC);
        if !contracts.is_empty() && !self.check(TokenKind::Fn) {
            return Err(self.err(
                ErrorCode::E0105,
                "effect contracts require a function or method",
            ));
        }
        match self.peek_kind() {
            TokenKind::Fn => {
                let mut function =
                    self.parse_fn(modifiers & (Modifiers::PUBLIC | Modifiers::ASYNC))?;
                if !function.type_params.is_empty() {
                    self.reject_sending(&contracts, &function.params, "generic functions")?;
                }
                function.effect_contracts = contracts;
                Ok(Some(Item::Function(function)))
            }
            TokenKind::Class => Ok(Some(Item::Class(
                self.parse_class(modifiers & (Modifiers::PUBLIC | Modifiers::OPEN))?,
            ))),
            TokenKind::Enum => Ok(Some(Item::Enum(self.parse_enum_decl(public)?))),
            TokenKind::Const if modifiers.contains(Modifiers::OPEN) => Err(self.err(
                ErrorCode::E0105,
                "`open` cannot be used on a `const` declaration",
            )),
            TokenKind::Const if modifiers.contains(Modifiers::ASYNC) => Err(self.err(
                ErrorCode::E0105,
                "`async` cannot be used on a `const` declaration",
            )),
            TokenKind::Const => Ok(self
                .parse_const(modifiers & Modifiers::PUBLIC)?
                .map(Item::Function)),
            TokenKind::Interface if modifiers.contains(Modifiers::OPEN) => Err(self.err(
                ErrorCode::E0105,
                "`open` cannot be used on an interface declaration",
            )),
            TokenKind::Interface if modifiers.contains(Modifiers::ASYNC) => Err(self.err(
                ErrorCode::E0105,
                "`async` cannot be used on an interface declaration",
            )),
            TokenKind::Interface => Ok(Some(Item::Interface(self.parse_interface(public)?))),
            _ if modifiers.contains(Modifiers::ASYNC) => {
                Err(self.err(ErrorCode::E0105, "`async` can only be used on `fn`"))
            }
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
            if self.check(TokenKind::Colon) {
                let bound = self.unsupported_bound(&name, false);
                self.recovered_errors.push(bound);
            }
            params.push(name);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Gt)?;
        Ok(params)
    }

    /// A `where T: Show` clause on a function or class declaration. Only a
    /// method's `where` restricts a class type parameter to listed types.
    /// Each predicate is reported and skipped so the declaration still parses.
    fn reject_where_clause(&mut self) -> Result<(), Diagnostic> {
        if !matches!(self.peek_kind(), TokenKind::Ident(n) if n == "where") {
            return Ok(());
        }
        let mut keyword = Some(self.current_span());
        self.advance();
        loop {
            let start = keyword.take().unwrap_or_else(|| self.current_span());
            let TokenKind::Ident(parameter) = self.peek_kind().clone() else {
                return Err(unsupported_where(start));
            };
            self.advance();
            if !self.check(TokenKind::Colon) {
                return Err(unsupported_where(start));
            }
            let mut diagnostic = self.unsupported_bound(&parameter, true);
            if let Some(label) = diagnostic.labels.first_mut() {
                label.span = start.to(label.span);
            }
            self.recovered_errors.push(diagnostic);
            if !self.eat(TokenKind::Comma) {
                return Ok(());
            }
        }
    }

    /// `<T: Show>` or `where T: Show`, with the parser at the `:`. Willow
    /// generics take every type, so report the bound with the two forms that
    /// express the requirement instead.
    fn unsupported_bound(&mut self, parameter: &str, clause: bool) -> Diagnostic {
        let start = self.current_span();
        self.advance();
        let mut bounds = Vec::new();
        while let Ok(ty) = self.parse_type() {
            bounds.push(bound_spelling(&ty));
            if !self.eat(TokenKind::Plus) {
                break;
            }
        }
        let span = start.to(self.previous_span());
        let bound = bounds.join(" + ");
        let interface = bounds.first().map_or("Show", String::as_str);
        let written = if clause {
            format!("where {parameter}: {bound}")
        } else {
            format!("{parameter}: {bound}")
        };
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0102,
            format!("type parameter bounds are not supported: `{written}`"),
        )
        .with_label(Label::primary(span, "bound on a type parameter"))
        .with_note(format!(
            "a type parameter accepts every type, so a generic body may only use operations valid for all of them; `{parameter}` cannot require an interface"
        ))
        .with_help(format!(
            "take the interface itself instead of `{parameter}`, as in `x: {interface}` or `Array<{interface}>`, so method calls dispatch through it; or keep `{parameter}` and pass the operation as a function parameter, as in `op: fn({parameter}) -> String`"
        ))
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

    pub(super) fn parse_class(&mut self, modifiers: Modifiers) -> Result<ClassDecl, Diagnostic> {
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

        self.reject_where_clause()?;
        self.expect(TokenKind::LBrace)?;

        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut constructors = Vec::new();

        while !self.check(TokenKind::RBrace) && !self.at_eof() {
            let contracts = self.parse_effect_contracts()?;
            match self.parse_member_head(&contracts)? {
                MemberHead::Method(modifiers) => {
                    let mut method = self.parse_method(modifiers)?;
                    self.reject_sending(&contracts, &method.params, "methods")?;
                    method.effect_contracts = contracts;
                    methods.push(method);
                }
                MemberHead::Constructor(modifiers) => {
                    let constructor = self.parse_constructor(modifiers)?;
                    self.reject_sending(&contracts, &constructor.params, "constructors")?;
                    constructors.push(constructor)
                }
                MemberHead::Field(modifiers) => fields.push(self.parse_field(modifiers)?),
            }
        }

        let end = self.current_span();
        self.expect(TokenKind::RBrace)?;
        let span = start.to(end);

        Ok(ClassDecl {
            abi_storage: None,
            type_params,
            name,
            modifiers,
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
        // Optional generic type parameters: `<T>` or `<T, U>`.
        let mut type_params = Vec::new();
        if self.eat(TokenKind::Lt) {
            while !matches!(self.peek_kind(), TokenKind::Gt | TokenKind::Eof) {
                let parameter = self.expect_ident()?;
                if self.check(TokenKind::Colon) {
                    let bound = self.unsupported_bound(&parameter, false);
                    self.recovered_errors.push(bound);
                }
                type_params.push(parameter);
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.advance();
                }
            }
            self.expect(TokenKind::Gt)?;
        }
        // Optional super-interfaces: `interface B extends A`.
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
            let contracts = self.parse_effect_contracts()?;
            // Interface members carry no visibility/modifier keywords: methods are
            // public by contract. A leading `pub`/`prot`/`open`/etc. is not allowed.
            // Static interface members are out of scope (static member support §17 → E0836).
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
            let mut method = self.parse_interface_method()?;
            self.reject_sending(&contracts, &method.params, "interface methods")?;
            method.effect_contracts = contracts;
            methods.push(method);
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
            template_checked: false,
            span,
        })
    }

    async fn parse_method_params(
        &mut self,
        is_static: bool,
    ) -> Result<(bool, Vec<Param>), Diagnostic> {
        self.expect(TokenKind::LParen)?;
        let has_self = self.check(TokenKind::SelfKw);
        if has_self && is_static {
            let span = self.current_span();
            return Err(Diagnostic::new(
                Severity::Error,
                ErrorCode::E0831,
                "static methods cannot take `self`",
            )
            .with_label(Label::primary(span, "`self` in a static method"))
            .with_help("remove `self`, or make this an instance method by dropping `static`"));
        }
        let mut params = Vec::new();
        let mut more = if has_self {
            self.advance();
            self.eat(TokenKind::Comma) && !self.check(TokenKind::RParen)
        } else {
            !self.check(TokenKind::RParen) && !self.at_eof()
        };
        while more {
            params.push(self.parse_param_cont().await?);
            // After an explicit receiver and a named parameter, a comma
            // requires another parameter, even at ')' or EOF.
            more = self.eat(TokenKind::Comma)
                && (has_self || (!self.check(TokenKind::RParen) && !self.at_eof()));
        }
        self.expect(TokenKind::RParen)?;
        Ok((has_self, params))
    }

    pub(super) fn parse_interface_method(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
        let scope = self.default_import_usage.scope();
        let result = self.parse_interface_method_inner().await;
        self.default_import_usage.restore(scope);
        result
    }

    async fn parse_interface_method_inner(&mut self) -> Result<InterfaceMethodDecl, Diagnostic> {
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
        let (has_self, params) = self.parse_method_params(false).await?;

        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type_cont().await?
        } else {
            Type::Void
        };

        // A method with a body (`{ ... }`) is a DEFAULT method;
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
            let body = self.parse_callable_block_cont().await?;
            let end = self.previous_span();
            (Some(body), end)
        } else {
            let end = self.current_span();
            self.expect(TokenKind::Semicolon)?;
            (None, end)
        };
        let span = start.to(end);

        Ok(InterfaceMethodDecl {
            effect_contracts: Vec::new(),
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

    pub(super) fn parse_field(&mut self, modifiers: Modifiers) -> Result<FieldDecl, Diagnostic> {
        let is_static = modifiers.contains(Modifiers::STATIC);
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
            modifiers,
            initializer,
            span,
        })
    }

    pub(super) fn parse_method(&mut self, modifiers: Modifiers) -> Result<MethodDecl, Diagnostic> {
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
        let (_, mut params) = self
            .parse_method_params(modifiers.contains(Modifiers::STATIC))
            .await?;
        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type()?
        } else {
            Type::Void
        };
        let constraint = if matches!(self.peek_kind(), TokenKind::Ident(n) if n == "where") {
            self.advance();
            let parameter_span = self.current_span();
            let parameter = self.expect_ident()?;
            self.type_uses.push(TypeUse {
                name: parameter.clone(),
                span: parameter_span,
            });
            self.expect(TokenKind::Colon)?;
            let mut alternatives = vec![self.parse_type()?];
            while self.eat(TokenKind::Pipe) {
                alternatives.push(self.parse_type()?);
            }
            Some(Box::new(MethodConstraint {
                parameter,
                alternatives,
            }))
        } else {
            None
        };
        let body = self.parse_callable_block()?;
        let span = start.to(body.span);
        let mut return_type = return_type;
        if modifiers.contains(Modifiers::ASYNC) {
            sync_callable_signature(&mut params, &mut return_type);
        }

        Ok(MethodDecl {
            effect_contracts: Vec::new(),
            array_body_optimization: crate::parser::ast::ArrayBodyOptimization::None,
            constraint,
            name,
            modifiers,
            params,
            return_type,
            body,
            span,
            is_default_injected: false,
            is_interface_default: false,
        })
    }

    /// Parse an `init(self, params...) { body }` constructor. No
    /// return type is allowed; the explicit `self` receiver is not stored in
    /// `params`, matching method lowering and `new Class(args...)` arity.
    pub(super) fn parse_constructor(
        &mut self,
        modifiers: Modifiers,
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
        // Constructors must not declare a return type (constructor support §4.2).
        if self.eat(TokenKind::Arrow) {
            return Err(self.err(
                ErrorCode::E0840,
                "constructor `init` must not declare a return type",
            ));
        }
        let body = self.parse_block()?;
        let span = start.to(body.span);
        Ok(ConstructorDecl {
            modifiers,
            params,
            body,
            span,
        })
    }

    pub(super) fn parse_fn(&mut self, modifiers: Modifiers) -> Result<FunctionDecl, Diagnostic> {
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

        let (return_type, return_type_span) = if self.eat(TokenKind::Arrow) {
            let start = self.current_span();
            let ty = self.parse_type()?;
            (ty, Some(Box::new(start.to(self.previous_span()))))
        } else {
            (Type::Void, None)
        };

        self.reject_where_clause()?;
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        while let TokenKind::Ident(keyword) = self.peek_kind() {
            let post = match keyword.as_str() {
                "requires" => false,
                "ensures" => true,
                _ => break,
            };
            self.advance();
            let previous = self.allow_object_literals;
            self.allow_object_literals = false;
            let expression = self.parse_expr();
            self.allow_object_literals = previous;
            let expression = expression?;
            if post {
                ensures.push(expression);
            } else {
                requires.push(expression);
            }
        }
        let body = self.parse_callable_block()?;
        let span = start.to(body.span);
        let mut return_type = return_type;
        if modifiers.contains(Modifiers::ASYNC) {
            sync_callable_signature(&mut params, &mut return_type);
        }
        Ok(FunctionDecl {
            requires,
            ensures,
            effect_contracts: Vec::new(),
            type_params,
            name,
            modifiers,
            params,
            return_type,
            return_type_span,
            body,
            span,
            rust_bridge: None,
            constant: None,
        })
    }

    /// `[pub] const NAME: T = literal;`. The constant is
    /// represented as a zero-parameter function returning the literal and
    /// carrying the value in [`FunctionDecl::constant`]; see there.
    ///
    /// A syntactically complete declaration whose initializer or type is not a
    /// supported constant yields `Ok(None)` with its diagnostic recorded in
    /// `recovered_errors`: the whole declaration, `;` included, is consumed, so
    /// item recovery must not run and skip the following item.
    pub(super) fn parse_const(
        &mut self,
        modifiers: Modifiers,
    ) -> Result<Option<FunctionDecl>, Diagnostic> {
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
            requires: Vec::new(),
            ensures: Vec::new(),
            effect_contracts: Vec::new(),
            type_params: Vec::new(),
            name,
            modifiers,
            params: Vec::new(),
            return_type,
            return_type_span: Some(Box::new(type_span)),
            body,
            span,
            rust_bridge: None,
            constant: Some(constant),
        }))
    }

    pub(super) fn parse_param(&mut self) -> Result<Param, Diagnostic> {
        let sending = if self.eat(TokenKind::At) {
            if self.expect_ident()? != "sending" {
                return Err(self.err_at(
                    ErrorCode::E0102,
                    "unknown parameter attribute; expected `sending`",
                    self.previous_span(),
                ));
            }
            true
        } else {
            false
        };
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
            sending,
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
    if !matches!(
        declared,
        Type::I8
            | Type::I16
            | Type::I32
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::Char
            | Type::U8
            | Type::I64
            | Type::F64
            | Type::Bool
            | Type::String
    ) {
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
    if let (ConstValue::Integer(value), Some(layout)) = (&constant, declared.integer_layout()) {
        if layout.contains(*value) {
            return Ok(constant);
        }
        return Err(Diagnostic::new(
            Severity::Error,
            ErrorCode::E0110,
            format!(
                "integer literal is outside {}..={} for `{}`",
                layout.min(),
                layout.max(),
                type_name(declared)
            ),
        )
        .with_label(Label::primary(type_span, "constant out of range")));
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
            Type::Array(_) | Type::FixedArray(..) => true,
            Type::Named(name) => name == "Array" || name == "FrozenArray",
            Type::Generic(name, _) => name == "Array" || name == "FrozenArray",
            _ => false,
        }
}

/// The replacement for a constant array: a frozen array built once in a class
/// static property. A function returning an array literal would rebuild the
/// array on every call.
fn frozen_array_static_help(declared: &Type) -> String {
    let element = match declared {
        Type::Array(element) | Type::FixedArray(element, _) => {
            crate::semantic::type_checker::types::type_name::<String>(element)
        }
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
        Expr::Char(value, ..) => Some(ConstValue::Char(*value)),
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

    // a rejected `const` is fully consumed, so recovery must not
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
            ("const A: i32 = 1; const B: i64 = 2;", &[], &["A", "B"]),
            (
                "const A: i32 = 2147483648; const B: i64 = 2;",
                &[ErrorCode::E0110],
                &["B"],
            ),
            (
                "const A: Array<i64> = 1; fn main() {}",
                &[ErrorCode::E0110],
                &["main"],
            ),
            (
                "const A: f64 = 1; const B: Array<i64> = [1]; const C: i32 = 2147483648; const D: bool = 1;",
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

#[cfg(test)]
mod effect_contract_diagnostic_tests {
    use super::*;
    use crate::diagnostics::FileId;
    use crate::lexer::Lexer;

    thread_local! {
        pub(super) static CONTRACT_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(super) static EDIT_CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn purity_spelling_normalization_and_bounded_work() {
        for (attributes, expected) in [
            ("@pure", 1),
            ("@effects(pure)", 1),
            ("@verified", 2),
            ("@pure @verified", 2),
            ("@verified @pure", 2),
            ("@effects(pure) @pure", 1),
            ("@pure @effects(pure)", 1),
            ("@effects(pure, pure)", 1),
            ("@effects(pure, no_panic)", 2),
            ("@verified @effects(pure, no_panic)", 3),
            (
                "@effects(no_wait, no_block, no_suspend, no_io, no_panic, no_alloc, pure) @verified @sending",
                9,
            ),
        ] {
            for count in [1, 16, 256, 4096] {
                let source = format!("{} fn f() {{}}", attributes.repeat(count));
                let tokens = Lexer::new(&source).tokenize().unwrap();
                CONTRACT_COMPARISONS.with(|n| n.set(0));
                let (program, errors) = Parser::new(tokens).parse();
                assert!(errors.is_empty(), "{attributes}: {errors:?}");
                let Item::Function(f) = &program.items[0] else {
                    panic!("function")
                };
                assert_eq!(f.effect_contracts.len(), expected);
                let pure = f
                    .effect_contracts
                    .iter()
                    .find(|c| c.kind == EffectContractKind::Pure)
                    .unwrap();
                assert!(pure.span.end <= attributes.len());
                // At most nine bounded set comparisons per written contract.
                assert!(CONTRACT_COMPARISONS.with(|n| n.get()) <= 81 * count);
                let restored: Program =
                    serde_json::from_str(&serde_json::to_string(&program).unwrap()).unwrap();
                let Item::Function(restored) = &restored.items[0] else {
                    panic!("function")
                };
                assert_eq!(f.effect_contracts, restored.effect_contracts);
            }
        }
    }

    #[test]
    fn effect_contract_unknown_names_have_precise_spans_and_help() {
        // Substitution, deletion, insertion, case, distant name and stable tie.
        for (name, closest) in [
            ("pur", "pure"),
            ("no_wai", "no_wait"),
            ("no_waitt", "no_wait"),
            ("no_wxit", "no_wait"),
            ("NO_WAIT", "no_wait"),
            ("no_bloc", "no_block"),
            ("no_blockk", "no_block"),
            ("no_blook", "no_block"),
            ("no_allo", "no_alloc"),
            ("no_pani", "no_panic"),
            ("no_suspen", "no_suspend"),
            ("no_suspendd", "no_suspend"),
            ("no_suspond", "no_suspend"),
            ("no_", "no_io"),
        ] {
            for source in [
                format!("@effects({name}) fn f() {{}}"),
                format!("@effects(no_wait, {name}, no_block) fn f() {{}}"),
                format!("@effects(no_wait)\n@effects({name}) fn f() {{}}"),
                format!("class C {{ @effects({name}) fn f(self) {{}} }}"),
                format!("interface I {{ @effects({name}) fn f(self); }}"),
                format!("// offset\n@effects(  {name}  ) fn f() {{}}"),
            ] {
                let tokens = Lexer::with_file_id(&source, FileId(7)).tokenize().unwrap();
                let (_, errors) = Parser::new(tokens).parse();
                let error = errors
                    .iter()
                    .find(|e| e.message.contains("unknown effect contract"))
                    .unwrap();
                assert_eq!(error.code, ErrorCode::E0102);
                let span = error.labels[0].span;
                assert_eq!(span.file_id, FileId(7));
                assert_eq!(&source[span.start..span.end], name, "{source}");
                assert_eq!(
                    error.helps,
                    [format!(
                        "valid effect contracts: no_wait, no_block, no_suspend, no_io, no_panic, no_alloc, pure; did you mean `{closest}`?"
                    )]
                );
            }
        }
    }

    #[test]
    fn effect_contract_valid_names_and_syntax_errors_are_preserved() {
        for attribute in [
            "@effects(no_wait)",
            "@effects(no_block)",
            "@effects(no_suspend)",
            "@effects(no_io)",
            "@effects(no_wait, no_block, no_suspend)",
            "@pure",
            "@verified",
        ] {
            let source = format!("{attribute} fn f() {{}}");
            let tokens = Lexer::new(&source).tokenize().unwrap();
            assert!(Parser::new(tokens).parse().1.is_empty(), "{source}");
        }
        for attribute in [
            "@effects()",
            "@effects(1)",
            "@effects(no_wait,)",
            "@effects(no_wait",
            "@unknown",
        ] {
            let source = format!("{attribute} fn f() {{}}");
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let errors = Parser::new(tokens).parse().1;
            assert!(!errors.is_empty(), "{source}");
            assert!(
                errors
                    .iter()
                    .all(|e| e.helps.iter().all(|h| !h.contains("did you mean")))
            );
        }
    }

    #[test]
    fn effect_contract_suggestion_work_is_linear_in_name_length() {
        for length in [16, 256, 4096] {
            EDIT_CELLS.with(|n| n.set(0));
            closest_effect_contract(&"x".repeat(length));
            EDIT_CELLS.with(|n| assert_eq!(n.get(), 50 * length));
        }
    }
}

/// An async body runs as a task that may resume on another worker, so the
/// function values its signature carries must be shareable: every written
/// `fn(...)` there is the Send + Sync callable kind.
pub(super) fn sync_callable_signature(params: &mut [Param], return_type: &mut Type) {
    for param in params {
        param.ty = param.ty.with_callable_kind(true);
    }
    *return_type = return_type.with_callable_kind(true);
}

fn unsupported_where(span: Span) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0102,
        "`where` clauses are not supported here",
    )
    .with_label(Label::primary(span, "`where` clause"))
}

/// A bound as written, for diagnostics: an interface name with its arguments.
fn bound_spelling(ty: &Type) -> String {
    let list = |types: &[Type]| {
        types
            .iter()
            .map(bound_spelling)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match ty {
        Type::I8 => "i8".to_string(),
        Type::I16 => "i16".to_string(),
        Type::I32 => "i32".to_string(),
        Type::I64 => "i64".to_string(),
        Type::U8 => "u8".to_string(),
        Type::U16 => "u16".to_string(),
        Type::U32 => "u32".to_string(),
        Type::U64 => "u64".to_string(),
        Type::Char => "char".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Never => "!".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(element) => format!("Array<{}>", bound_spelling(element)),
        Type::FixedArray(element, n) => format!("[{}; {n}]", bound_spelling(element)),
        Type::Generic(name, args) => format!("{name}<{}>", list(args)),
        Type::Fn(params, ret) | Type::Closure(params, ret) => {
            format!("fn({}) -> {}", list(params), bound_spelling(ret))
        }
    }
}

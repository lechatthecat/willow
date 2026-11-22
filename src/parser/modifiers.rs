//! Ordered declaration prefixes. Each entry point consumes only the modifiers
//! admitted at that position, leaving the declaration token for its parser.
use super::{
    Parser,
    ast::{EffectContract, Modifiers},
};
use crate::{
    diagnostics::{Diagnostic, ErrorCode, Label, Severity},
    lexer::token::TokenKind,
};

#[derive(Clone, Copy)]
enum Site {
    Item,
    Member,
}

pub(super) enum MemberHead {
    Constructor(Modifiers),
    Method(Modifiers),
    Field(Modifiers),
}

impl Parser {
    fn visibility(&mut self, site: Site) -> Modifiers {
        if self.eat(TokenKind::Pub) {
            Modifiers::PUBLIC
        } else if matches!(site, Site::Member) && self.eat(TokenKind::Prot) {
            Modifiers::PROTECTED
        } else {
            Modifiers::empty()
        }
    }

    // `open` is contextual: at declaration modifier positions only. A field
    // named `open` is followed by `:`, so leave it for ordinary name parsing.
    fn eat_open_modifier(&mut self) -> bool {
        if matches!(self.peek_kind(), TokenKind::Ident(name) if name == "open")
            && !matches!(self.peek_kind_at(1), TokenKind::Colon)
        {
            self.advance();
            true
        } else {
            false
        }
    }

    pub(super) fn parse_async_modifier(&mut self, mut modifiers: Modifiers) -> Modifiers {
        modifiers.set(Modifiers::ASYNC, self.eat(TokenKind::Async));
        modifiers
    }

    fn callable_modifiers(&mut self, mut modifiers: Modifiers, site: Site) -> Modifiers {
        modifiers.set(Modifiers::OPEN, self.eat_open_modifier());
        if matches!(site, Site::Member) {
            modifiers.set(Modifiers::OVERRIDE, self.eat(TokenKind::Override));
        }
        self.parse_async_modifier(modifiers)
    }

    pub(super) fn parse_item_modifiers(&mut self) -> Modifiers {
        let visibility = self.visibility(Site::Item);
        self.callable_modifiers(visibility, Site::Item)
    }

    fn is_constructor_init_ahead(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(n) if n == "init")
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::LParen)
            )
    }

    pub(super) fn parse_member_head(
        &mut self,
        contracts: &[EffectContract],
    ) -> Result<MemberHead, Diagnostic> {
        let mut modifiers = self.visibility(Site::Member);
        if self.is_constructor_init_ahead() {
            if !contracts.is_empty() {
                return Err(self.err(ErrorCode::E0105, "effect contracts require a method"));
            }
            return Ok(MemberHead::Constructor(modifiers));
        }
        if self.eat(TokenKind::Static) {
            if self.is_constructor_init_ahead() {
                let static_span = self.previous_span();
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
            modifiers.insert(Modifiers::STATIC);
        }
        let mut modifiers = self.callable_modifiers(modifiers, Site::Member);
        if self.check(TokenKind::Fn) {
            return Ok(MemberHead::Method(modifiers));
        }
        if !contracts.is_empty() {
            return Err(self.err(ErrorCode::E0105, "effect contracts require a method"));
        }
        if modifiers.intersects(Modifiers::OPEN | Modifiers::OVERRIDE | Modifiers::ASYNC) {
            return Err(self.err(
                ErrorCode::E0105,
                "`open`, `override`, and `async` can only be used on methods",
            ));
        }
        if self.eat(TokenKind::Mut) {
            if !modifiers.contains(Modifiers::STATIC) {
                return Err(self.err(
                    ErrorCode::E0105,
                    "`mut` on a class field is only allowed on a `static` property",
                ));
            }
            modifiers.insert(Modifiers::MUT);
        }
        Ok(MemberHead::Field(modifiers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::Lexer, parser::ast::Program};
    use serde_json::{Value, json};

    #[test]
    fn declaration_modifiers_preserve_flat_artifact_fields() {
        let source = "pub open class C {
            prot init(self) {} pub static mut open: i64 = 1;
            prot static open override async fn work() {}
        } pub open async fn f() {}";
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let wire = serde_json::to_value(&program).unwrap();
        let class = &wire["items"][0]["Class"];
        let function = &wire["items"][1]["Function"];
        for (decl, expected) in [
            (class, json!({"public": true, "is_open": true})),
            (
                &class["constructors"][0],
                json!({"public": false, "protected": true}),
            ),
            (
                &class["fields"][0],
                json!({"public": true, "protected": false, "is_static": true, "is_mut": true}),
            ),
            (
                &class["methods"][0],
                json!({"public": false, "protected": true, "is_async": true, "is_open": true, "is_override": true, "is_static": true}),
            ),
            (function, json!({"public": true, "is_async": true})),
        ] {
            let actual: serde_json::Map<_, _> = decl
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "public"
                            | "protected"
                            | "is_static"
                            | "is_async"
                            | "is_open"
                            | "is_override"
                            | "is_mut"
                            | "modifiers"
                    )
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            assert_eq!(Value::Object(actual), expected);
        }
        let decoded: Program = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }
}

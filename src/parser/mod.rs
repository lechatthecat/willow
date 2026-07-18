pub mod ast;
mod decl;
mod expr;
mod pattern;
mod stmt;
mod types;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::lexer::token::{Token, TokenKind};
use ast::*;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Statement-level errors recovered inside blocks (willow-qzxg): the block
    /// keeps parsing after a bad statement, so one error no longer swallows the
    /// rest of the surrounding item (which used to cascade into a false
    /// `missing entry point` on `main`).
    recovered_errors: Vec<Diagnostic>,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            recovered_errors: Vec::new(),
        }
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

        errors.append(&mut self.recovered_errors);
        (
            Program {
                module,
                imports,
                items,
            },
            errors,
        )
    }

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
        // Three classes of next-token (mirrored by the exhaustive classifier in
        // this module's tests, which forces reclassification whenever a new
        // TokenKind is added):
        //   1. Unambiguous expression starts → ternary.
        //   2. Ambiguous tokens that ALSO continue a binary/lambda expression
        //      (`Minus`: `r? - 1` vs `c ? -1 : 2`; `Or`: `r? || b` vs
        //      `c ? || 1 : || 2`) → resolved by scanning ahead for the
        //      ternary's mandatory `:` at the same nesting level.
        //   3. Everything else → try-propagate.
        match next {
            Some(TokenKind::Minus | TokenKind::Or) => !self.ternary_colon_ahead(next_pos + 1),
            _ => !matches!(
                next,
                Some(
                    TokenKind::Integer(_)
                        | TokenKind::Float(_)
                        | TokenKind::True
                        | TokenKind::False
                        | TokenKind::Ident(_)
                        | TokenKind::StringLiteral(_)
                        | TokenKind::LParen
                        | TokenKind::LBracket
                        | TokenKind::Bang
                        | TokenKind::Ampersand
                        | TokenKind::Nil
                        | TokenKind::New
                        | TokenKind::SelfKw
                        | TokenKind::Match
                        | TokenKind::Await
                        | TokenKind::Select
                        | TokenKind::Pipe
                        | TokenKind::Print
                        | TokenKind::Println
                )
            ),
        }
    }

    /// Disambiguate an ambiguous token after `?` by looking for the ternary's
    /// mandatory `:` before the expression can end. Scans from `from`, tracking
    /// `(`/`[` nesting; a `Colon` at nesting level 0 means the `?` opened a
    /// ternary. Statement/argument boundaries (`;`, `=`, `{`, `}`, `,`, `=>`,
    /// unbalanced `)`/`]`, EOF) end the scan as try-propagate.
    fn ternary_colon_ahead(&self, from: usize) -> bool {
        let mut depth: i32 = 0;
        for token in self.tokens.iter().skip(from) {
            match &token.kind {
                TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => {
                    if depth == 0 {
                        return false; // closes an enclosing group — expression ended
                    }
                    depth -= 1;
                }
                TokenKind::Colon if depth == 0 => return true,
                TokenKind::Semicolon
                | TokenKind::Eq
                | TokenKind::LBrace
                | TokenKind::RBrace
                | TokenKind::FatArrow
                | TokenKind::Eof => return false,
                TokenKind::Comma if depth == 0 => return false,
                _ => {}
            }
        }
        false
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
        // `new` is a prefix keyword for object construction (`new Class(...)`),
        // matched before this point in expression position. As a plain
        // identifier — a method/function name like `Channel::new` or
        // `static fn new` — it is still accepted here (willow-scq2).
        if matches!(self.peek_kind(), TokenKind::New) {
            self.advance();
            return Ok("new".to_string());
        }
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

    #[test]
    fn constructor_01_explicit_self_is_required() {
        let errs = parse_errors("class User { pub init(name: String) {} }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0849));
    }

    #[test]
    fn constructor_02_explicit_self_is_not_stored_as_a_user_param() {
        let p = parse_ok("class User { pub init(self, name: String) {} }\n");
        let Item::Class(class) = &p.items[0] else {
            panic!("expected a class");
        };
        let ctor = &class.constructors[0];
        assert!(ctor.public);
        assert!(!ctor.protected);
        assert_eq!(ctor.params.len(), 1);
        assert_eq!(ctor.params[0].name, "name");
    }

    #[test]
    fn constructor_03_visibility_flags_are_preserved() {
        let p = parse_ok(
            r#"
class PrivateCtor { init(self) {} }
class PublicCtor { pub init(self) {} }
class ProtectedCtor { prot init(self) {} }
"#,
        );
        let flags = p
            .items
            .iter()
            .map(|item| match item {
                Item::Class(class) => {
                    let ctor = &class.constructors[0];
                    (class.name.as_str(), ctor.public, ctor.protected)
                }
                _ => panic!("expected classes only"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            flags,
            vec![
                ("PrivateCtor", false, false),
                ("PublicCtor", true, false),
                ("ProtectedCtor", false, true),
            ]
        );
    }

    #[test]
    fn constructor_04_self_receiver_must_be_bare() {
        let errs = parse_errors("class User { pub init(self: User) {} }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0849));
    }

    #[test]
    fn constructor_05_static_modifier_is_rejected() {
        let errs = parse_errors("class User { static init(self) {} }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0850));
    }

    #[test]
    fn constructor_06_fn_init_method_syntax_is_rejected() {
        let errs = parse_errors("class User { fn init(self) {} }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0850));
    }

    #[test]
    fn constructor_07_static_fn_init_method_syntax_is_rejected() {
        let errs = parse_errors("class User { static fn init() {} }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0850));
    }

    #[test]
    fn constructor_08_interface_fn_init_is_rejected() {
        let errs = parse_errors("interface Bad { fn init(self); }\nfn main() {}\n");
        assert!(errs.iter().any(|e| e.code == ErrorCode::E0850));
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
    fn interface_23_trait_like_example_file_parses() {
        // The on-disk runnable example must parse cleanly.
        let src = include_str!("../../example/trait_like_interfaces.wi");
        let p = parse_ok(src);
        assert!(p.items.iter().any(|i| matches!(i, Item::Interface(_))));
        let dog = first_class(&p);
        assert_eq!(dog.implements.len(), 2);
    }

    // ── ternary `?` vs try-propagate `?` disambiguation (willow-0g8j find) ──
    // The heuristic peeks at the token after `?`; every token that can START a
    // ternary then-branch must be listed, or the `?` is misread as try-propagate.
    // 8 perspectives: string/array/new/self/paren/negative/call/nested branches,
    // plus try-propagate regressions.

    #[test]
    fn ternary_q1_string_literal_branches() {
        let p = parse_ok("fn f(c: bool) -> String { let s = c ? \"a\" : \"b\"; return s; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    #[test]
    fn ternary_q2_array_literal_branches() {
        let p = parse_ok("fn f(c: bool) { let xs = c ? [1] : [2]; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    #[test]
    fn ternary_q3_new_branches() {
        let p = parse_ok("class A {} fn f(c: bool) { let x = c ? new A() : new A(); }");
        let f = function_named(&p, "f");
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    #[test]
    fn ternary_q4_self_branches_parse() {
        // `self` in expression position after `?` (method context).
        let p = parse_ok(
            "class A { v: i64; pub fn pick(self, c: bool) -> i64 { return c ? self.v : 0; } }",
        );
        assert!(p.items.iter().any(|i| matches!(i, Item::Class(_))));
    }

    #[test]
    fn ternary_q5_string_in_call_argument() {
        // The original failing shape: a ternary with string branches inside
        // a call's argument list.
        let p = parse_ok("fn f(c: bool) { println(c ? \"a\" : \"b\"); }");
        let f = first_function(&p);
        assert!(matches!(f.body.stmts[0], Stmt::Expr(_)));
    }

    #[test]
    fn ternary_q6_try_propagate_still_parses_before_semicolon() {
        // `expr?;` — `?` followed by `;` stays try-propagate.
        let p = parse_ok(
            "fn f(r: Result<i64, String>) -> Result<i64, String> { let v = r?; return r; }",
        );
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::TryPropagate(_, _))
        ));
    }

    #[test]
    fn ternary_q7_try_propagate_in_arithmetic() {
        // `a? + b?` — `?` followed by an operator stays try-propagate.
        let p = parse_ok(
            "fn f(a: Result<i64, String>, b: Result<i64, String>) -> Result<i64, String> { let v = a? + b?; return a; }",
        );
        let f = first_function(&p);
        assert!(matches!(f.body.stmts[0], Stmt::Let(_)));
    }

    #[test]
    fn ternary_q8_nested_ternary_with_strings() {
        let p = parse_ok(
            "fn f(a: bool, b: bool) -> String { let s = a ? \"x\" : b ? \"y\" : \"z\"; return s; }",
        );
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    // ── exhaustive `?`-disambiguation classifier ────────────────────────────
    // `classify_after_question` matches EVERY TokenKind with no wildcard arm.
    // Adding a new token variant breaks this compile, forcing a decision on
    // whether it can start a ternary then-branch — the exact failure mode that
    // produced willow-7qt5 (StringLiteral & co. missing from the heuristic).

    /// How a `?` reads when this token follows it.
    #[derive(Debug, PartialEq)]
    enum AfterQuestion {
        /// Unambiguous expression start → ternary.
        Ternary,
        /// Cannot start an expression → try-propagate.
        Try,
        /// Both readings are grammatical → resolved by the colon-scan
        /// (`ternary_colon_ahead`): a `:` at the same nesting level means
        /// ternary, otherwise try-propagate.
        Contextual,
    }

    fn classify_after_question(kind: &TokenKind) -> AfterQuestion {
        match kind {
            // Expression-starting tokens → ternary.
            TokenKind::Integer(_)
            | TokenKind::Float(_)
            | TokenKind::StringLiteral(_)
            | TokenKind::Ident(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Nil
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::Bang
            | TokenKind::Ampersand // reference marker only exists in call args
            | TokenKind::New
            | TokenKind::SelfKw
            | TokenKind::Match
            | TokenKind::Await
            | TokenKind::Select
            | TokenKind::Pipe
            | TokenKind::Print
            | TokenKind::Println => AfterQuestion::Ternary,

            // `r? - 1` (binary minus on the try result) vs `c ? -1 : 2`
            // (negative then-branch); `r? || b` vs `c ? || 1 : || 2` (lambda).
            TokenKind::Minus | TokenKind::Or => AfterQuestion::Contextual,

            // Binary/postfix operators and punctuation cannot start an
            // expression → try-propagate.
            TokenKind::Plus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Eq
            | TokenKind::EqEq
            | TokenKind::BangEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq
            | TokenKind::And
            | TokenKind::Question
            | TokenKind::Semicolon
            | TokenKind::Colon
            | TokenKind::ColonColon
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::DotDot
            | TokenKind::LBrace
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::Arrow
            | TokenKind::FatArrow
            | TokenKind::Eof => AfterQuestion::Try,

            // Keywords that cannot start an expression → try-propagate.
            TokenKind::Fn
            | TokenKind::Let
            | TokenKind::Mut
            | TokenKind::If
            | TokenKind::Else
            | TokenKind::While
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Defer
            | TokenKind::For
            | TokenKind::In
            | TokenKind::Return
            | TokenKind::Class
            | TokenKind::Pub
            | TokenKind::Prot
            | TokenKind::Open
            | TokenKind::Override
            | TokenKind::Static
            | TokenKind::Extends
            | TokenKind::Interface
            | TokenKind::Implements
            | TokenKind::Import
            | TokenKind::Module
            | TokenKind::As
            | TokenKind::Async
            | TokenKind::Enum
            | TokenKind::I64
            | TokenKind::Bool
            | TokenKind::F64 => AfterQuestion::Try,
        }
    }

    /// One concrete instance of every TokenKind variant. Kept next to
    /// `classify_after_question`: extend BOTH when adding a token.
    fn all_token_kinds() -> Vec<TokenKind> {
        vec![
            TokenKind::Fn,
            TokenKind::Let,
            TokenKind::Mut,
            TokenKind::If,
            TokenKind::Else,
            TokenKind::While,
            TokenKind::Break,
            TokenKind::Continue,
            TokenKind::Defer,
            TokenKind::For,
            TokenKind::In,
            TokenKind::Return,
            TokenKind::Print,
            TokenKind::Println,
            TokenKind::True,
            TokenKind::False,
            TokenKind::Nil,
            TokenKind::Class,
            TokenKind::Pub,
            TokenKind::Prot,
            TokenKind::Open,
            TokenKind::Override,
            TokenKind::Static,
            TokenKind::New,
            TokenKind::Extends,
            TokenKind::Interface,
            TokenKind::Implements,
            TokenKind::SelfKw,
            TokenKind::Import,
            TokenKind::Module,
            TokenKind::As,
            TokenKind::Async,
            TokenKind::Await,
            TokenKind::Select,
            TokenKind::Match,
            TokenKind::Enum,
            TokenKind::ColonColon,
            TokenKind::I64,
            TokenKind::Bool,
            TokenKind::F64,
            TokenKind::Integer(1),
            TokenKind::Float(1.0),
            TokenKind::StringLiteral("s".to_string()),
            TokenKind::Ident("x".to_string()),
            TokenKind::Plus,
            TokenKind::Minus,
            TokenKind::Star,
            TokenKind::Slash,
            TokenKind::Percent,
            TokenKind::Eq,
            TokenKind::EqEq,
            TokenKind::BangEq,
            TokenKind::Lt,
            TokenKind::LtEq,
            TokenKind::Gt,
            TokenKind::GtEq,
            TokenKind::And,
            TokenKind::Ampersand,
            TokenKind::Or,
            TokenKind::Pipe,
            TokenKind::Bang,
            TokenKind::Question,
            TokenKind::Semicolon,
            TokenKind::Colon,
            TokenKind::Comma,
            TokenKind::Dot,
            TokenKind::DotDot,
            TokenKind::LBrace,
            TokenKind::RBrace,
            TokenKind::LParen,
            TokenKind::RParen,
            TokenKind::LBracket,
            TokenKind::RBracket,
            TokenKind::Arrow,
            TokenKind::FatArrow,
            TokenKind::Eof,
        ]
    }

    // The runtime heuristic must agree with the exhaustive classifier for
    // EVERY token kind. A new variant first breaks `classify_after_question`
    // at compile time; this test then catches any drift between the
    // classification and `is_try_propagate_question`.
    #[test]
    fn ternary_q9_heuristic_matches_exhaustive_classifier() {
        let tok = |kind: TokenKind| Token {
            kind,
            span: Span::dummy(),
        };
        for kind in all_token_kinds() {
            // Minimal context: `? X <eof>` — a Contextual token has no ternary
            // colon ahead, so it must read as try-propagate.
            let expected_try = match classify_after_question(&kind) {
                AfterQuestion::Ternary => false,
                AfterQuestion::Try | AfterQuestion::Contextual => true,
            };
            let parser = Parser::new(vec![
                tok(TokenKind::Question),
                tok(kind.clone()),
                tok(TokenKind::Eof),
            ]);
            assert_eq!(
                parser.is_try_propagate_question(),
                expected_try,
                "minimal-context drift for {kind:?}"
            );

            // Colon context: `? X x : <eof>` — a Contextual token now sees the
            // ternary's colon and must read as a ternary.
            if classify_after_question(&kind) == AfterQuestion::Contextual {
                let parser = Parser::new(vec![
                    tok(TokenKind::Question),
                    tok(kind.clone()),
                    tok(TokenKind::Ident("x".to_string())),
                    tok(TokenKind::Colon),
                    tok(TokenKind::Eof),
                ]);
                assert!(
                    !parser.is_try_propagate_question(),
                    "colon-context drift for {kind:?}"
                );
            }
        }
    }

    // Behavior checks for the newly classified expression-start tokens.
    #[test]
    fn ternary_q10_await_branches_parse() {
        let p = parse_ok(
            "async fn g() -> i64 { return 1; } \
             async fn f(c: bool) -> i64 { return c ? await g() : await g(); }",
        );
        assert!(p.items.len() == 2);
    }

    #[test]
    fn ternary_q11_lambda_branch_parses() {
        let p = parse_ok("fn f(c: bool) { let g = c ? |x: i64| x : |x: i64| x * 2; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    #[test]
    fn ternary_q12_try_then_logical_or_stays_try() {
        // `r? || b` — Or favors try-propagate by design.
        let p = parse_ok(
            "fn f(r: Result<bool, String>, b: bool) -> Result<bool, String> { let v = r? || b; return r; }",
        );
        let f = first_function(&p);
        assert!(matches!(f.body.stmts[0], Stmt::Let(_)));
    }

    #[test]
    fn ternary_q13_try_minus_binary_no_parens() {
        // `r? - 1` — no colon ahead → try-propagate feeding a subtraction.
        let p = parse_ok(
            "fn f(r: Result<i64, String>) -> Result<i64, String> { let v = r? - 1; return r; }",
        );
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Binary(_))
        ));
    }

    #[test]
    fn ternary_q14_negative_then_branch_still_ternary() {
        let p = parse_ok("fn f(c: bool) -> i64 { return c ? -1 : 2; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Return(ref r) if matches!(r.value, Some(Expr::Ternary(_)))
        ));
    }

    #[test]
    fn ternary_q15_empty_param_lambda_branches_no_parens() {
        // `c ? || 1 : || 2` — the colon-scan sees the ternary colon, so the
        // `||` reads as an empty-parameter lambda without parens.
        let p = parse_ok("fn f(c: bool) { let g = c ? || 1 : || 2; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Let(ref l) if matches!(l.init, Expr::Ternary(_))
        ));
    }

    #[test]
    fn ternary_q16_try_minus_inside_call_args() {
        // Inside call args the scan stops at the argument comma / closing
        // paren, so `f(r? - 1, x)` keeps the try-propagate reading.
        let p = parse_ok(
            "fn g(a: i64, b: i64) -> i64 { return a + b; } \
             fn f(r: Result<i64, String>) -> Result<i64, String> { let v = g(r? - 1, 2); return r; }",
        );
        assert_eq!(p.items.len(), 2);
    }

    #[test]
    fn ternary_q17_nested_ternary_colon_not_confused_by_parens() {
        // The scan skips colons inside nested parens/brackets.
        let p = parse_ok("fn f(c: bool, xs: Array<i64>) -> i64 { return c ? -xs[0] : xs[1]; }");
        let f = first_function(&p);
        assert!(matches!(
            f.body.stmts[0],
            Stmt::Return(ref r) if matches!(r.value, Some(Expr::Ternary(_)))
        ));
    }

    // ── assignment targets + statement-level recovery (willow-qzxg) ─────────
    // 10 parser perspectives (runtime behavior covered in integration tests):
    // 1 nested field target parses as FieldAssign, 2 index-then-field,
    // 3 call-then-field, 4 deep chain, 5 invalid call target -> E0106 (not a
    // misleading `expected ;`), 6 invalid literal target -> E0106, 7 the rest
    // of the block SURVIVES a bad statement (no false missing-main cascade),
    // 8 two bad statements -> two errors, 9 `==` never mistaken for `=`,
    // 10 recovery skips a nested brace block without desync.

    #[test]
    fn assign_t01_nested_field_target() {
        let p = parse_ok(
            "class B { pub v: i64; } class A { pub b: B; } \
                          fn f(a: A) { a.b.v = 2; }",
        );
        let f = function_named(&p, "f");
        assert!(matches!(f.body.stmts[0], Stmt::FieldAssign(_)));
    }

    #[test]
    fn assign_t02_index_then_field_target() {
        let p = parse_ok("class P { pub x: i64; } fn f(ps: Array<P>) { ps[0].x = 5; }");
        let f = function_named(&p, "f");
        assert!(matches!(f.body.stmts[0], Stmt::FieldAssign(_)));
    }

    #[test]
    fn assign_t03_call_then_field_target() {
        let p = parse_ok(
            "class P { pub x: i64; } fn make() -> P { return new P(1); } fn f() { make().x = 5; }",
        );
        let f = function_named(&p, "f");
        assert!(matches!(f.body.stmts[0], Stmt::FieldAssign(_)));
    }

    #[test]
    fn assign_t04_deep_chain_target() {
        let p = parse_ok(
            "class C { pub v: i64; } class B { pub c: C; } class A { pub b: B; } \
             fn f(a: A) { a.b.c.v = 9; }",
        );
        let f = function_named(&p, "f");
        assert!(matches!(f.body.stmts[0], Stmt::FieldAssign(_)));
    }

    #[test]
    fn assign_t05_call_target_rejected_with_e0106() {
        let errors = parse_errors("fn g() -> i64 { return 1; } fn main() { g() = 5; }");
        assert!(
            errors.iter().any(|e| format!("{:?}", e.code) == "E0106"),
            "{errors:?}"
        );
    }

    #[test]
    fn assign_t06_literal_target_rejected_with_e0106() {
        let errors = parse_errors("fn main() { 5 = 6; }");
        assert!(
            errors.iter().any(|e| format!("{:?}", e.code) == "E0106"),
            "{errors:?}"
        );
    }

    #[test]
    fn assign_t07_block_survives_bad_statement() {
        // The bad statement errors, but main and its later statements survive
        // — no false `missing entry point` cascade.
        let src = "fn g() -> i64 { return 1; } fn main() { g() = 5; println(7); }";
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, errors) = Parser::new(tokens).parse();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let main = function_named(&program, "main");
        assert_eq!(main.body.stmts.len(), 1, "later stmt survives");
    }

    #[test]
    fn assign_t08_two_bad_statements_two_errors() {
        let src = "fn main() { 1 = 2; 3 = 4; println(5); }";
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, errors) = Parser::new(tokens).parse();
        assert_eq!(errors.len(), 2, "{errors:?}");
        let main = function_named(&program, "main");
        assert_eq!(main.body.stmts.len(), 1);
    }

    #[test]
    fn assign_t09_equality_not_mistaken_for_assignment() {
        let p = parse_ok("class P { pub x: i64; } fn f(p: P) -> bool { return p.x == 5; }");
        let f = function_named(&p, "f");
        assert!(matches!(f.body.stmts[0], Stmt::Return(_)));
    }

    #[test]
    fn assign_t10_recovery_skips_nested_braces() {
        // The malformed statement contains a block; recovery must skip it
        // wholesale and resume at the enclosing block's next statement.
        let src = "fn main() { 1 = match 2 { _ => 3, }; println(4); }";
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, errors) = Parser::new(tokens).parse();
        assert!(!errors.is_empty());
        let main = function_named(&program, "main");
        assert!(
            main.body.stmts.iter().any(|s| matches!(s, Stmt::Expr(_))),
            "println survives: {:?}",
            main.body.stmts.len()
        );
    }

    // ── parser robustness mini-fuzzer (willow-qzxg bug CLASS detector) ──────
    // Systematic single-token mutations (deletion and duplication) over
    // representative programs: the parser must NEVER panic or hang on
    // malformed input — it must return diagnostics. This is the generalized
    // net for recovery bugs: any future recovery change that can crash or
    // desynchronize the parser fails here across hundreds of mutants.

    fn fuzz_sources() -> Vec<&'static str> {
        vec![
            "class B { pub v: i64; } class A { pub b: B; } \
             fn main() { let a = new A(new B(1)); a.b.v = 2; println(a.b.v); }",
            "enum Shape { Circle(i64), Empty, } \
             fn area(s: Shape) -> i64 { return match s { Shape::Circle(r) => r * r, Shape::Empty => 0, }; } \
             fn main() { println(area(Shape::Circle(3))); }",
            "interface Animal { fn speak(self) -> i64; } \
             class Dog implements Animal { pub fn speak(self) -> i64 { return 7; } } \
             fn main() { let d = new Dog(); println(d.speak()); }",
            "async fn compute(n: i64) -> i64 { await sleep(1); return n * n; } \
             async fn main() { let t = compute(6); println(t.join()); }",
            "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); } \
             fn main() { let mut t = 0; for i in 0..4 { t = t + i; } \
             println(apply(|x: i64| x * 2, t)); println(t > 2 ? \"big\" : \"small\"); }",
        ]
    }

    #[test]
    fn fuzz_01_token_deletion_never_panics() {
        for src in fuzz_sources() {
            let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
            for skip in 0..tokens.len().saturating_sub(1) {
                let mutated: Vec<Token> = tokens
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != skip)
                    .map(|(_, t)| t.clone())
                    .collect();
                // Must terminate and return; a panic fails the test.
                let (_, _errors) = Parser::new(mutated).parse();
            }
        }
    }

    #[test]
    fn fuzz_02_token_duplication_never_panics() {
        for src in fuzz_sources() {
            let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
            for dup in 0..tokens.len().saturating_sub(1) {
                let mut mutated: Vec<Token> = Vec::with_capacity(tokens.len() + 1);
                for (i, t) in tokens.iter().enumerate() {
                    mutated.push(t.clone());
                    if i == dup {
                        mutated.push(t.clone());
                    }
                }
                let (_, _errors) = Parser::new(mutated).parse();
            }
        }
    }

    #[test]
    fn fuzz_03_stmt_junk_never_cascades_to_missing_main() {
        // Statement-level junk inside main must produce errors WITHOUT losing
        // main itself (the qzxg cascade). Junk snippets are inserted as a
        // statement in an otherwise-valid main.
        let junks = [
            "1 = 2;",
            "g() = 5;",
            "match { }",
            "x +;",
            "= 3;",
            "let = 4;",
            "new ;",
        ];
        for junk in junks {
            let src = format!("fn main() {{ let x = 1; {junk} println(x); }}");
            let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("lex");
            let (program, errors) = Parser::new(tokens).parse();
            assert!(!errors.is_empty(), "junk `{junk}` must error");
            assert!(
                program
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::Function(f) if f.name == "main")),
                "junk `{junk}` swallowed main (recovery cascade)"
            );
        }
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

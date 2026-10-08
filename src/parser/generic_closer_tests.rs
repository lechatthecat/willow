//! Regression perspectives for willow-jz15.56. Inputs also reproduce the audit.
use super::*;
use crate::diagnostics::FileId;
use crate::lexer::Lexer;

thread_local! {
    pub(super) static TABLE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static SPLITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parser(source: &str) -> Parser {
    Parser::new(Lexer::new(source).tokenize().unwrap())
}

fn parse_ok(source: &str) -> Program {
    let (program, errors) = parser(source).parse();
    assert!(errors.is_empty(), "{source}: {errors:?}");
    program
}

#[test]
fn generic_closer_valid_contexts() {
    // P01-P12: adjacent/spaced closers, nesting, trailing commas, aliases,
    // callable returns, qualified names, optional sugar, and static fields.
    for (perspective, source) in [
        (1, "fn main() { let x: Option<i64>= Some(1); }"),
        (
            2,
            "fn main() { let x: Option<Option<i64>>= Some(Some(1)); }",
        ),
        (3, "fn main() { let x: Option<Option<i64> >= None; }"),
        (4, "fn main() { let x: Option<i64> = None; }"),
        (5, "fn main() { let x: Option<i64,>= None; }"),
        (6, "fn main() { let x: Option<>= None; }"),
        (7, "fn main() { let x: Array<i64>= []; }"),
        (8, "fn main() { let x: Option<fn() -> Option<i64>>= None; }"),
        (9, "fn main() { let x: pkg::Option<pkg::Value>= None; }"),
        (10, "fn main() { let x: Option<i64?>= None; }"),
        (11, "class C { static x: Option<i64>= Some(1); }"),
        (12, "class C { static x: Option<Option<i64>>= None; }"),
    ] {
        let program = parse_ok(source);
        assert_eq!(program.items.len(), 1, "perspective {perspective}");
    }
}

#[test]
fn generic_closer_parameter_and_constructor_boundaries() {
    // P13-P16: both type shapes in params and constructor args. '=' is not a
    // parameter default or constructor call opener; leave it to the caller.
    for ty in ["Option<i64>", "Option<Option<i64>>"] {
        let source = format!("value: {ty}= None");
        let mut p = parser(&source);
        let param = p.parse_param().unwrap();
        assert_eq!(&source[param.type_span.start..param.type_span.end], ty);
        assert!(p.check(TokenKind::Eq));
        assert_eq!(p.current_span().start, source.find('=').unwrap());
        parse_ok(&format!("fn f(value: {ty}) {{}}"));
        parse_ok(&format!("fn main() {{ let x = new Box<{ty}>(); }}"));
        let source = format!("new Box<{ty}>=()");
        let mut p = parser(&source);
        let error = p.parse_new().unwrap_err();
        assert!(p.check(TokenKind::Eq));
        assert_eq!(
            error.primary_span().unwrap().start,
            source.find('=').unwrap()
        );
    }
}

#[test]
fn generic_closer_spans_and_immutable_tokens() {
    // P17: exact virtual '>'/'=' spans including imported-file identity.
    let source = "Option<Option<i64>>= value";
    let mut tokens = Lexer::new(source).tokenize().unwrap();
    for token in &mut tokens {
        token.span.file_id = FileId(7);
    }
    let original = tokens.clone();
    let mut p = Parser::new(tokens);
    p.parse_type().unwrap();
    let eq = source.find('=').unwrap();
    assert_eq!(
        p.previous_span(),
        Span::in_file(FileId(7), eq - 1, eq, 1, eq)
    );
    assert_eq!(
        p.current_span(),
        Span::in_file(FileId(7), eq, eq + 1, 1, eq + 1)
    );
    assert_eq!(p.peek_kind_at(0), &TokenKind::Eq);
    assert!(matches!(p.peek_kind_at(1), TokenKind::Ident(name) if name == "value"));
    let eq_span = p.expect(TokenKind::Eq).unwrap();
    assert_eq!(p.previous_span(), eq_span);
    assert_eq!(p.tokens.len(), original.len());
    for (actual, original) in p.tokens.iter().zip(original) {
        assert_eq!(actual.kind, original.kind);
        assert_eq!(actual.span, original.span);
    }
}

#[test]
fn generic_closer_speculation_restores_cursor() {
    // P18-P19: rollback after a successful inner type and after a type error.
    for source in ["<Option<i64>= rhs", "<Pair<Option<i64>= rhs"] {
        let mut p = parser(source);
        assert!(
            p.try_parse_generic_static_call("Box".into(), p.current_span())
                .unwrap()
                .is_none()
        );
        assert_eq!(p.pos, 0);
        assert!(!p.pending_type_eq);
        assert!(p.last_span.is_none());
        assert!(p.type_uses.is_empty());
        while !p.check(TokenKind::GtEq) {
            assert!(!p.at_eof());
            p.advance();
        }
        assert_eq!(p.peek_kind(), &TokenKind::GtEq);
    }
    // Ordinary comparison speculation must not turn >= into assignment.
    let program = parse_ok("fn main() { let x = a < Option<Value>= b; }");
    let Item::Function(f) = &program.items[0] else {
        panic!()
    };
    let Stmt::Let(binding) = &f.body.stmts[0] else {
        panic!()
    };
    assert!(matches!(&binding.init, Expr::Binary(b) if b.op == BinOp::Ge));
}

#[test]
fn generic_closer_operator_and_cache_semantics() {
    // P20-P22: comparison, shift assignment, and cache use before/after split.
    let program = parse_ok(
        "fn main() { let a = c ? -1 : 2; let x: Option<i64>= None; let b = d ? -3 : 4; let ge = 8>=2; let mut n = 8; n>>=1; }",
    );
    let Item::Function(f) = &program.items[0] else {
        panic!()
    };
    for index in [0, 2] {
        let Stmt::Let(binding) = &f.body.stmts[index] else {
            panic!()
        };
        assert!(matches!(binding.init, Expr::Ternary(_)));
    }
    let Stmt::Let(binding) = &f.body.stmts[3] else {
        panic!()
    };
    assert!(matches!(&binding.init, Expr::Binary(b) if b.op == BinOp::Ge));
    let Stmt::Assign(assign) = &f.body.stmts[5] else {
        panic!()
    };
    assert!(matches!(&assign.value, Expr::Binary(b) if b.op == BinOp::Shr));
    parse_ok("fn f(x: Option<i64>) { let y: Option<i64>= Some(x? | 1); }");
    parse_ok("fn f() { let x: Option<i64>= None; let f = c ? |x: i64| x : |x: i64| x; }");
}

#[test]
fn generic_closer_invalid_syntax_still_rejected() {
    // P23-P26: missing closer, missing initializer, unsupported instance
    // initializer, and unsupported parameter default retain diagnostics.
    for source in [
        "fn main() { let x: Option<i64= None; }",
        "fn main() { let x: Option<i64>=; }",
        "class C { x: Option<i64>= None; }",
        "fn f(x: Option<i64>= None) {}",
    ] {
        let (_, errors) = parser(source).parse();
        assert!(!errors.is_empty(), "{source}");
    }
}

#[test]
fn generic_closer_work_scales_linearly() {
    // P27-P29: many splits interleaved with ambiguous '?', deep nesting,
    // and wide argument lists. Count work, not elapsed time.
    for shape in ["repeated", "deep", "wide"] {
        let mut samples = Vec::new();
        for n in [8, 16, 32, 64, 128] {
            let body = match shape {
                "repeated" => "let x: Option<i64>= None; let y = c ? -1 : 2;".repeat(n),
                "deep" => format!("let x: {}i64{}= None;", "Option<".repeat(n), ">".repeat(n)),
                _ => format!("let x: Tuple<{}>= None;", vec!["i64"; n].join(",")),
            };
            let source =
                format!("fn main() {{ let before = c ? -1 : 2; {body} let after = d ? -3 : 4; }}");
            let mut p = parser(&source);
            let token_count = p.tokens.len();
            let token_ptr = p.tokens.as_ptr();
            TABLE_VISITS.with(|v| v.set(0));
            SPLITS.with(|v| v.set(0));
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let (_, errors) = p.parse();
            assert!(errors.is_empty(), "{errors:?}");
            let visits = TABLE_VISITS.with(|v| v.get());
            let splits = SPLITS.with(|v| v.get());
            let reads = PARSER_TOKEN_READS.with(|v| v.get());
            assert_eq!(visits, 2 * token_count);
            assert_eq!(splits, if shape == "repeated" { n } else { 1 });
            assert_eq!(p.tokens.as_ptr(), token_ptr);
            assert_eq!(p.tokens.len(), token_count);
            samples.push((n, token_count, visits, splits, reads));
        }
        for pair in samples.windows(3) {
            assert_eq!(pair[2].4 - pair[1].4, 2 * (pair[1].4 - pair[0].4));
        }
        eprintln!("generic closer {shape} (n,tokens,table visits,splits,reads): {samples:?}");
    }
}

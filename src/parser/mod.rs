pub mod ast;
mod decl;
mod expr;
mod flat_syntax;
#[cfg(test)]
mod generic_closer_tests;
pub(crate) mod import_usage;
pub mod iter;
mod modifiers;
pub(crate) mod ownership;
mod pattern;
mod stmt;
pub(crate) mod tuples;
mod types;

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::lexer::token::{Token, TokenKind};
use ast::*;

pub struct Parser {
    default_import_usage: import_usage::DefaultImportUsage,
    tuple_arities: std::collections::HashSet<usize>,
    /// Payload nesting of the pattern being parsed; bounded by
    /// `pattern::MAX_PATTERN_DEPTH` so every pattern walk has a bounded depth.
    pattern_depth: usize,
    /// Parsing one operand of an `if let ... && ...` chain: `&&` ends the
    /// operand instead of combining it. Nested delimiters reset it.
    in_let_chain: bool,
    type_uses: Vec<TypeUse>,
    tokens: Vec<Token>,
    pos: usize,
    /// Consumed brace balance, so recovery never rescans a failed prefix.
    brace_depth: usize,
    /// Only direct expression statements in this match-arm block get tail-value help.
    match_arm_block_depth: Option<usize>,
    match_arm_conditional_depth: Option<usize>,
    /// The current immutable `GtEq` has contributed its `>` to a type.
    /// Only its `=` remains at the cursor; advancing clears this state.
    pending_type_eq: bool,
    last_span: Option<Span>,
    allow_object_literals: bool,
    /// Statement-level errors recovered inside blocks: the block
    /// keeps parsing after a bad statement, so one error no longer swallows the
    /// rest of the surrounding item (which used to cascade into a false
    /// `missing entry point` on `main`).
    recovered_errors: Vec<Diagnostic>,
    /// `ternary_colon_table` for `tokens`, built on the first ambiguous `?`.
    ternary_colon: std::cell::OnceCell<Vec<bool>>,
}

/// A token that ends a `?`-ternary colon scan at any nesting depth.
fn ends_ternary_scan(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Semicolon
            | TokenKind::Eq
            | TokenKind::LBrace
            | TokenKind::RBrace
            | TokenKind::FatArrow
            | TokenKind::Eof
    )
}

/// For each token position `i` (plus one past the end), whether scanning
/// forward from `i` at nesting depth 0 meets a `Colon` before the expression
/// ends: `;`, `=`, `{`, `}`, `=>` or EOF at any depth, or `,` or an unbalanced
/// `)`/`]` at depth 0. Inside a `(`/`[` group, colons and commas are skipped.
/// One forward pass matches each group with its closer, and one backward pass
/// fills the answers, so the table costs O(tokens).
fn ternary_colon_table(tokens: &[Token]) -> Vec<bool> {
    // For an opener, the position just past its closer, or `None` when a
    // boundary or EOF comes first.
    let mut after_group = vec![None; tokens.len()];
    let mut open = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        #[cfg(test)]
        generic_closer_tests::TABLE_VISITS.with(|n| n.set(n.get() + 1));
        match &token.kind {
            TokenKind::LParen | TokenKind::LBracket => open.push(i),
            TokenKind::RParen | TokenKind::RBracket => {
                if let Some(opener) = open.pop() {
                    after_group[opener] = Some(i + 1);
                }
            }
            kind if ends_ternary_scan(kind) => open.clear(),
            _ => {}
        }
    }
    let mut colon_ahead = vec![false; tokens.len() + 1];
    for i in (0..tokens.len()).rev() {
        #[cfg(test)]
        generic_closer_tests::TABLE_VISITS.with(|n| n.set(n.get() + 1));
        colon_ahead[i] = match &tokens[i].kind {
            TokenKind::Colon => true,
            TokenKind::Comma | TokenKind::RParen | TokenKind::RBracket => false,
            TokenKind::LParen | TokenKind::LBracket => {
                after_group[i].is_some_and(|next: usize| colon_ahead[next])
            }
            kind if ends_ternary_scan(kind) => false,
            _ => colon_ahead[i + 1],
        };
    }
    colon_ahead
}

#[willow_continuations::parser]
impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self {
            default_import_usage: Default::default(),
            tuple_arities: Default::default(),
            pattern_depth: 0,
            in_let_chain: false,
            type_uses: Vec::new(),
            tokens,
            pos: 0,
            brace_depth: 0,
            match_arm_block_depth: None,
            match_arm_conditional_depth: None,
            pending_type_eq: false,
            last_span: None,
            allow_object_literals: true,
            recovered_errors: Vec::new(),
            ternary_colon: std::cell::OnceCell::new(),
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
                Ok(decls) => imports.extend(decls),
                Err(e) => {
                    errors.push(e);
                    self.recover_to_next_item();
                }
            }
        }

        self.default_import_usage.imports(&imports);
        while !self.at_eof() {
            if matches!(self.peek_kind(), TokenKind::Import) {
                errors.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0105,
                        "imports must appear before all items",
                    )
                    .with_label(Label::primary(self.current_span(), "misplaced import"))
                    .with_help(
                        "move this import before the first item, after any module declaration",
                    ),
                );
                self.recover_to_next_item();
                continue;
            }
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
            if matches!(self.peek_kind(), TokenKind::Ident(name) if name == "extern")
                && matches!(self.peek_kind_at(1), TokenKind::Ident(name) if name == "rust")
            {
                errors.push(
                    self.err(
                        ErrorCode::E0102,
                        "rust_module_migration_required: `extern rust` declarations have been removed",
                    )
                    .with_help(
                        "move the Rust implementation to `src/<name>.rs`, replace this declaration with `import <name>;`, and call `<name>::function(...)`; remove `[rust] bridge` from project.toml",
                    ),
                );
                self.skip_removed_rust_declaration();
                continue;
            }
            self.default_import_usage.begin(false);
            let item_start = self.pos;
            match self.parse_item() {
                Ok(item) => items.extend(item),
                Err(e) => {
                    errors.push(e);
                    self.recover_after_item_error(item_start);
                }
            }
        }

        errors.append(&mut self.recovered_errors);
        errors.sort_by_key(|error| {
            error
                .primary_span()
                .map(|span| (span.file_id.0, span.start))
        });
        let mut arities: Vec<_> = self.tuple_arities.iter().copied().collect();
        arities.sort_unstable();
        items.extend(arities.into_iter().map(tuples::declaration));
        let mut program = Program {
            instantiation_types: Vec::new(),
            type_uses: std::mem::take(&mut self.type_uses),
            module,
            imports,
            items,
        };
        crate::semantic::callable_slots::apply_program(&mut program);
        (program, errors)
    }

    fn peek_kind(&self) -> &TokenKind {
        #[cfg(test)]
        PARSER_TOKEN_READS.with(|count| count.set(count.get() + 1));
        self.peek_kind_at(0)
    }

    /// The token `offset` places ahead, saturating at the final token — which
    /// is always EOF, so a lookahead past the end reads as EOF rather than
    /// panicking.
    fn peek_kind_at(&self, offset: usize) -> &TokenKind {
        if offset == 0 && self.pending_type_eq {
            return &TokenKind::Eq;
        }
        let idx = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[idx].kind
    }

    fn current_span(&self) -> Span {
        let mut span = self.tokens[self.pos].span;
        if self.pending_type_eq {
            span.start += 1;
            span.col += 1;
        }
        span
    }

    fn previous_span(&self) -> Span {
        self.last_span.unwrap_or_else(|| self.current_span())
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            match self.tokens[self.pos].kind {
                TokenKind::LBrace => self.brace_depth += 1,
                TokenKind::RBrace => self.brace_depth = self.brace_depth.saturating_sub(1),
                _ => {}
            }
            self.last_span = Some(self.current_span());
            self.pending_type_eq = false;
            self.pos += 1;
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.tokens[self.pos].kind, TokenKind::Eof)
    }

    fn check(&self, kind: TokenKind) -> bool {
        *self.peek_kind_at(0) == kind
    }

    fn at_type_gt(&self) -> bool {
        matches!(self.peek_kind_at(0), TokenKind::Gt | TokenKind::GtEq)
    }

    /// Consume a type's closing `>` without inserting or mutating tokens.
    /// Ternary queries start strictly ahead of the current cursor, hence ahead
    /// of the virtual `=`. Their immutable-token suffix and cached answer stay
    /// valid. Speculative type parsing restores this cursor state on rollback.
    fn expect_type_gt(&mut self) -> Result<Span, Diagnostic> {
        if self.check(TokenKind::GtEq) {
            #[cfg(test)]
            generic_closer_tests::SPLITS.with(|n| n.set(n.get() + 1));
            let mut span = self.current_span();
            span.end = span.start + 1;
            self.pending_type_eq = true;
            self.last_span = Some(span);
            Ok(span)
        } else {
            self.expect(TokenKind::Gt)
        }
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
        //      `c ? || 1 : || 2`; `Pipe`: `r? | m` vs `c ? |x| x : f`) →
        //      resolved by scanning ahead for the ternary's mandatory `:` at
        //      the same nesting level.
        //   3. Everything else → try-propagate.
        match next {
            Some(TokenKind::Minus | TokenKind::Or | TokenKind::Pipe) => {
                !self.ternary_colon_ahead(next_pos + 1)
            }
            _ => !matches!(
                next,
                Some(
                    TokenKind::CharLiteral(_)
                        | TokenKind::Integer(_)
                        | TokenKind::Float(_)
                        | TokenKind::True
                        | TokenKind::False
                        | TokenKind::Ident(_)
                        | TokenKind::StringLiteral(_)
                        | TokenKind::LParen
                        | TokenKind::LBracket
                        | TokenKind::Bang
                        | TokenKind::Nil
                        | TokenKind::New
                        | TokenKind::SelfKw
                        | TokenKind::Match
                        | TokenKind::Await
                        | TokenKind::Select
                        | TokenKind::Print
                        | TokenKind::Println
                        | TokenKind::I8
                        | TokenKind::I16
                        | TokenKind::I32
                        | TokenKind::U16
                        | TokenKind::U32
                        | TokenKind::U64
                        | TokenKind::Char
                        | TokenKind::U8
                        | TokenKind::I64
                        | TokenKind::F64
                )
            ),
        }
    }

    /// Disambiguate an ambiguous token after `?` by looking for the ternary's
    /// mandatory `:` before the expression can end: a `Colon` at nesting
    /// level 0 from `from` means the `?` opened a ternary. Statement/argument
    /// boundaries (`;`, `=`, `{`, `}`, `,`, `=>`, unbalanced `)`/`]`, EOF) end
    /// the scan as try-propagate. The answers for every position come from one
    /// O(tokens) table built on first use, so a statement with many ambiguous
    /// `?` (`a? | b? | c?`) is not rescanned per `?`.
    fn ternary_colon_ahead(&self, from: usize) -> bool {
        self.ternary_colon
            .get_or_init(|| ternary_colon_table(&self.tokens))
            .get(from)
            .copied()
            .unwrap_or(false)
    }

    /// The kind of the token after the current one when the two touch, so a
    /// two-token operator such as `<<` is never assembled from `< <`.
    fn adjacent_next_kind(&self) -> Option<&TokenKind> {
        let current = &self.tokens[self.pos];
        let next = self.tokens.get(self.pos + 1)?;
        (current.span.end == next.span.start).then_some(&next.kind)
    }

    /// `<<` or `>>` at the current position.
    fn shift_op(&self) -> Option<BinOp> {
        match (self.peek_kind(), self.adjacent_next_kind()?) {
            (TokenKind::Lt, TokenKind::Lt) => Some(BinOp::Shl),
            (TokenKind::Gt, TokenKind::Gt) => Some(BinOp::Shr),
            _ => None,
        }
    }

    /// `<<=` or `>>=` (a `<`/`>` touching `<=`/`>=`) at the current position.
    fn shift_assign_op(&self) -> Option<BinOp> {
        match (self.peek_kind(), self.adjacent_next_kind()?) {
            (TokenKind::Lt, TokenKind::LtEq) => Some(BinOp::Shl),
            (TokenKind::Gt, TokenKind::GtEq) => Some(BinOp::Shr),
            _ => None,
        }
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
        } else if kind == TokenKind::Semicolon
            && let Some(after) = self.end_of_previous_line()
        {
            // The `;` belongs after the last token of the previous line, not
            // before the first token of the next one.
            let (code, msg) = token_expect_message(&kind);
            Err(Diagnostic::new(Severity::Error, code, msg)
                .with_label(Label::primary(after, "add `;` here"))
                .with_help(format!("end the statement on line {} with `;`", after.line))
                .with_fix(FixSuggestion::insertion(after, ";", "add `;`")))
        } else {
            let (code, msg) = token_expect_message(&kind);
            Err(self.err(code, msg))
        }
    }

    /// The empty span just after the previous token, when the current token
    /// starts a later line.
    fn end_of_previous_line(&self) -> Option<Span> {
        let previous = self.last_span?;
        let current = self.current_span();
        if current.line <= previous.line || previous.end > current.start {
            return None;
        }
        // Only a string literal can span lines; its end column is unknown
        // here, so keep the default position after one.
        if let TokenKind::StringLiteral(text) = &self.tokens[self.pos.checked_sub(1)?].kind
            && text.contains('\n')
        {
            return None;
        }
        let mut after = previous;
        after.col = previous.col + (previous.end - previous.start);
        after.start = previous.end;
        Some(after)
    }

    fn expect_ident(&mut self) -> Result<String, Diagnostic> {
        // `new` is a prefix keyword for object construction (`new Class(...)`),
        // matched before this point in expression position. As a plain
        // identifier — a method/function name like `Channel::new` or
        // `static fn new` — it is still accepted here.
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

    // Retain the surrounding declaration after a reserved builtin method name.
    // The recorded diagnostic still rejects the program, but parsing its signature
    // here prevents the next receiver from being parsed as a free-function parameter.
    fn expect_method_name(&mut self) -> Result<String, Diagnostic> {
        if matches!(self.peek_kind(), TokenKind::Print | TokenKind::Println) {
            let error = self.err(ErrorCode::E0102, "expected identifier");
            self.recovered_errors.push(error);
            let name = self.peek_kind().keyword_name().unwrap().to_string();
            self.advance();
            Ok(name)
        } else {
            self.expect_ident()
        }
    }

    fn err(&self, code: ErrorCode, msg: impl Into<String>) -> Diagnostic {
        let msg = msg.into();
        let span = self.current_span();
        let mut diagnostic = Diagnostic::new(Severity::Error, code, msg.clone())
            .with_label(Label::primary(span, msg));
        if code == ErrorCode::E0102
            && let Some(keyword) = self.peek_kind().keyword_name()
        {
            diagnostic
                .message
                .push_str(&format!("; '{keyword}' is a reserved keyword"));
            if matches!(self.peek_kind(), TokenKind::Print | TokenKind::Println) {
                diagnostic.message.push_str(" (builtin function name)");
            }
            diagnostic.helps.push(format!(
                "if you intended '{keyword}' as a name, rename it to a non-keyword identifier"
            ));
        }
        diagnostic
    }

    /// Like [`Self::err`], but labels a span the parser already has instead of
    /// the current token (which has usually moved past the offending syntax).
    fn err_at(&self, code: ErrorCode, msg: impl Into<String>, span: Span) -> Diagnostic {
        let msg = msg.into();
        Diagnostic::new(Severity::Error, code, msg.clone()).with_label(Label::primary(span, msg))
    }

    /// Discard the rejected declaration as one unit, including nested record
    /// bodies. Every token is consumed once; malformed or truncated blocks stop
    /// at EOF, and declarations inside them never become Willow functions.
    fn skip_removed_rust_declaration(&mut self) {
        self.advance(); // extern
        self.advance(); // rust
        #[cfg(test)]
        rust_migration_tests::SKIPPED_TOKENS.set(rust_migration_tests::SKIPPED_TOKENS.get() + 2);
        while !self.at_eof() && !self.check(TokenKind::LBrace) {
            if self.at_item_start() {
                return;
            }
            #[cfg(test)]
            rust_migration_tests::SKIPPED_TOKENS
                .set(rust_migration_tests::SKIPPED_TOKENS.get() + 1);
            if self.eat(TokenKind::Semicolon) {
                return;
            }
            self.advance();
        }
        if self.at_eof() {
            return;
        }
        let outer_depth = self.brace_depth;
        self.advance();
        #[cfg(test)]
        rust_migration_tests::SKIPPED_TOKENS.set(rust_migration_tests::SKIPPED_TOKENS.get() + 1);
        while !self.at_eof() && self.brace_depth > outer_depth {
            self.advance();
            #[cfg(test)]
            rust_migration_tests::SKIPPED_TOKENS
                .set(rust_migration_tests::SKIPPED_TOKENS.get() + 1);
        }
    }

    fn recover_to_next_item(&mut self) {
        if !self.at_eof() {
            self.advance();
        }

        while !self.at_eof() && !self.at_item_start() {
            self.advance();
        }
    }

    /// Recovery after a failed top-level item that began at token `item_start`.
    /// When the item consumed tokens and failed at module level (outside any
    /// brace) on the start of the next item — `const A: i64 = 1 const B ...` —
    /// that item is kept instead of skipped. Otherwise, e.g.
    /// on an error inside a class body or on the item's own first token, the
    /// first token is skipped as before so recovery always makes progress.
    fn recover_after_item_error(&mut self, item_start: usize) {
        if self.pos > item_start && self.brace_depth == 0 && self.at_item_start() {
            return;
        }
        self.recover_to_next_item();
    }

    fn at_item_start(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(name) if name == "open")
            || matches!(
                self.peek_kind(),
                TokenKind::At
                    | TokenKind::Fn
                    | TokenKind::Async
                    | TokenKind::Class
                    | TokenKind::Const
                    | TokenKind::Interface
                    | TokenKind::Pub
                    | TokenKind::Prot
                    | TokenKind::Import
                    | TokenKind::Enum
            )
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
mod tests;

#[cfg(test)]
thread_local! {
    static PARSER_TOKEN_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod keyword_tests;

#[cfg(test)]
mod unsupported_postfix_tests;

#[cfg(test)]
mod generic_interface_extends_tests;

#[cfg(test)]
mod if_expression_tests;

#[cfg(test)]
mod match_arm_diagnostic_tests;
#[cfg(test)]
mod or_pattern_diagnostic_tests;

#[cfg(test)]
mod unit_diagnostic_tests;

#[cfg(test)]
mod misplaced_import_tests;

#[cfg(test)]
mod rust_migration_tests {
    use super::*;
    use crate::lexer::Lexer;

    thread_local! {
        pub(super) static SKIPPED_TOKENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn parse(source: &str) -> (Program, Vec<Diagnostic>) {
        Parser::new(Lexer::new(source).tokenize().unwrap()).parse()
    }

    #[test]
    fn removed_rust_declarations_explain_module_migration() {
        for declaration in [
            "extern rust { fn value() -> i64; }",
            "extern rust native { fn value() -> i64; }",
            "extern rust { class Value { pub field: i64; } fn value() -> Value; }",
            "extern rust { nonblocking(\"obsolete\") fn value(); }",
            "extern rust { fn broken( ; }",
        ] {
            let (program, errors) = parse(&format!("{declaration} fn main() {{}}"));
            assert_eq!(errors.len(), 1, "{declaration}: {errors:?}");
            let error = &errors[0];
            assert_eq!(error.code, ErrorCode::E0102);
            assert!(error.message.contains("rust_module_migration_required"));
            let help = error.helps.join(" ");
            assert!(help.contains("src/<name>.rs") && help.contains("import <name>;"));
            assert!(help.contains("[rust] bridge"));
            assert!(
                program
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Function(f) if f.name == "main"))
            );
            assert!(
                !program
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Function(f) if f.rust_bridge.is_some()))
            );
        }
    }

    #[test]
    fn truncated_rust_declarations_stop_without_cascading() {
        for source in [
            "extern rust",
            "extern rust native",
            "extern rust {",
            "extern rust { fn value() -> i64;",
            "extern rust { class Record { pub field: i64; }",
        ] {
            let (_, errors) = parse(source);
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            assert!(errors[0].message.contains("rust_module_migration_required"));
        }
    }

    #[test]
    fn removed_rust_recovery_scales_with_tokens() {
        for count in [1, 16, 256] {
            let source = format!(
                "extern rust {{ {} }} fn main() {{}}",
                "class Record { pub field: i64; } ".repeat(count)
            );
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let token_count = tokens.len();
            SKIPPED_TOKENS.set(0);
            let (program, errors) = Parser::new(tokens).parse();
            assert_eq!(errors.len(), 1);
            // Six tokens belong to `fn main() {}`, and one is EOF.
            assert_eq!(SKIPPED_TOKENS.get(), token_count - 7);
            eprintln!(
                "removed records={count} skipped_tokens={}",
                SKIPPED_TOKENS.get()
            );
            assert!(
                program
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Function(f) if f.name == "main"))
            );
        }
    }

    #[test]
    fn native_module_imports_and_qualified_calls_remain_valid() {
        let (program, errors) = parse(
            "import native; fn main() { let value = native::answer; println(native::answer()); }",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(program.imports.len(), 1);
        assert!(
            program
                .items
                .iter()
                .any(|item| matches!(item, Item::Function(f) if f.name == "main"))
        );
    }
}

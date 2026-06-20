pub mod token;

use crate::diagnostics::{Diagnostic, ErrorCode, FileId, Label, Severity, Span};
use token::{Token, TokenKind};

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
    file_id: FileId,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self::with_file_id(src, FileId::ENTRY)
    }

    pub fn with_file_id(src: &'a str, file_id: FileId) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            file_id,
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, Vec<Diagnostic>> {
        let mut tokens = Vec::new();
        let mut errors = Vec::new();

        loop {
            if let Err(diag) = self.skip_whitespace_and_comments() {
                // An unterminated block comment consumes to end of input; record
                // the error and let the next iteration emit `Eof` and finish.
                errors.push(diag);
            }
            if self.pos >= self.bytes.len() {
                tokens.push(Token::new(TokenKind::Eof, self.span(self.pos, self.pos)));
                break;
            }

            let start = self.pos;
            let line = self.line;
            let col = self.col;

            match self.next_token() {
                Ok(Some(kind)) => {
                    let span = self.span_at(start, self.pos, line, col);
                    tokens.push(Token::new(kind, span));
                }
                Ok(None) => {}
                Err(diag) => {
                    errors.push(diag);
                }
            }
        }

        if errors.is_empty() {
            Ok(tokens)
        } else {
            Err(errors)
        }
    }

    fn next_token(&mut self) -> Result<Option<TokenKind>, Diagnostic> {
        let b = self.bytes[self.pos];
        let kind = match b {
            b'+' => {
                self.advance();
                TokenKind::Plus
            }
            b'-' => {
                self.advance();
                if self.peek() == Some(b'>') {
                    self.advance();
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            b'*' => {
                self.advance();
                TokenKind::Star
            }
            b'/' => {
                self.advance();
                TokenKind::Slash
            }
            b'%' => {
                self.advance();
                TokenKind::Percent
            }
            b'!' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::BangEq
                } else {
                    TokenKind::Bang
                }
            }
            b'=' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::EqEq
                } else if self.peek() == Some(b'>') {
                    self.advance();
                    TokenKind::FatArrow
                } else {
                    TokenKind::Eq
                }
            }
            b'<' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            b'>' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            b'&' => {
                self.advance();
                if self.peek() == Some(b'&') {
                    self.advance();
                    TokenKind::And
                } else {
                    TokenKind::Ampersand
                }
            }
            b'|' => {
                self.advance();
                if self.peek() == Some(b'|') {
                    self.advance();
                    TokenKind::Or
                } else {
                    TokenKind::Pipe
                }
            }
            b'"' => return self.lex_string().map(Some),
            b';' => {
                self.advance();
                TokenKind::Semicolon
            }
            b':' => {
                self.advance();
                if self.peek() == Some(b':') {
                    self.advance();
                    TokenKind::ColonColon
                } else {
                    TokenKind::Colon
                }
            }
            b',' => {
                self.advance();
                TokenKind::Comma
            }
            b'.' => {
                self.advance();
                if self.peek() == Some(b'.') {
                    self.advance();
                    TokenKind::DotDot
                } else {
                    TokenKind::Dot
                }
            }
            b'{' => {
                self.advance();
                TokenKind::LBrace
            }
            b'}' => {
                self.advance();
                TokenKind::RBrace
            }
            b'(' => {
                self.advance();
                TokenKind::LParen
            }
            b')' => {
                self.advance();
                TokenKind::RParen
            }
            b'[' => {
                self.advance();
                TokenKind::LBracket
            }
            b']' => {
                self.advance();
                TokenKind::RBracket
            }
            b'?' => {
                self.advance();
                TokenKind::Question
            }
            b'0'..=b'9' => return self.lex_number().map(Some),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident_or_keyword(),
            c => {
                let err = self.err_invalid_char(c);
                self.advance();
                return Err(err);
            }
        };
        Ok(Some(kind))
    }

    fn err_invalid_char(&self, c: u8) -> Diagnostic {
        self.err_invalid_char_at(c, self.pos, self.line, self.col)
    }

    fn err_invalid_char_at(&self, c: u8, start: usize, line: usize, col: usize) -> Diagnostic {
        let span = self.span_at(start, start + 1, line, col);
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0050,
            format!("invalid character `{}`", c as char),
        )
        .with_label(Label::primary(span, "invalid character"))
    }

    fn err_unterminated_string_at(&mut self, start: usize, line: usize, col: usize) -> Diagnostic {
        // consume the opening quote and scan to end of line
        if self.pos == start {
            self.advance();
        }
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.advance();
        }
        let span = self.span_at(start, start + 1, line, col);
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0051,
            "unterminated string literal",
        )
        .with_label(Label::primary(span, "string starts here but never ends"))
    }

    fn lex_string(&mut self) -> Result<TokenKind, Diagnostic> {
        let start = self.pos;
        let line = self.line;
        let col = self.col;
        self.advance(); // opening quote

        let mut value = String::new();
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'"' => {
                    self.advance();
                    return Ok(TokenKind::StringLiteral(value));
                }
                b'\n' => return Err(self.err_unterminated_string_at(start, line, col)),
                b'\\' => {
                    self.advance();
                    if self.pos >= self.bytes.len() || self.bytes[self.pos] == b'\n' {
                        return Err(self.err_unterminated_string_at(start, line, col));
                    }
                    let escaped = match self.advance_char().unwrap_or('\0') {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '"' => '"',
                        '\\' => '\\',
                        '0' => '\0',
                        other => other,
                    };
                    value.push(escaped);
                }
                _ => {
                    if let Some(ch) = self.advance_char() {
                        value.push(ch);
                    }
                }
            }
        }

        Err(self.err_unterminated_string_at(start, line, col))
    }

    fn lex_number(&mut self) -> Result<TokenKind, Diagnostic> {
        let start = self.pos;
        let line = self.line;
        let col = self.col;
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
            self.advance();
        }
        // check for decimal point
        if self.pos + 1 < self.bytes.len()
            && self.bytes[self.pos] == b'.'
            && self.bytes[self.pos + 1].is_ascii_digit()
        {
            self.advance(); // consume '.'
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
                self.advance();
            }
            let s = &self.src[start..self.pos];
            let f: f64 = s.parse().unwrap_or(0.0);
            return Ok(TokenKind::Float(f));
        }
        let s = &self.src[start..self.pos];
        // A digit sequence that overflows `i64` was previously silently parsed
        // as 0, miscompiling the program. Report it as a source-aware error.
        match s.parse::<i64>() {
            Ok(n) => Ok(TokenKind::Integer(n)),
            Err(_) => Err(self.err_integer_out_of_range(s, start, line, col)),
        }
    }

    fn err_integer_out_of_range(
        &self,
        lit: &str,
        start: usize,
        line: usize,
        col: usize,
    ) -> Diagnostic {
        let span = self.span_at(start, self.pos, line, col);
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0052,
            format!("integer literal `{lit}` out of range for `i64`"),
        )
        .with_label(Label::primary(span, "value does not fit in `i64`"))
        .with_help(format!(
            "`i64` values range from {} to {}",
            i64::MIN,
            i64::MAX
        ))
    }

    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let start = self.pos;
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_alphanumeric() || self.bytes[self.pos] == b'_')
        {
            self.advance();
        }
        let word = &self.src[start..self.pos];
        match word {
            "fn" => TokenKind::Fn,
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "return" => TokenKind::Return,
            "print" => TokenKind::Print,
            "println" => TokenKind::Println,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "nil" => TokenKind::Nil,
            "class" => TokenKind::Class,
            "pub" => TokenKind::Pub,
            "prot" => TokenKind::Prot,
            "open" => TokenKind::Open,
            "override" => TokenKind::Override,
            "static" => TokenKind::Static,
            "new" => TokenKind::New,
            "extends" => TokenKind::Extends,
            "interface" => TokenKind::Interface,
            "implements" => TokenKind::Implements,
            "self" => TokenKind::SelfKw,
            "import" => TokenKind::Import,
            "module" => TokenKind::Module,
            "as" => TokenKind::As,
            "async" => TokenKind::Async,
            "await" => TokenKind::Await,
            "select" => TokenKind::Select,
            "match" => TokenKind::Match,
            "enum" => TokenKind::Enum,
            "i64" => TokenKind::I64,
            "f64" => TokenKind::F64,
            "bool" => TokenKind::Bool,
            _ => TokenKind::Ident(word.to_string()),
        }
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), Diagnostic> {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                if self.bytes[self.pos] == b'\n' {
                    self.line += 1;
                    self.col = 1;
                } else {
                    self.col += 1;
                }
                self.pos += 1;
            }
            // line comments
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'/'
            {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'*'
            {
                // block comments (Rust-style, may nest)
                self.skip_block_comment()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Skip a `/* ... */` block comment. Block comments nest, so `/* /* */ */`
    /// is a single comment. Returns an `unterminated block comment` error if the
    /// input ends before the outermost comment is closed. Assumes the cursor is
    /// positioned at the opening `/*`.
    fn skip_block_comment(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        let line = self.line;
        let col = self.col;
        self.advance(); // '/'
        self.advance(); // '*'
        let mut depth = 1usize;
        while self.pos < self.bytes.len() {
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'*'
            {
                self.advance();
                self.advance();
                depth += 1;
            } else if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'*'
                && self.bytes[self.pos + 1] == b'/'
            {
                self.advance();
                self.advance();
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            } else {
                self.advance();
            }
        }
        let span = self.span_at(start, start + 2, line, col);
        Err(Diagnostic::new(
            Severity::Error,
            ErrorCode::E0053,
            "unterminated block comment",
        )
        .with_label(Label::primary(
            span,
            "block comment starts here but is never closed",
        )))
    }

    fn advance(&mut self) {
        if self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            self.pos += 1;
        }
    }

    fn advance_char(&mut self) -> Option<char> {
        let ch = self.src.get(self.pos..)?.chars().next()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn span(&self, start: usize, end: usize) -> Span {
        self.span_at(start, end, self.line, self.col)
    }

    fn span_at(&self, start: usize, end: usize, line: usize, col: usize) -> Span {
        Span::in_file(self.file_id, start, end, line, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use token::TokenKind;

    // Tokenize and return the kinds (without the trailing Eof) on success.
    fn kinds(src: &str) -> Result<Vec<TokenKind>, Vec<Diagnostic>> {
        Lexer::new(src).tokenize().map(|toks| {
            toks.into_iter()
                .map(|t| t.kind)
                .filter(|k| *k != TokenKind::Eof)
                .collect()
        })
    }

    fn first_error(src: &str) -> Diagnostic {
        Lexer::new(src)
            .tokenize()
            .err()
            .and_then(|mut e| e.drain(..).next())
            .expect("expected a lexer error")
    }

    // ── Block comments: valid ────────────────────────────────────────────────

    // Perspective 1: a simple block comment between tokens is skipped.
    #[test]
    fn block_comment_between_tokens() {
        assert_eq!(
            kinds("1 /* c */ + 2").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::Plus,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 2: a block comment spanning multiple lines is skipped.
    #[test]
    fn block_comment_multiline() {
        assert_eq!(
            kinds("1 /* line one\n line two */ 2").unwrap(),
            vec![TokenKind::Integer(1), TokenKind::Integer(2)]
        );
    }

    // Perspective 3: an empty block comment `/**/` is valid.
    #[test]
    fn block_comment_empty() {
        assert_eq!(kinds("/**/ 5").unwrap(), vec![TokenKind::Integer(5)]);
    }

    // Perspective 4: block comments nest (`/* /* */ */` is one comment).
    #[test]
    fn block_comment_nested() {
        assert_eq!(
            kinds("1 /* a /* b */ c */ 2").unwrap(),
            vec![TokenKind::Integer(1), TokenKind::Integer(2)]
        );
    }

    // Perspective 5: deeply nested block comments (3 levels) are one comment.
    #[test]
    fn block_comment_deeply_nested() {
        assert_eq!(
            kinds("/* a /* b /* c */ d */ e */ 9").unwrap(),
            vec![TokenKind::Integer(9)]
        );
    }

    // Perspective 6: a `//` inside a block comment does not change nesting.
    #[test]
    fn block_comment_contains_line_marker() {
        assert_eq!(
            kinds("/* not a // line comment */ 3").unwrap(),
            vec![TokenKind::Integer(3)]
        );
    }

    // Perspective 7: code-like text and keywords inside a block comment are not
    // tokenized.
    #[test]
    fn block_comment_contains_codeish_text() {
        assert_eq!(
            kinds("/* fn main() { let x = 1; } */ 4").unwrap(),
            vec![TokenKind::Integer(4)]
        );
    }

    // Perspective 8: lone `*` / `/` inside a block comment do not close it.
    #[test]
    fn block_comment_contains_loose_star_slash() {
        assert_eq!(
            kinds("/* a * b / c */ 6").unwrap(),
            vec![TokenKind::Integer(6)]
        );
    }

    // Perspective 9: block comments adjacent to tokens with no spaces.
    #[test]
    fn block_comment_adjacent_no_spaces() {
        assert_eq!(
            kinds("1/*x*/+/*y*/2").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::Plus,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 10: `//` line comments still work (regression).
    #[test]
    fn line_comment_still_skipped() {
        assert_eq!(
            kinds("1 // trailing\n+ 2").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::Plus,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 11: a multi-line block comment keeps line numbers correct for
    // a later token's span.
    #[test]
    fn block_comment_preserves_line_numbers() {
        let toks = Lexer::new("/* one\n two\n three */\nx").tokenize().unwrap();
        let ident = toks
            .iter()
            .find(|t| matches!(t.kind, TokenKind::Ident(_)))
            .unwrap();
        assert_eq!(
            ident.span.line, 4,
            "token after 3-line comment is on line 4"
        );
    }

    // ── Block comments: invalid ──────────────────────────────────────────────

    // Perspective 12: an unterminated block comment is E0053.
    #[test]
    fn block_comment_unterminated() {
        let d = first_error("1 /* never closed");
        assert_eq!(d.code, ErrorCode::E0053);
        assert!(d.message.contains("unterminated block comment"));
    }

    // Perspective 13: a nested comment whose inner closes but outer does not is
    // still unterminated.
    #[test]
    fn block_comment_unterminated_nested() {
        let d = first_error("/* outer /* inner */ still open");
        assert_eq!(d.code, ErrorCode::E0053);
    }

    // Perspective 14: `/*/` is not self-closing — it is unterminated.
    #[test]
    fn block_comment_slash_star_slash_is_unterminated() {
        let d = first_error("/*/");
        assert_eq!(d.code, ErrorCode::E0053);
    }

    // ── Integer literals: valid ──────────────────────────────────────────────

    // Perspective 15: ordinary integers are unaffected.
    #[test]
    fn integer_ordinary() {
        assert_eq!(
            kinds("0 7 1000").unwrap(),
            vec![
                TokenKind::Integer(0),
                TokenKind::Integer(7),
                TokenKind::Integer(1000),
            ]
        );
    }

    // Perspective 16: `i64::MAX` parses exactly.
    #[test]
    fn integer_i64_max_ok() {
        assert_eq!(
            kinds("9223372036854775807").unwrap(),
            vec![TokenKind::Integer(i64::MAX)]
        );
    }

    // Perspective 17: a large but in-range integer parses.
    #[test]
    fn integer_large_in_range() {
        assert_eq!(
            kinds("1000000000000").unwrap(),
            vec![TokenKind::Integer(1_000_000_000_000)]
        );
    }

    // ── Integer literals: invalid ────────────────────────────────────────────

    // Perspective 18: an obviously-too-big integer is E0052 (was silently 0).
    #[test]
    fn integer_overflow_is_error() {
        let d = first_error("99999999999999999999");
        assert_eq!(d.code, ErrorCode::E0052);
        assert!(d.message.contains("out of range"));
    }

    // Perspective 19: `i64::MAX + 1` is rejected (boundary).
    #[test]
    fn integer_one_past_max_is_error() {
        let d = first_error("9223372036854775808");
        assert_eq!(d.code, ErrorCode::E0052);
    }

    // Perspective 20: a very long digit run is rejected (no panic / no 0).
    #[test]
    fn integer_very_long_is_error() {
        let d = first_error("123456789012345678901234567890");
        assert_eq!(d.code, ErrorCode::E0052);
    }

    // Perspective 21: the overflow error help mentions the i64 range.
    #[test]
    fn integer_overflow_help_mentions_range() {
        let d = first_error("99999999999999999999");
        let has_label = d.labels.iter().any(|l| l.message.contains("does not fit"));
        assert!(has_label || !d.helps.is_empty());
    }

    // ── Interaction / regression ─────────────────────────────────────────────

    // Perspective 22: float literals are unaffected by the integer range check.
    #[test]
    fn float_literal_unaffected() {
        assert_eq!(kinds("3.5").unwrap(), vec![TokenKind::Float(3.5)]);
    }

    // Perspective 23: an integer immediately followed by `.method`-style dot is
    // still an integer then a dot (range check does not consume the dot).
    #[test]
    fn integer_then_dotdot_range() {
        assert_eq!(
            kinds("1..3").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::DotDot,
                TokenKind::Integer(3),
            ]
        );
    }

    // Perspective 24: block comment and a valid max integer combine cleanly.
    #[test]
    fn block_comment_then_max_integer() {
        assert_eq!(
            kinds("/* c */ 9223372036854775807").unwrap(),
            vec![TokenKind::Integer(i64::MAX)]
        );
    }
}

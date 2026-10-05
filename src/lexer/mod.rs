pub mod token;

use crate::diagnostics::{Diagnostic, ErrorCode, FileId, Label, Severity, Span};
use crate::errors::LexError;
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

    pub fn tokenize(&mut self) -> Result<Vec<Token>, LexError> {
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
            Err(LexError::new(errors))
        }
    }

    fn next_token(&mut self) -> Result<Option<TokenKind>, Diagnostic> {
        let b = self.bytes[self.pos];
        let kind = match b {
            b'+' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::PlusEq
                } else {
                    TokenKind::Plus
                }
            }
            b'-' => {
                self.advance();
                if self.peek() == Some(b'>') {
                    self.advance();
                    TokenKind::Arrow
                } else if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::MinusEq
                } else {
                    TokenKind::Minus
                }
            }
            b'*' => {
                self.advance();
                // Maximal munch: `**` is exponentiation, so `***` is `**` then
                // `*`. `**=` remains `**` followed by `=`.
                if self.peek() == Some(b'*') {
                    self.advance();
                    TokenKind::StarStar
                } else if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::StarEq
                } else {
                    TokenKind::Star
                }
            }
            b'/' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::SlashEq
                } else {
                    TokenKind::Slash
                }
            }
            b'%' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::PercentEq
                } else {
                    TokenKind::Percent
                }
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
                } else if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::AmpersandEq
                } else {
                    TokenKind::Ampersand
                }
            }
            b'|' => {
                self.advance();
                if self.peek() == Some(b'|') {
                    self.advance();
                    TokenKind::Or
                } else if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::PipeEq
                } else {
                    TokenKind::Pipe
                }
            }
            b'^' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    TokenKind::CaretEq
                } else {
                    TokenKind::Caret
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
                b'\n' => {
                    self.advance();
                    value.push('\n');
                }
                b'\\' => {
                    self.advance();
                    if self.pos >= self.bytes.len() || self.bytes[self.pos] == b'\n' {
                        return Err(self.err_unterminated_string_at(start, line, col));
                    }
                    let escaped = match self.advance_char().unwrap_or('\0') {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'e' => '\x1b',
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

    /// Lexes a numeric literal. Rules (willow-jz15.55):
    ///
    /// - `0x` (hex), `0o` (octal) and `0b` (binary) prefixes introduce an
    ///   integer literal; hex digits may be either case, the prefix must be
    ///   lowercase.
    /// - `_` is a digit separator. It may appear only between two digits, or
    ///   directly after a radix prefix (`0x_FF`); a leading, trailing or
    ///   doubled `_` is rejected. Decimal integers and both halves of a float
    ///   accept separators too.
    /// - An integer literal must fit `i64` whatever its radix, so
    ///   `0xFFFF_FFFF_FFFF_FFFF` is out of range rather than `-1` (as in Rust).
    fn lex_number(&mut self) -> Result<TokenKind, Diagnostic> {
        let start = self.pos;
        let line = self.line;
        let col = self.col;
        if self.bytes[self.pos] == b'0' {
            let radix = match self.bytes.get(self.pos + 1) {
                Some(b'x' | b'X') => Some(16),
                Some(b'o' | b'O') => Some(8),
                Some(b'b' | b'B') => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                return self.lex_prefixed_integer(radix, start, line, col);
            }
        }
        self.skip_decimal_digits();
        // check for decimal point
        if self.pos + 1 < self.bytes.len()
            && self.bytes[self.pos] == b'.'
            && self.bytes[self.pos + 1].is_ascii_digit()
        {
            self.advance(); // consume '.'
            self.skip_decimal_digits();
            let s = &self.src[start..self.pos];
            // Only the separator rules apply; a float half may exceed `i64`.
            for part in s.split('.') {
                match scan_digits(part.as_bytes(), 10) {
                    Ok(_) | Err(NumberError::OutOfRange) => {}
                    Err(err) => {
                        return Err(self.err_numeric_literal(err, s, 10, start, line, col));
                    }
                }
            }
            let f: f64 = if s.contains('_') {
                s.replace('_', "").parse()
            } else {
                s.parse()
            }
            .unwrap_or(0.0);
            return Ok(TokenKind::Float(f));
        }
        let s = &self.src[start..self.pos];
        // A digit sequence that overflows `i64` was previously silently parsed
        // as 0, miscompiling the program. Report it as a source-aware error.
        match scan_digits(s.as_bytes(), 10) {
            Ok(n) => Ok(TokenKind::Integer(n)),
            Err(err) => Err(self.err_numeric_literal(err, s, 10, start, line, col)),
        }
    }

    fn skip_decimal_digits(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
        {
            self.advance();
        }
    }

    /// Lexes `0x…`/`0o…`/`0b…`. The body takes every following identifier
    /// byte so that `0b102` or `0xFG` is one malformed literal with a precise
    /// diagnostic rather than a literal glued to an identifier.
    fn lex_prefixed_integer(
        &mut self,
        radix: u32,
        start: usize,
        line: usize,
        col: usize,
    ) -> Result<TokenKind, Diagnostic> {
        self.advance(); // '0'
        self.advance(); // prefix letter
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_alphanumeric() || self.bytes[self.pos] == b'_')
        {
            self.advance();
        }
        let s = &self.src[start..self.pos];
        if self.bytes[start + 1].is_ascii_uppercase() {
            return Err(self.err_numeric_literal(
                NumberError::UppercasePrefix,
                s,
                radix,
                start,
                line,
                col,
            ));
        }
        let body = &s.as_bytes()[2..];
        // One `_` may directly follow the prefix (`0x_FF`).
        let body = match body {
            [b'_', rest @ ..] if rest.first().is_some_and(|b| b.is_ascii_alphanumeric()) => rest,
            _ => body,
        };
        match scan_digits(body, radix) {
            Ok(n) => Ok(TokenKind::Integer(n)),
            Err(NumberError::InvalidDigit(offset)) => {
                // Translate the body offset back to a literal offset.
                let offset = s.len() - body.len() + offset;
                Err(self.err_numeric_literal(
                    NumberError::InvalidDigit(offset),
                    s,
                    radix,
                    start,
                    line,
                    col,
                ))
            }
            Err(err) => Err(self.err_numeric_literal(err, s, radix, start, line, col)),
        }
    }

    fn err_numeric_literal(
        &self,
        err: NumberError,
        lit: &str,
        radix: u32,
        start: usize,
        line: usize,
        col: usize,
    ) -> Diagnostic {
        let span = self.span_at(start, self.pos, line, col);
        let (kind, prefix, digits) = match radix {
            16 => ("hexadecimal", "0x", "`0`-`9` and `a`-`f` (either case)"),
            8 => ("octal", "0o", "`0`-`7`"),
            2 => ("binary", "0b", "`0` and `1`"),
            _ => ("decimal", "", "`0`-`9`"),
        };
        match err {
            NumberError::OutOfRange => {
                let diag = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0052,
                    format!("integer literal `{lit}` out of range for `i64`"),
                )
                .with_label(Label::primary(span, "value does not fit in `i64`"))
                .with_help(format!(
                    "`i64` values range from {} to {}",
                    i64::MIN,
                    i64::MAX
                ));
                if radix == 10 {
                    diag
                } else {
                    diag.with_help(
                        "a literal cannot set the sign bit; write all bits set as `!0` \
                         and the sign bit as `1 << 63`",
                    )
                }
            }
            NumberError::InvalidDigit(offset) => {
                let digit = lit.as_bytes()[offset] as char;
                let span = self.span_at(start + offset, start + offset + 1, line, col + offset);
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0054,
                    format!("invalid digit `{digit}` in {kind} literal `{lit}`"),
                )
                .with_label(Label::primary(span, format!("not a {kind} digit")))
                .with_help(format!("{kind} digits are {digits}"))
            }
            NumberError::NoDigits => Diagnostic::new(
                Severity::Error,
                ErrorCode::E0054,
                format!("{kind} literal `{lit}` has no digits"),
            )
            .with_label(Label::primary(
                span,
                format!("expected digits after `{prefix}`"),
            )),
            NumberError::MisplacedSeparator => Diagnostic::new(
                Severity::Error,
                ErrorCode::E0054,
                format!("misplaced `_` in numeric literal `{lit}`"),
            )
            .with_label(Label::primary(span, "`_` must sit between two digits"))
            .with_help(
                "a `_` separator may appear only between two digits, or directly after \
                 a `0x`, `0o` or `0b` prefix",
            ),
            NumberError::UppercasePrefix => Diagnostic::new(
                Severity::Error,
                ErrorCode::E0054,
                format!("uppercase radix prefix in `{lit}`"),
            )
            .with_label(Label::primary(span, "radix prefix must be lowercase"))
            .with_help(format!("write `{prefix}`")),
        }
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
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "defer" => TokenKind::Defer,
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
            "const" => TokenKind::Const,
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

/// Why a digit sequence is not a valid `i64` literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberError {
    /// Byte offset of the first byte that is not a digit of the radix.
    InvalidDigit(usize),
    NoDigits,
    MisplacedSeparator,
    OutOfRange,
    UppercasePrefix,
}

/// Validates `digits` (digits of `radix` and `_` separators, no prefix) and
/// returns their value in one pass. Errors are reported in priority order:
/// the first invalid digit, then an empty body, then a misplaced separator,
/// then overflow, so the diagnostic names the most specific problem.
fn scan_digits(digits: &[u8], radix: u32) -> Result<i64, NumberError> {
    let mut value: i64 = 0;
    let mut overflow = false;
    let mut count = 0usize;
    let mut misplaced = false;
    for (i, &b) in digits.iter().enumerate() {
        if b == b'_' {
            // A separator needs a digit on both sides.
            let after = digits.get(i + 1).is_some_and(|n| *n != b'_');
            if i == 0 || !after {
                misplaced = true;
            }
            continue;
        }
        let Some(d) = (b as char).to_digit(radix) else {
            return Err(NumberError::InvalidDigit(i));
        };
        count += 1;
        match value
            .checked_mul(radix as i64)
            .and_then(|v| v.checked_add(d as i64))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if count == 0 {
        Err(NumberError::NoDigits)
    } else if misplaced {
        Err(NumberError::MisplacedSeparator)
    } else if overflow {
        Err(NumberError::OutOfRange)
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use token::TokenKind;

    // Tokenize and return the kinds (without the trailing Eof) on success.
    fn kinds(src: &str) -> Result<Vec<TokenKind>, LexError> {
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

    // Bitwise operators (willow-jz15.8): `<`/`>` stay single tokens so the
    // parser can tell `>>` from two generic closers.
    #[test]
    fn bitwise_operator_tokens() {
        assert_eq!(
            kinds("& &= && | |= || ^ ^= << >>= ").unwrap(),
            vec![
                TokenKind::Ampersand,
                TokenKind::AmpersandEq,
                TokenKind::And,
                TokenKind::Pipe,
                TokenKind::PipeEq,
                TokenKind::Or,
                TokenKind::Caret,
                TokenKind::CaretEq,
                TokenKind::Lt,
                TokenKind::Lt,
                TokenKind::Gt,
                TokenKind::GtEq,
            ]
        );
    }

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

    // ── Exponentiation `**` tokenization (willow-n5yv.2) ─────────────────────
    //
    // `**` is lexed with maximal munch out of the same `*` arm that produces
    // multiplication, so the perspectives below pin both the new token and the
    // fact that every pre-existing `*` spelling is unchanged.

    // Perspective 25: a lone `*` is still multiplication, not exponentiation.
    #[test]
    fn pow_01_single_star_is_multiplication() {
        assert_eq!(
            kinds("2 * 3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::Star,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 26: `**` is one StarStar token, not two Stars.
    #[test]
    fn pow_02_double_star_is_one_token() {
        assert_eq!(
            kinds("2 ** 3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::StarStar,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 27: maximal munch splits `***` as `**` then `*` (the parser
    // rejects the trailing `*`; the lexer must not invent a third operator).
    #[test]
    fn pow_03_triple_star_is_starstar_then_star() {
        assert_eq!(
            kinds("2 *** 3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::StarStar,
                TokenKind::Star,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 28: four stars are two StarStar tokens.
    #[test]
    fn pow_04_quad_star_is_two_starstar() {
        assert_eq!(
            kinds("****").unwrap(),
            vec![TokenKind::StarStar, TokenKind::StarStar]
        );
    }

    // Perspective 29: no whitespace is required around `**`.
    #[test]
    fn pow_05_adjacency_without_spaces() {
        assert_eq!(
            kinds("2**3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::StarStar,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 30: two `*` separated by whitespace stay two Star tokens —
    // maximal munch works on adjacent bytes only.
    #[test]
    fn pow_06_spaced_stars_are_not_starstar() {
        assert_eq!(
            kinds("2 * * 3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::Star,
                TokenKind::Star,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 31: a comment separating two stars also blocks the munch.
    #[test]
    fn pow_07_comment_between_stars_is_not_starstar() {
        assert_eq!(
            kinds("2 */* c */* 3").unwrap(),
            vec![
                TokenKind::Integer(2),
                TokenKind::Star,
                TokenKind::Star,
                TokenKind::Integer(3)
            ]
        );
    }

    // Perspective 32: stars inside a line comment produce no tokens.
    #[test]
    fn pow_08_stars_in_line_comment_are_skipped() {
        assert_eq!(
            kinds("1 // ** stars **\n+ 2").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::Plus,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 33: stars inside a block comment produce no tokens, and the
    // empty comment `/**/` is still an empty comment rather than `/`+`**`+`/`.
    #[test]
    fn pow_09_stars_in_block_comment_are_skipped() {
        assert_eq!(
            kinds("1 /* ** */ /**/ /***/ + 2").unwrap(),
            vec![
                TokenKind::Integer(1),
                TokenKind::Plus,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 34: stars inside a string literal are literal content.
    #[test]
    fn pow_10_stars_in_string_literal_are_content() {
        assert_eq!(
            kinds("\"a ** b\"").unwrap(),
            vec![TokenKind::StringLiteral("a ** b".to_string())]
        );
    }

    #[test]
    fn quoted_string_can_contain_raw_newlines_and_resume_on_next_line() {
        let tokens = Lexer::new("\"first\nsecond\"\n42")
            .tokenize()
            .expect("multi-line string");
        assert_eq!(
            tokens[0].kind,
            TokenKind::StringLiteral("first\nsecond".into())
        );
        assert_eq!(tokens[1].span.line, 3);
        assert_eq!(tokens[1].kind, TokenKind::Integer(42));
    }

    #[test]
    fn unclosed_multi_line_string_reports_opening_quote() {
        let error = Lexer::new("\"first\nsecond")
            .tokenize()
            .expect_err("missing closing quote");
        assert_eq!(error.diagnostics[0].code, ErrorCode::E0051);
        assert_eq!(error.diagnostics[0].primary_span().unwrap().start, 0);
    }

    // Perspective 35: there is no `**=` operator — it lexes as `**` then `=`,
    // which the parser rejects as a malformed assignment.
    #[test]
    fn pow_11_star_star_equals_is_starstar_then_eq() {
        assert_eq!(
            kinds("x **= 2").unwrap(),
            vec![
                TokenKind::Ident("x".to_string()),
                TokenKind::StarStar,
                TokenKind::Eq,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 36: `*=` is a compound assignment token.
    #[test]
    fn pow_12_star_equals_is_compound_assignment() {
        assert_eq!(
            kinds("x *= 2").unwrap(),
            vec![
                TokenKind::Ident("x".to_string()),
                TokenKind::StarEq,
                TokenKind::Integer(2)
            ]
        );
    }

    // Perspective 37: the StarStar span covers both bytes, so diagnostics that
    // point at the operator underline `**` and not just the first `*`.
    #[test]
    fn pow_13_starstar_span_covers_both_bytes() {
        let tokens = Lexer::new("2 ** 3").tokenize().unwrap();
        let op = tokens
            .iter()
            .find(|t| t.kind == TokenKind::StarStar)
            .expect("expected a StarStar token");
        assert_eq!(op.span.end - op.span.start, 2);
    }

    // Perspective 38: `**` at end of input terminates cleanly instead of
    // running past the buffer looking for a third byte.
    #[test]
    fn pow_14_trailing_starstar_at_eof() {
        assert_eq!(
            kinds("2 **").unwrap(),
            vec![TokenKind::Integer(2), TokenKind::StarStar]
        );
    }

    // Perspective 39: a glob-style `use x::*;` tail keeps a single Star, so the
    // import parser's "wildcard imports are not supported" path is unaffected.
    #[test]
    fn pow_15_import_glob_star_is_unchanged() {
        assert_eq!(
            kinds("import math::*;").unwrap(),
            vec![
                TokenKind::Import,
                TokenKind::Ident("math".to_string()),
                TokenKind::ColonColon,
                TokenKind::Star,
                TokenKind::Semicolon
            ]
        );
    }

    // ── Radix prefixes and digit separators (willow-jz15.55) ─────────────────

    fn ints(src: &str) -> Vec<i64> {
        kinds(src)
            .unwrap()
            .into_iter()
            .map(|k| match k {
                TokenKind::Integer(n) => n,
                other => panic!("expected an integer, got {other:?}"),
            })
            .collect()
    }

    // Radix 1: each prefix reads its digits in its own base.
    #[test]
    fn radix_01_prefixes() {
        assert_eq!(ints("0xff 0o17 0b1011 0x0"), vec![255, 15, 11, 0]);
    }

    // Radix 2: hex digits may be upper or lower case, mixed.
    #[test]
    fn radix_02_hex_digit_case() {
        assert_eq!(ints("0xFF 0xfF 0xDeadBeef"), vec![255, 255, 0xDEAD_BEEF]);
    }

    // Radix 3: `i64::MAX` is reachable in every radix.
    #[test]
    fn radix_03_i64_max_in_every_radix() {
        let max = format!("0b{:b} 0o{:o} 0x7FFF_FFFF_FFFF_FFFF", i64::MAX, i64::MAX);
        assert_eq!(ints(&max), vec![i64::MAX; 3]);
    }

    // Radix 4: one past `i64::MAX` is E0052 in every radix; the sign-bit help
    // explains how to spell such masks.
    #[test]
    fn radix_04_sign_bit_is_out_of_range() {
        for src in [
            "0x8000_0000_0000_0000",
            "0o1000000000000000000000",
            "0b1000000000000000000000000000000000000000000000000000000000000000",
        ] {
            let d = first_error(src);
            assert_eq!(d.code, ErrorCode::E0052, "{src}");
            assert!(d.helps.iter().any(|h| h.contains("!0")), "{src}");
        }
    }

    // Radix 5: all bits set is rejected rather than wrapping to -1 (as Rust).
    #[test]
    fn radix_05_all_bits_set_is_rejected() {
        let d = first_error("0xFFFF_FFFF_FFFF_FFFF");
        assert_eq!(d.code, ErrorCode::E0052);
        assert!(d.message.contains("0xFFFF_FFFF_FFFF_FFFF"));
    }

    // Radix 6: very long hex bodies are rejected without panicking.
    #[test]
    fn radix_06_very_long_hex_is_out_of_range() {
        let d = first_error(&format!("0x{}", "F".repeat(200)));
        assert_eq!(d.code, ErrorCode::E0052);
    }

    // Radix 7: leading zeros after the prefix keep the value in range.
    #[test]
    fn radix_07_leading_zeros() {
        assert_eq!(
            ints("0x0000_0000_0000_0000_00FF 0b0001 007"),
            vec![255, 1, 7]
        );
    }

    // Radix 8: invalid digits are E0054 pointing at the offending byte.
    #[test]
    fn radix_08_invalid_digit_span() {
        let d = first_error("0b102");
        assert_eq!(d.code, ErrorCode::E0054);
        assert!(d.message.contains("invalid digit `2` in binary literal"));
        let span = d.labels[0].span;
        assert_eq!((span.start, span.end, span.col), (4, 5, 5));
    }

    // Radix 9: octal rejects 8/9 and hex rejects letters past `f`.
    #[test]
    fn radix_09_invalid_digit_per_radix() {
        assert!(first_error("0o78").message.contains("octal"));
        let d = first_error("0xFG");
        assert_eq!(d.code, ErrorCode::E0054);
        assert!(d.message.contains("`G`"));
    }

    // Radix 10: a bare prefix has no digits.
    #[test]
    fn radix_10_no_digits() {
        for src in ["0x", "0b", "0o", "0x_", "0x;"] {
            let d = first_error(src);
            assert_eq!(d.code, ErrorCode::E0054, "{src}");
            assert!(d.message.contains("has no digits"), "{src}");
        }
    }

    // Radix 11: uppercase prefixes are rejected with a lowercase suggestion.
    #[test]
    fn radix_11_uppercase_prefix() {
        let d = first_error("0XFF");
        assert_eq!(d.code, ErrorCode::E0054);
        assert!(d.helps.iter().any(|h| h.contains("`0x`")));
        assert_eq!(first_error("0B1").code, ErrorCode::E0054);
        assert_eq!(first_error("0O7").code, ErrorCode::E0054);
    }

    // Radix 12: separators between digits are ignored in every radix.
    #[test]
    fn separator_12_between_digits() {
        assert_eq!(
            ints("1_000_000 0xFF_FF 0b1111_0000 0o7_7"),
            vec![1_000_000, 0xFFFF, 0b1111_0000, 0o77]
        );
    }

    // Radix 13: one separator may follow the prefix.
    #[test]
    fn separator_13_after_prefix() {
        assert_eq!(ints("0x_FF 0b_1"), vec![255, 1]);
    }

    // Radix 14: trailing, doubled, or prefix-doubled separators are E0054.
    #[test]
    fn separator_14_misplaced() {
        for src in ["1_", "1__0", "0xFF_", "0x__FF", "0b1__0"] {
            let d = first_error(src);
            assert_eq!(d.code, ErrorCode::E0054, "{src}");
            assert!(d.message.contains("misplaced `_`"), "{src}");
        }
    }

    // Radix 15: a leading `_` is still an identifier, not a number.
    #[test]
    fn separator_15_leading_underscore_is_ident() {
        assert_eq!(
            kinds("_1").unwrap(),
            vec![TokenKind::Ident("_1".to_string())]
        );
    }

    // Radix 16: floats accept separators in both halves and keep their value.
    #[test]
    fn separator_16_float() {
        assert_eq!(
            kinds("1_000.5 2.718_5").unwrap(),
            vec![TokenKind::Float(1000.5), TokenKind::Float(2.7185)]
        );
    }

    // Radix 17: a misplaced separator next to the float point is rejected.
    #[test]
    fn separator_17_float_misplaced() {
        assert_eq!(first_error("1_.5").code, ErrorCode::E0054);
        assert_eq!(first_error("1.5_").code, ErrorCode::E0054);
    }

    // Radix 18: a float whose integer half exceeds `i64` is still a float.
    #[test]
    fn separator_18_large_float_is_not_out_of_range() {
        assert_eq!(
            kinds("99999999999999999999.5").unwrap(),
            vec![TokenKind::Float(99999999999999999999.5)]
        );
    }

    // Radix 19: decimal overflow still reports E0052 when separators are used.
    #[test]
    fn separator_19_decimal_overflow_with_separators() {
        let d = first_error("9_223_372_036_854_775_808");
        assert_eq!(d.code, ErrorCode::E0052);
        assert_eq!(ints("9_223_372_036_854_775_807"), vec![i64::MAX]);
    }

    // Radix 20: a prefixed literal stops at `.`, operators and delimiters, and
    // its span covers exactly the literal.
    #[test]
    fn radix_20_boundaries_and_span() {
        assert_eq!(
            kinds("0xF..0b11").unwrap(),
            vec![
                TokenKind::Integer(15),
                TokenKind::DotDot,
                TokenKind::Integer(3)
            ]
        );
        assert_eq!(
            kinds("(0xff)+0o1").unwrap(),
            vec![
                TokenKind::LParen,
                TokenKind::Integer(255),
                TokenKind::RParen,
                TokenKind::Plus,
                TokenKind::Integer(1)
            ]
        );
        let toks = Lexer::new("  0x_FF ").tokenize().unwrap();
        assert_eq!((toks[0].span.start, toks[0].span.end), (2, 7));
    }

    // Radix 21: lexing recovers after a malformed literal and reports later
    // errors too.
    #[test]
    fn radix_21_recovers_after_error() {
        let errs = Lexer::new("0b2 + 0xFFFF_FFFF_FFFF_FFFF + 0x")
            .tokenize()
            .unwrap_err();
        let codes: Vec<_> = errs.iter().map(|d| d.code).collect();
        assert_eq!(
            codes,
            vec![ErrorCode::E0054, ErrorCode::E0052, ErrorCode::E0054]
        );
    }

    // Radix 22: a plain `0` and decimal numbers starting with `0` are unchanged.
    #[test]
    fn radix_22_zero_forms_unchanged() {
        assert_eq!(ints("0 01 0"), vec![0, 1, 0]);
        assert_eq!(kinds("0.5").unwrap(), vec![TokenKind::Float(0.5)]);
    }
}

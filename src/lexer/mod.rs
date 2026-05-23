pub mod token;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use token::{Token, TokenKind};

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, Vec<Diagnostic>> {
        let mut tokens = Vec::new();
        let mut errors = Vec::new();

        loop {
            self.skip_whitespace_and_comments();
            if self.pos >= self.bytes.len() {
                tokens.push(Token::new(TokenKind::Eof, self.span(self.pos, self.pos)));
                break;
            }

            let start = self.pos;
            let line = self.line;
            let col = self.col;

            match self.next_token() {
                Ok(Some(kind)) => {
                    let span = Span::new(start, self.pos, line, col);
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
                    return Err(self.err_invalid_char(b'&'));
                }
            }
            b'|' => {
                self.advance();
                if self.peek() == Some(b'|') {
                    self.advance();
                    TokenKind::Or
                } else {
                    return Err(self.err_invalid_char(b'|'));
                }
            }
            b'"' => return Err(self.err_unterminated_string()),
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
                TokenKind::Dot
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
            b'0'..=b'9' => self.lex_number(),
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
        let span = Span::new(self.pos, self.pos + 1, self.line, self.col);
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0050,
            format!("invalid character `{}`", c as char),
        )
        .with_label(Label::primary(span, "invalid character"))
    }

    fn err_unterminated_string(&mut self) -> Diagnostic {
        let start = self.pos;
        let line = self.line;
        let col = self.col;
        // consume the opening quote and scan to end of line
        self.advance();
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.advance();
        }
        let span = Span::new(start, start + 1, line, col);
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0051,
            "unterminated string literal",
        )
        .with_label(Label::primary(span, "string starts here but never ends"))
    }

    fn lex_number(&mut self) -> TokenKind {
        let start = self.pos;
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
            return TokenKind::Float(f);
        }
        let s = &self.src[start..self.pos];
        let n: i64 = s.parse().unwrap_or(0);
        TokenKind::Integer(n)
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
            "return" => TokenKind::Return,
            "print" => TokenKind::Print,
            "println" => TokenKind::Println,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "class" => TokenKind::Class,
            "pub" => TokenKind::Pub,
            "open" => TokenKind::Open,
            "override" => TokenKind::Override,
            "extends" => TokenKind::Extends,
            "self" => TokenKind::SelfKw,
            "import" => TokenKind::Import,
            "as" => TokenKind::As,
            "i64" => TokenKind::I64,
            "f64" => TokenKind::F64,
            "bool" => TokenKind::Bool,
            _ => TokenKind::Ident(word.to_string()),
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
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
            } else {
                break;
            }
        }
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

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(start, end, self.line, self.col)
    }
}

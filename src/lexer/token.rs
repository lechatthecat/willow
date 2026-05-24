use crate::diagnostics::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Fn,
    Let,
    Mut,
    If,
    Else,
    While,
    Return,
    Print,
    Println,
    True,
    False,
    Class,
    Pub,
    Open,
    Override,
    Extends,
    SelfKw,
    Import,
    As,
    ColonColon,

    // Types
    I64,
    Bool,
    F64,

    // Literals
    Integer(i64),
    Float(f64),

    // Identifiers
    Ident(String),

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    EqEq,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Pipe,
    Bang,
    Question,

    // Delimiters
    Semicolon,
    Colon,
    Comma,
    Dot,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Arrow,

    // Special
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

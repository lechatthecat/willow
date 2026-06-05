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
    For,
    In,
    Return,
    Print,
    Println,
    True,
    False,
    Nil,
    Class,
    Pub,
    Prot,
    Open,
    Override,
    Extends,
    Interface,
    Implements,
    SelfKw,
    Import,
    Module,
    As,
    Spawn,
    Async,
    Await,
    Select,
    Match,
    Enum,
    ColonColon,

    // Types
    I64,
    Bool,
    F64,

    // Literals
    Integer(i64),
    Float(f64),
    StringLiteral(String),

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
    Ampersand,
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
    LBracket,
    RBracket,
    Arrow,
    FatArrow,

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

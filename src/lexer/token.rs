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
    Break,
    Continue,
    Defer,
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
    /// Reserved modifier, including in member/local names; use `open_gate`
    /// or `is_open` as identifiers (willow-jz15.26, policy b).
    Open,
    Override,
    Static,
    New,
    Extends,
    Interface,
    Implements,
    SelfKw,
    Import,
    Module,
    As,
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
    /// `**`, the right-associative exponentiation operator (willow-n5yv.2).
    StarStar,
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
    DotDot,
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

impl TokenKind {
    /// Source spelling for every lexically recognized keyword, including types.
    pub fn keyword_name(&self) -> Option<&'static str> {
        match self {
            Self::Fn => Some("fn"),
            Self::Let => Some("let"),
            Self::Mut => Some("mut"),
            Self::If => Some("if"),
            Self::Else => Some("else"),
            Self::While => Some("while"),
            Self::Break => Some("break"),
            Self::Continue => Some("continue"),
            Self::Defer => Some("defer"),
            Self::For => Some("for"),
            Self::In => Some("in"),
            Self::Return => Some("return"),
            Self::Print => Some("print"),
            Self::Println => Some("println"),
            Self::True => Some("true"),
            Self::False => Some("false"),
            Self::Nil => Some("nil"),
            Self::Class => Some("class"),
            Self::Pub => Some("pub"),
            Self::Prot => Some("prot"),
            Self::Open => Some("open"),
            Self::Override => Some("override"),
            Self::Static => Some("static"),
            Self::New => Some("new"),
            Self::Extends => Some("extends"),
            Self::Interface => Some("interface"),
            Self::Implements => Some("implements"),
            Self::SelfKw => Some("self"),
            Self::Import => Some("import"),
            Self::Module => Some("module"),
            Self::As => Some("as"),
            Self::Async => Some("async"),
            Self::Await => Some("await"),
            Self::Select => Some("select"),
            Self::Match => Some("match"),
            Self::Enum => Some("enum"),
            Self::I64 => Some("i64"),
            Self::F64 => Some("f64"),
            Self::Bool => Some("bool"),
            _ => None,
        }
    }
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

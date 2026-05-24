use super::span::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelKind {
    Primary,
    Secondary,
}

#[derive(Debug, Clone)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub kind: LabelKind,
}

impl Label {
    pub fn primary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            kind: LabelKind::Primary,
        }
    }

    pub fn secondary(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            kind: LabelKind::Secondary,
        }
    }
}

/// A suggested code fix: replace the bytes at `span` with `replacement`.
///
/// When `span.start == span.end` the fix is a pure insertion.
/// The reporter renders the fixed line and diff markers (`+` for inserted
/// text, `~` for replaced text) below the help message.
#[derive(Debug, Clone)]
pub struct FixSuggestion {
    pub span: Span,
    pub replacement: String,
    pub message: String,
}

impl FixSuggestion {
    pub fn new(span: Span, replacement: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            span,
            replacement: replacement.into(),
            message: message.into(),
        }
    }

    /// Convenience: an insertion fix (span.start == span.end) that inserts
    /// `text` before the column given by `span`.
    pub fn insertion(span: Span, text: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(span, text, message)
    }
}

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

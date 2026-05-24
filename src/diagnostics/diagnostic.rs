use super::error_code::ErrorCode;
use super::label::{FixSuggestion, Label};
use super::span::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: ErrorCode,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
    pub helps: Vec<String>,
    pub fix_suggestions: Vec<FixSuggestion>,
}

impl Diagnostic {
    pub fn new(severity: Severity, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            helps: Vec::new(),
            fix_suggestions: Vec::new(),
        }
    }

    /// Convenience constructor: simple error with a primary label on `span`.
    /// Backward-compatible with all existing call sites.
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        let msg: String = message.into();
        let mut d = Self::new(Severity::Error, ErrorCode::E0001, msg.clone());
        d.labels.push(Label::primary(span, ""));
        d
    }

    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.helps.push(help.into());
        self
    }

    pub fn with_fix(mut self, fix: FixSuggestion) -> Self {
        self.fix_suggestions.push(fix);
        self
    }

    /// Returns the primary label's span, if any.
    pub fn primary_span(&self) -> Option<Span> {
        use super::label::LabelKind;
        self.labels
            .iter()
            .find(|l| l.kind == LabelKind::Primary)
            .map(|l| l.span)
    }
}

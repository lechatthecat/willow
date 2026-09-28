use super::error_code::ErrorCode;
use super::label::{FixSuggestion, Label};
use super::span::Span;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

    /// Project names only at the emission boundary; cached diagnostics and
    /// semantic identities retain their exact compiler spelling.
    pub(crate) fn with_source_names(
        &self,
        packages: &std::collections::HashMap<String, String>,
    ) -> Self {
        let mut result = self.clone();
        result.message = source_names(result.message, packages);
        for label in &mut result.labels {
            label.message = source_names(std::mem::take(&mut label.message), packages);
        }
        for text in result.notes.iter_mut().chain(&mut result.helps) {
            *text = source_names(std::mem::take(text), packages);
        }
        for fix in &mut result.fix_suggestions {
            fix.message = source_names(std::mem::take(&mut fix.message), packages);
        }
        result
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

/// Remove only canonical package namespaces from prose, never from identities
/// or fix replacement text. Scan once so messages with many types stay linear.
fn source_names(text: String, packages: &std::collections::HashMap<String, String>) -> String {
    if !text.contains("$pkg") {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, _) in text.match_indices("$pkg") {
        let end = start + 38;
        if text
            .as_bytes()
            .get(start + 4..start + 36)
            .is_some_and(|hash| hash.iter().all(u8::is_ascii_hexdigit))
            && text.as_bytes().get(start + 36..end) == Some(b"::")
        {
            out.push_str(&text[copied..start]);
            if let Some(prefix) = packages.get(&text[start..start + 36]) {
                out.push_str(prefix);
            }
            copied = end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod source_name_tests {
    use super::*;

    #[test]
    fn dependency_names_remain_distinct_from_root_types() {
        let root = "$pkg0123456789abcdef0123456789abcdef";
        let dep = "$pkgfedcba9876543210fedcba9876543210";
        let packages = [
            (root.into(), String::new()),
            (dep.into(), "ledger::".into()),
        ]
        .into();
        let text = format!("expected {root}::bank::Bank, found {dep}::bank::Bank");
        assert_eq!(
            source_names(text, &packages),
            "expected bank::Bank, found ledger::bank::Bank"
        );
    }

    #[test]
    fn diagnostic_prose_hides_internal_names_without_changing_fixes() {
        let internal = "$pkg0123456789abcdef0123456789abcdef::bank::Bank";
        let message = format!("expected Array<{internal}>, found fn({internal}) -> {internal}");
        let span = Span::new(0, 1, 1, 1);
        let d = Diagnostic::new(Severity::Error, ErrorCode::E2402, &message)
            .with_label(Label::primary(span, &message))
            .with_note(&message)
            .with_help(&message)
            .with_fix(FixSuggestion::new(span, internal, "replacement"))
            .with_source_names(&Default::default());
        for text in [&d.message, &d.labels[0].message, &d.notes[0], &d.helps[0]] {
            assert!(!text.contains("$pkg"));
            assert_eq!(text.matches("bank::Bank").count(), 3);
        }
        assert_eq!(d.fix_suggestions[0].replacement, internal);
        for text in [
            "$pkgshort::Name",
            "$pkg0123456789abcdef0123456789abcdeg::Name",
            "ordinary::Name",
            "日本語",
        ] {
            assert_eq!(source_names(text.into(), &Default::default()), text);
        }
    }
}

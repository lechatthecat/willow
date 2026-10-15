use super::error_code::ErrorCode;
use super::label::{FixSuggestion, Label};
use super::span::Span;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

/// Request-local equivalence of generic diagnostics after display projection.
/// Keep complete payloads in the key: equal locations alone must not hide
/// distinct type-dependent errors, secondary labels, or actionable fixes.
#[derive(Default)]
pub(crate) struct SourceDiagnosticSet(std::collections::HashSet<Diagnostic>);

impl SourceDiagnosticSet {
    pub(crate) fn insert(
        &mut self,
        diagnostic: &Diagnostic,
        packages: &std::collections::HashMap<String, String>,
    ) -> Option<Diagnostic> {
        let source = diagnostic.with_source_names(packages);
        // Diagnostics without a real primary location cannot be attributed to
        // repeated checks of the same source expression.
        if source.primary_span().is_none_or(|span| span.line == 0) {
            return Some(source);
        }
        if self.0.contains(&source) {
            return None;
        }
        self.0.insert(source.clone());
        Some(source)
    }
}

/// Remove only canonical declaration namespaces from prose, never from identities
/// or fix replacement text. Scan once so messages with many types stay linear.
pub(crate) fn source_names(
    text: String,
    packages: &std::collections::HashMap<String, String>,
) -> String {
    if !text.contains("$pkg")
        && !text.contains("$local")
        && !text.contains("$receiver")
        && !text.contains("$mono$")
    {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, _) in text.match_indices('$') {
        if start < copied {
            continue;
        }
        if text[start..].starts_with("$mono$") {
            let end = start
                + 6
                + text[start + 6..]
                    .bytes()
                    .take_while(u8::is_ascii_hexdigit)
                    .count();
            if end == start + 70 {
                out.push_str(&text[copied..start]);
                copied = end;
                continue;
            }
        }
        if text[start..].starts_with("$local::") || text[start..].starts_with("$local.") {
            let end = start
                + if text[start..].starts_with("$local::") {
                    8
                } else {
                    7
                };
            out.push_str(&text[copied..start]);
            copied = end;
            continue;
        }
        // A copied default's receiver (willow-nvi6).
        if text[start..].starts_with(crate::semantic::ids::FOREIGN_RECEIVER_PREFIX) {
            out.push_str(&text[copied..start]);
            copied = start + crate::semantic::ids::FOREIGN_RECEIVER_PREFIX.len();
            continue;
        }
        if !text[start..].starts_with("$pkg") {
            continue;
        }
        let separator = if text.as_bytes().get(start + 36) == Some(&b'.') {
            "."
        } else {
            "::"
        };
        let end = start + 36 + separator.len();
        if text
            .as_bytes()
            .get(start + 4..start + 36)
            .is_some_and(|hash| hash.iter().all(u8::is_ascii_hexdigit))
            && text.as_bytes().get(start + 36..end) == Some(separator.as_bytes())
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
    fn monomorph_names_hide_only_complete_hashes() {
        let hash = "a".repeat(64);
        let text = format!("`$local::boxes::id$mono${hash}` and Box$mono${hash}::get; 日本語");
        assert_eq!(
            source_names(text, &Default::default()),
            "`boxes::id` and Box::get; 日本語"
        );
        for text in ["id$mono$", "id$mono$short", "id$mono$abc"] {
            assert_eq!(source_names(text.into(), &Default::default()), text);
        }
    }

    #[test]
    fn standalone_namespace_is_display_only_and_scans_mixed_names() {
        let text = "expected $local::a::Cell, got $local.b.Cell; $localish::Keep; $other";
        assert_eq!(
            source_names(text.into(), &Default::default()),
            "expected a::Cell, got b.Cell; $localish::Keep; $other"
        );
        let d = Diagnostic::error("$local::a::Cell", Span::dummy());
        assert_eq!(d.with_source_names(&Default::default()).message, "a::Cell");
        assert_eq!(d.message, "$local::a::Cell");
    }

    #[test]
    fn copied_default_receiver_shows_its_own_name() {
        let text = "the method `me` on a `$receiver::A`; $receiverish::B";
        assert_eq!(
            source_names(text.into(), &Default::default()),
            "the method `me` on a `A`; $receiverish::B"
        );
    }

    #[test]
    fn backend_names_hide_package_hashes() {
        let package = "$pkg0123456789abcdef0123456789abcdef";
        assert_eq!(
            source_names(
                format!("function `{package}.graph.empty` has invalid lowered IR"),
                &Default::default()
            ),
            "function `graph.empty` has invalid lowered IR"
        );
        assert_eq!(
            source_names(
                format!("{package}.graph.empty"),
                &[(package.into(), "dep::".into())].into()
            ),
            "dep::graph.empty"
        );
    }

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

#[cfg(test)]
mod generic_diagnostic_tests {
    use super::*;

    fn error() -> Diagnostic {
        Diagnostic::new(Severity::Error, ErrorCode::E0502, "no method on Heap")
            .with_label(Label::primary(Span::new(10, 14, 2, 1), "missing"))
    }

    #[test]
    fn generic_diagnostic_identity_preserves_distinct_payloads() {
        for perspective in 0..13 {
            let first = error();
            let mut second = first.clone();
            match perspective {
                0 => second.labels[0].span.file_id = super::super::FileId(1),
                1 => second.labels[0].span.start += 1,
                2 => second.labels[0].span.end += 1,
                3 => second.code = ErrorCode::E0501,
                4 => second.severity = Severity::Warning,
                5 => second.message.push_str(" different type"),
                6 => second.labels[0].message.push_str(" different label"),
                7 => second
                    .labels
                    .push(Label::secondary(Span::new(30, 40, 3, 1), "definition")),
                8 => second.notes.push("note".into()),
                9 => second.helps.push("help".into()),
                10 => second.fix_suggestions.push(FixSuggestion::new(
                    Span::new(10, 14, 2, 1),
                    "fix",
                    "replace",
                )),
                11 => {
                    second.labels.clear();
                }
                12 => second.labels[0].span = Span::dummy(),
                _ => unreachable!(),
            }
            let mut seen = SourceDiagnosticSet::default();
            assert!(seen.insert(&first, &Default::default()).is_some());
            assert!(
                seen.insert(&second, &Default::default()).is_some(),
                "perspective {perspective}"
            );
            assert!(seen.insert(&first, &Default::default()).is_none());
        }
    }

    #[test]
    fn generic_diagnostic_projection_covers_all_prose_preserves_replacements() {
        let original = error();
        let mut first = original.clone();
        let name = format!("Heap$mono${:064x}", 1);
        first.message = name.clone();
        first.labels[0].message = name.clone();
        first.notes.push(name.clone());
        first.helps.push(name.clone());
        first
            .fix_suggestions
            .push(FixSuggestion::new(Span::dummy(), &name, &name));
        let source = first.with_source_names(&Default::default());
        for text in [
            &source.message,
            &source.labels[0].message,
            &source.notes[0],
            &source.helps[0],
            &source.fix_suggestions[0].message,
        ] {
            assert_eq!(text, "Heap");
        }
        assert_eq!(source.fix_suggestions[0].replacement, name);
        assert_eq!(first.message, name);
        let mut seen = SourceDiagnosticSet::default();
        assert!(seen.insert(&first, &Default::default()).is_some());
        let mut other = first.clone();
        other.fix_suggestions[0].replacement.push('x');
        assert!(seen.insert(&other, &Default::default()).is_some());
    }

    #[test]
    fn generic_diagnostic_unlocated_errors_are_not_collapsed() {
        for dummy in [false, true] {
            let mut diagnostic = error();
            if dummy {
                diagnostic.labels[0].span = Span::dummy();
            } else {
                diagnostic.labels.clear();
            }
            let mut seen = SourceDiagnosticSet::default();
            for _ in 0..2 {
                assert!(seen.insert(&diagnostic, &Default::default()).is_some());
            }
        }
    }

    #[test]
    fn generic_diagnostic_scaling_counts() {
        for locations in [1, 16, 64] {
            for instances in [1, 16, 64] {
                let mut seen = SourceDiagnosticSet::default();
                let mut visits = 0;
                let mut emitted = 0;
                for instance in 0..instances {
                    for location in 0..locations {
                        let mut diagnostic = error();
                        diagnostic.message = format!("no method on Heap$mono${instance:064x}");
                        diagnostic.labels[0].span.start += location;
                        diagnostic.labels[0].span.end += location;
                        visits += 1;
                        emitted +=
                            usize::from(seen.insert(&diagnostic, &Default::default()).is_some());
                    }
                }
                assert_eq!(visits, instances * locations);
                assert_eq!(emitted, locations);
                assert_eq!(seen.0.len(), locations);
                eprintln!(
                    "generic diagnostics: locations={locations} instances={instances} visits={visits} emitted={emitted} retained={}",
                    seen.0.len()
                );
            }
        }
    }
}

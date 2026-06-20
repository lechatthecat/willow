//! Typed failures at compiler phase boundaries.

use std::ops::{Deref, DerefMut};

use thiserror::Error;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};

/// User-facing lexer failure. Diagnostics remain structured for IDE callers.
#[derive(Debug, Error)]
#[error("lexing failed with {count} error(s)", count = diagnostics.len())]
pub struct LexError {
    pub diagnostics: Vec<Diagnostic>,
}

impl LexError {
    pub fn new(diagnostics: Vec<Diagnostic>) -> Self {
        Self { diagnostics }
    }
}

impl Deref for LexError {
    type Target = Vec<Diagnostic>;

    fn deref(&self) -> &Self::Target {
        &self.diagnostics
    }
}

impl DerefMut for LexError {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.diagnostics
    }
}

impl IntoIterator for LexError {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diagnostics.into_iter()
    }
}

/// User-facing import/module-resolution failure.
#[derive(Debug, Error)]
#[error("module resolution failed with {error_count} error(s)")]
pub struct ResolveError {
    pub diagnostics: Vec<Diagnostic>,
    pub error_count: usize,
}

impl ResolveError {
    pub fn from_diagnostics(diagnostics: &[Diagnostic]) -> Option<Self> {
        let error_count = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .count();
        (error_count > 0).then(|| Self {
            diagnostics: diagnostics.to_vec(),
            error_count,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenStage {
    Initialize,
    Module(String),
    Entry,
    Metadata,
    Finish,
}

/// Internal native-code generation failure, distinct from user diagnostics.
#[derive(Debug, Error)]
#[error("code generation failed during {stage:?}: {message}")]
pub struct CodegenError {
    pub stage: CodegenStage,
    pub message: String,
}

impl CodegenError {
    pub fn new(stage: CodegenStage, error: impl std::fmt::Display) -> Self {
        Self {
            stage,
            message: error.to_string(),
        }
    }

    pub fn diagnostic(&self) -> Diagnostic {
        let context = match &self.stage {
            CodegenStage::Module(module) => format!(" in module `{module}`"),
            _ => String::new(),
        };
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error{context}: {}", self.message),
        )
    }
}

/// Internal failure not attributable to user source.
#[derive(Debug, Error)]
#[error("internal compiler error in {phase}: {message}")]
pub struct InternalCompilerError {
    pub phase: &'static str,
    pub message: String,
}

impl InternalCompilerError {
    pub fn new(phase: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            phase,
            message: error.to_string(),
        }
    }

    pub fn diagnostic(&self, span: crate::diagnostics::Span) -> Diagnostic {
        Diagnostic::new(Severity::Error, ErrorCode::E0800, self.to_string())
            .with_label(Label::primary(span, "compiler failed here"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_error_counts_only_error_severity() {
        let diagnostics = vec![
            Diagnostic::new(Severity::Warning, ErrorCode::W2002, "warning"),
            Diagnostic::new(Severity::Error, ErrorCode::E0401, "error"),
        ];
        assert_eq!(
            ResolveError::from_diagnostics(&diagnostics)
                .unwrap()
                .error_count,
            1
        );
    }

    #[test]
    fn codegen_error_preserves_stage_context() {
        let error = CodegenError::new(CodegenStage::Module("math".into()), "failure");
        assert!(error.diagnostic().message.contains("module `math`"));
    }
}

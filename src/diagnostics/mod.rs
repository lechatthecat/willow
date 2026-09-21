#![allow(dead_code)]

pub mod diagnostic;
pub mod error_code;
pub mod label;
pub mod reporter;
pub mod source_map;
pub mod span;

pub use diagnostic::{Diagnostic, Severity};
pub use error_code::ErrorCode;
pub use label::{FixSuggestion, Label};
pub use reporter::{emit, emit_all, emit_all_multi, emit_multi};
pub use source_map::{DebugSourceMap, SourceMap, SourceMaps};
pub use span::{FileId, Span};

/// A request-local diagnostic destination; library callers need no global mode.
pub trait DiagnosticEmitter {
    fn emit(
        &mut self,
        diagnostic: &Diagnostic,
        sources: &dyn source_map::SourceLookup,
    ) -> std::io::Result<()>;
}

pub struct HumanEmitter;

impl DiagnosticEmitter for HumanEmitter {
    fn emit(
        &mut self,
        diagnostic: &Diagnostic,
        sources: &dyn source_map::SourceLookup,
    ) -> std::io::Result<()> {
        reporter::emit_with(diagnostic, sources, &mut std::io::stderr().lock())
    }
}

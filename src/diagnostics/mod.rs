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

/// Project CLI adapter shared by human and structured diagnostic destinations.
/// Borrows source maps without copying source text or rebuilding line indexes.
pub struct ProjectEmitter<'a> {
    pub root: &'a std::path::Path,
    pub inner: &'a mut dyn DiagnosticEmitter,
}

impl DiagnosticEmitter for ProjectEmitter<'_> {
    fn emit(
        &mut self,
        diagnostic: &Diagnostic,
        sources: &dyn source_map::SourceLookup,
    ) -> std::io::Result<()> {
        struct ProjectSources<'a> {
            root: &'a std::path::Path,
            sources: &'a dyn source_map::SourceLookup,
        }
        impl source_map::SourceLookup for ProjectSources<'_> {
            fn get(&self, file_id: FileId) -> Option<&SourceMap> {
                self.sources.get(file_id)
            }

            fn display_path(&self, file_id: FileId) -> Option<&str> {
                let path = self.sources.display_path(file_id)?;
                Some(
                    std::path::Path::new(path)
                        .strip_prefix(self.root)
                        .ok()
                        .and_then(|relative| relative.to_str())
                        .unwrap_or(path),
                )
            }
        }
        self.inner.emit(
            diagnostic,
            &ProjectSources {
                root: self.root,
                sources,
            },
        )
    }
}

impl DiagnosticEmitter for HumanEmitter {
    fn emit(
        &mut self,
        diagnostic: &Diagnostic,
        sources: &dyn source_map::SourceLookup,
    ) -> std::io::Result<()> {
        reporter::emit_with(diagnostic, sources, &mut std::io::stderr().lock())
    }
}

#[cfg(test)]
mod project_path_tests;

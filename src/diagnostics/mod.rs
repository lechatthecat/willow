#![allow(dead_code)]

pub mod diagnostic;
pub mod error_code;
pub mod label;
pub mod reporter;
pub mod source_map;
pub mod span;

pub use diagnostic::{Diagnostic, Severity};
pub use error_code::ErrorCode;
pub use label::{FixSuggestion, Label, LabelKind};
pub use reporter::{emit, emit_all};
pub use source_map::{DebugSourceMap, SourceMap};
pub use span::Span;

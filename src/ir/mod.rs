//! Intermediate representations between the AST and the Cranelift backend.
//!
//! [`typed_ast`] is the typed high-level IR (HIR): every expression carries its
//! resolved type. [`lower`] builds the HIR from the type-checked AST. This is
//! the staged replacement for the backend's current AST-plus-`Span`-map
//! approach (willow-mb5).

pub mod lower;
pub mod typed_ast;

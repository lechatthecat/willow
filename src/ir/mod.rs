//! Intermediate representations between the AST and the Cranelift backend.
//!
//! [`typed_ast`] is the typed high-level IR (HIR): every expression carries its
//! resolved type. [`lower`] builds HIR from the type-checked AST, and
//! [`lowered`] produces the LIR consumed by backend emission (willow-mb5).

pub mod dump;
pub mod lower;
pub mod lowered;
pub mod module_init;
mod optimize;
pub mod typed_ast;

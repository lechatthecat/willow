//! Intermediate representations between the AST and the Cranelift backend.
//!
//! [`typed_ast`] is the typed high-level IR (HIR): every expression carries its
//! resolved type. [`lower`] builds HIR from the type-checked AST, and
//! [`lowered`] produces the LIR consumed by backend emission.

/// Match any of the listed variants, ignoring their payloads. Each decision
/// lists its own cases so adding an IR variant requires revisiting that pass.
macro_rules! ir_variants {
    ($kind:ident; $($variant:ident),+ $(,)?) => {
        $( $kind::$variant { .. } )|+
    };
}
pub(crate) use ir_variants;

pub mod dump;
pub mod lower;
pub mod lowered;
pub mod module_init;
mod optimize;
pub mod typed_ast;

/// Hash table scans cost capacity. Reuse small scratch tables and release a
/// large table after the first operation that used only a fraction of it.
fn sparse_scratch_table(capacity: usize, entries: usize) -> bool {
    capacity / 4 > entries.max(8)
}

#[cfg(test)]
mod contracts_tests;

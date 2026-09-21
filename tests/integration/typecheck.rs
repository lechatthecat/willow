//! End-to-end type-checking and execution tests, grouped by responsibility.
//! Leaf names and the `typecheck::` filter are preserved; exact paths include
//! the child module. Shared helpers remain in `integration/support.rs`.

#[path = "typecheck/classes.rs"]
mod classes;
#[path = "typecheck/gc.rs"]
mod gc;
#[path = "typecheck/matching.rs"]
mod matching;
#[path = "typecheck/option_context.rs"]
mod option_context;
#[path = "typecheck/option_result.rs"]
mod option_result;
#[path = "typecheck/performance.rs"]
mod performance;
#[path = "typecheck/protected.rs"]
mod protected;
#[path = "typecheck/receivers.rs"]
mod receivers;
#[path = "typecheck/subtyping.rs"]
mod subtyping;
#[path = "typecheck/variant_diagnostics.rs"]
mod variant_diagnostics;
#[path = "typecheck/variants.rs"]
mod variants;
#[path = "typecheck/wrapper_methods.rs"]
mod wrapper_methods;

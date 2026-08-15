//! End-to-end compiler tests, grouped by the phase or subsystem they exercise.
//!
//! The modules also form practical test tiers: frontend-focused checks can run
//! independently from native-code and runtime checks with Cargo's test filter.

#[path = "integration/support.rs"]
mod support;

#[path = "integration/diagnostics.rs"]
mod diagnostics;
#[path = "integration/lexer.rs"]
mod lexer;
#[path = "integration/parser.rs"]
mod parser;
#[path = "integration/typecheck.rs"]
mod typecheck;

#[path = "integration/codegen.rs"]
mod codegen;
#[path = "integration/defer_panic_termination.rs"]
mod defer_panic_termination;
#[path = "integration/lambda_shadowing.rs"]
mod lambda_shadowing;
#[path = "integration/panic_effects.rs"]
mod panic_effects;
#[path = "integration/panic_recover.rs"]
mod panic_recover;
#[path = "integration/panic_recover_matrix.rs"]
mod panic_recover_matrix;
#[path = "integration/panic_recover_review.rs"]
mod panic_recover_review;
#[path = "integration/panic_recover_stress.rs"]
mod panic_recover_stress;
#[path = "integration/runtime.rs"]
mod runtime;
#[path = "integration/runtime_safety_matrix.rs"]
mod runtime_safety_matrix;

#[path = "integration/concurrency.rs"]
mod concurrency;
#[path = "integration/exponentiation.rs"]
mod exponentiation;
#[path = "integration/option_interface_context.rs"]
mod option_interface_context;
#[path = "integration/option_nil_deprecation.rs"]
mod option_nil_deprecation;
#[path = "integration/option_repr_niche.rs"]
mod option_repr_niche;
#[path = "integration/option_shorthand_diagnostics.rs"]
mod option_shorthand_diagnostics;
#[path = "integration/option_sugar_normalization.rs"]
mod option_sugar_normalization;
#[path = "integration/scheduler_wakeup.rs"]
mod scheduler_wakeup;
#[path = "integration/stack_switch_capability.rs"]
mod stack_switch_capability;
#[path = "integration/toolchain.rs"]
mod toolchain;

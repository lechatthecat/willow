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
#[path = "integration/scheduler_wakeup.rs"]
mod scheduler_wakeup;
#[path = "integration/toolchain.rs"]
mod toolchain;

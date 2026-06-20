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
#[path = "integration/runtime.rs"]
mod runtime;

#[path = "integration/concurrency.rs"]
mod concurrency;
#[path = "integration/toolchain.rs"]
mod toolchain;

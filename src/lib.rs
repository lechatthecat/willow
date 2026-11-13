// `Diagnostic` is the compiler's pervasive error type; returning it by value
// keeps fallible parser/semantic signatures readable. Boxing every `Result` to
// shrink the cold `Err` path is churn not worth it here, so allow it crate-wide.
#![allow(
    clippy::result_large_err,
    reason = "Keep the pervasive Diagnostic error type by value"
)]

pub mod ai;
pub mod backend;
pub mod compiler_db;
pub(crate) mod compiler_stack;
pub mod desugar;
mod diagnostic_settings;
pub mod diagnostics;
pub mod errors;
mod hex;
pub mod interpolate;
pub mod ir;
pub mod lexer;
pub mod module;
pub mod package;
pub mod parser;
pub mod prelude;
pub mod project;
pub(crate) mod qualified_name;
mod query_stats;
pub mod rust_bridge_ir;
pub mod semantic;
pub mod stdlib_schema;
pub mod toolchain;
mod workspace_transaction;

mod driver;
#[cfg(test)]
use anyhow::Result;
pub use driver::{
    BuildMode, CodegenOptions, CompilerOptions, CompilerSession, TargetOptions, check_file,
    compile, emit_hir_text, emit_lir_text,
};
#[cfg(test)]
pub(crate) use driver::{
    DEPENDENCY_WORK, ModuleImportBindings, desugar_phase, import_phase, register_prelude,
    run_frontend, typecheck_phase,
};
pub(crate) use driver::{
    Frontend, backend_unit_imports, discovery_roots, run_frontend_revision,
    run_frontend_revision_cached, run_frontend_with_inputs,
};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
mod single_file_import_tests;

#[cfg(test)]
mod package_identity_tests;

#[cfg(test)]
use compiler_db::dependencies::ModuleDependencies;

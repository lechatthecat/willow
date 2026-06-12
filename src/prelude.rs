/// The Willow standard prelude.
///
/// Items in the prelude are implicitly available in every source file without
/// an explicit `import`.  Like Rust's `std::prelude::v1`, this is intentionally
/// small: only the types and functions needed to write basic programs.
///
/// The prelude is compiled from Willow source (PRELUDE_SOURCE) before the user
/// program and its enums / functions are registered in the symbol table.
pub const PRELUDE_SOURCE: &str = r#"
pub enum Option<T> {
    Some(T),
    None,
}

pub enum Result<T, E> {
    Ok(T),
    Err(E),
}

pub enum ParseFloatError {
    Invalid(String),
}

pub interface Into<T> {
    fn into(self) -> T;
}

// Compiler-known marker interfaces for safe concurrency (willow-dgwo).
// `Send` = a value may be transferred across worker/task boundaries.
// `Sync` = a value may be shared by multiple workers/tasks concurrently.
// These are not normal interfaces: the compiler INFERS them from a type's
// structure, and user code may not implement them manually (error E2401).
// An interface may `extends Send` / `extends Sync` to require its
// implementations (and thus its interface values) to be Send / Sync.
pub interface Send {}
pub interface Sync {}
"#;

/// Virtual file name shown in diagnostics for prelude items.
pub const PRELUDE_FILE: &str = "<prelude>";

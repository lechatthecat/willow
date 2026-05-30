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
"#;

/// Virtual file name shown in diagnostics for prelude items.
pub const PRELUDE_FILE: &str = "<prelude>";

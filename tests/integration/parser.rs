//! End-to-end syntax and semantic integration tests, grouped by responsibility.
//! Leaf test names and the `parser::` filter are preserved; exact paths include
//! the child module. Shared helpers remain in `integration/support.rs`.

#[path = "parser/bindings.rs"]
mod bindings;
#[path = "parser/classes.rs"]
mod classes;
#[path = "parser/control_flow.rs"]
mod control_flow;
#[path = "parser/expressions.rs"]
mod expressions;

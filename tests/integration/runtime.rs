use super::support::*;

#[path = "runtime/gc_allocation.rs"]
mod gc_allocation;

// ── Interface dispatch (willow-xds, spec 14) ───────────────────────────────

const IFACE_ANIMALS: &str = r#"
interface Animal {
    fn speak(self) -> String;
}
class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn speak(self) -> String { return "meow"; }
}
"#;

// ---------------------------------------------------------------------------
// A MODULE that reaches ANOTHER module's enum through a plain (unrenamed) item
// import (willow-favj). The aliased form of this lives in
// `enum_identity_aliases::METER`; the plain one had no coverage, and its
// failure mode was silent: every construction behaved like variant 0.
// ---------------------------------------------------------------------------

const FAVJ_ALPHA: &str = r#"
module alpha;
pub enum Color { A, B, C }
pub enum Tag { Plain, Payload(i64) }
pub fn own_code(c: Color) -> i64 {
    return match c {
        Color::A => 1,
        Color::B => 2,
        Color::C => 3,
    };
}
"#;

const FAVJ_BETA: &str = r#"
module beta;
import alpha::Color;
import alpha::Tag;

pub fn code(c: Color) -> i64 {
    return match c {
        Color::A => 1,
        Color::B => 2,
        Color::C => 3,
    };
}
pub fn code_a() -> i64 { return code(Color::A); }
pub fn code_b() -> i64 { return code(Color::B); }
pub fn code_c() -> i64 { return code(Color::C); }
pub fn nth(n: i64) -> Color {
    if n == 0 { return Color::A; }
    if n == 1 { return Color::B; }
    return Color::C;
}
pub fn value(t: Tag) -> i64 {
    return match t {
        Tag::Plain => 0,
        Tag::Payload(v) => v,
    };
}
pub fn payload(v: i64) -> Tag { return Tag::Payload(v); }
"#;

#[path = "runtime/advanced_interfaces.rs"]
mod advanced_interfaces;
#[path = "runtime/async_frames.rs"]
mod async_frames;
#[path = "runtime/async_suspension.rs"]
mod async_suspension;
#[path = "runtime/basic_execution.rs"]
mod basic_execution;
#[path = "runtime/classes.rs"]
mod classes;
#[path = "runtime/collections_strings.rs"]
mod collections_strings;
#[path = "runtime/default_methods.rs"]
mod default_methods;
#[path = "runtime/display.rs"]
mod display;
#[path = "runtime/downcast.rs"]
mod downcast;
#[path = "runtime/error_conversion.rs"]
mod error_conversion;
#[path = "runtime/examples.rs"]
mod examples;
#[path = "runtime/gc_roots.rs"]
mod gc_roots;
#[path = "runtime/generic_interfaces.rs"]
mod generic_interfaces;
#[path = "runtime/imported_types.rs"]
mod imported_types;
#[path = "runtime/inheritance_validation.rs"]
mod inheritance_validation;
#[path = "runtime/interface_inheritance.rs"]
mod interface_inheritance;
#[path = "runtime/interfaces.rs"]
mod interfaces;
#[path = "runtime/main_result.rs"]
mod main_result;
#[path = "runtime/panic.rs"]
mod panic;
#[path = "runtime/stack_traces.rs"]
mod stack_traces;
#[path = "runtime/type_visibility.rs"]
mod type_visibility;
#[path = "runtime/virtual_dispatch.rs"]
mod virtual_dispatch;
#[path = "runtime/virtual_error_conversion.rs"]
mod virtual_error_conversion;

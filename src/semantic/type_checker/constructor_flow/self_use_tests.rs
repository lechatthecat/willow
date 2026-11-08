//! Uses of a partially initialized `self` in constructors (E0852/E0853, and
//! E0848 for a skipped `super.init(...)`).
//!
//! Perspectives: reads before assignment (direct, nested receiver, String,
//! Array, one branch, loop body, compound assignment, ternary arm, nested
//! field store), escapes (helper method, virtual call in a base constructor,
//! argument, binding, match scrutinee, `super.m(...)`, lambda capture, lambda
//! call, defer body), base initialization (inherited read before
//! `super.init`, `self` in its arguments, `return` before it, an escaping base
//! requiring subclass fields first, transitively through an intermediate
//! class) and the accepted counterparts of each.

use crate::diagnostics::{Diagnostic, ErrorCode};

fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

const INNER: &str = "class Inner { pub v: i64; pub fn get(self) -> i64 { return self.v; } }";

/// `C` with fields `x: i64`, `y: i64`, `i: Inner` and a helper method.
fn class_c(body: &str) -> String {
    format!(
        "{INNER} fn take(c: C) {{}} class C {{ x: i64; y: i64; i: Inner; \
         init(self, flag: bool) {{ {body} }} fn peek(self) -> i64 {{ return self.x; }} \
         static fn make() -> i64 {{ return 1; }} }} fn main() {{}}"
    )
}

/// Full-assignment suffix that keeps E0842 out of the way.
const ALL: &str = "self.x = 1; self.y = 2; self.i = new Inner(3);";

fn codes(source: &str) -> Vec<ErrorCode> {
    check_source(source).iter().map(|d| d.code).collect()
}

fn assert_codes(source: &str, expected: &[ErrorCode], message: &str) {
    let errors = check_source(source);
    let got: Vec<_> = errors.iter().map(|d| d.code).collect();
    assert_eq!(got, expected, "{source}\n{errors:#?}");
    if !message.is_empty() {
        assert!(
            errors.iter().any(|d| d.message.contains(message)),
            "{message}: {errors:#?}"
        );
    }
}

#[test]
fn reads_before_assignment_are_rejected() {
    use ErrorCode::E0852;
    let cases: &[(&str, &str)] = &[
        // 1. the dogfood repro: a method on an unassigned reference field
        (
            &format!("println(self.i.get()); {ALL}"),
            "field `i` is read before",
        ),
        // 2. a scalar read
        (
            &format!("let a = self.x; {ALL}"),
            "field `x` is read before",
        ),
        // 3. assigned on one branch only
        (
            &format!("if flag {{ self.x = 1; }} let a = self.x; {ALL}"),
            "field `x`",
        ),
        // 4. first loop iteration reads before the body assigns
        (
            &format!("while flag {{ let a = self.y; self.y = 1; break; }} {ALL}"),
            "field `y`",
        ),
        // 5. compound update of an unassigned field
        (&format!("self.x = self.x + 1; {ALL}"), "field `x`"),
        // 6. a ternary arm
        (&format!("self.x = flag ? 1 : self.y; {ALL}"), "field `y`"),
        // 7. a store through an unassigned field reads it
        (&format!("self.i.v = 4; {ALL}"), "field `i`"),
        // 8. a lambda created before the assignment captures the read
        (
            &format!("let f = || -> i64 {{ return self.x; }}; {ALL}"),
            "field `x`",
        ),
        // 9. a deferred body is checked where it is registered
        (&format!("defer {{ println(self.y); }} {ALL}"), "field `y`"),
    ];
    for (body, message) in cases {
        assert_codes(&class_c(body), &[E0852], message);
    }
}

#[test]
fn string_and_array_fields_cannot_be_observed_unassigned() {
    assert_codes(
        "import std::collections::Array; class S { s: String; a: Array<i64>; \
         init(self) { println(self.s.len()); println(self.a[0]); self.s = \"x\"; self.a = [1]; } } \
         fn main() {}",
        &[ErrorCode::E0852, ErrorCode::E0852],
        "field `s` is read before",
    );
}

#[test]
fn escapes_before_full_initialization_are_rejected() {
    use ErrorCode::E0853;
    let cases: &[(&str, &str)] = &[
        // 10. a helper method (dogfood repro 2)
        (
            &format!("self.peek(); {ALL}"),
            "`self` is used before field `x` is assigned",
        ),
        // 11. passed as an argument after some fields
        (
            &format!("self.x = 1; take(self); {ALL}"),
            "before field `y` is assigned",
        ),
        // 12. bound to a local
        (&format!("let me = self; {ALL}"), "`self` is used before"),
        // 13. a lambda calling a method on it
        (
            &format!("let f = || -> i64 {{ return self.peek(); }}; {ALL}"),
            "`self` is used before",
        ),
        // 14. a deferred method call
        (
            &format!("defer self.peek(); {ALL}"),
            "`self` is used before",
        ),
        // 15. a deferred block passing it
        (
            &format!("defer {{ take(self); }} {ALL}"),
            "`self` is used before",
        ),
        // 16. the last field missing on one path only
        (
            &format!(
                "self.x = 1; self.i = new Inner(1); if flag {{ self.y = 1; }} self.peek(); {ALL}"
            ),
            "before field `y` is assigned",
        ),
    ];
    for (body, message) in cases {
        assert_codes(&class_c(body), &[E0853], message);
    }
}

#[test]
fn match_on_partial_self_is_rejected() {
    assert_codes(
        &class_c(&format!("match self {{ _ => {{}} }} {ALL}")),
        &[ErrorCode::E0853],
        "`self` is used before",
    );
}

#[test]
fn fully_initialized_self_and_assigned_reads_are_accepted() {
    for body in [
        // reads after assignment, both-branch joins
        format!("{ALL} self.y = self.x + self.i.get();"),
        "if flag { self.x = 1; } else { self.x = 2; } self.y = self.x; self.i = new Inner(self.y);"
            .to_string(),
        // escapes after every field
        format!("{ALL} self.peek(); take(self); let me = self;"),
        format!("{ALL} let f = || -> i64 {{ return self.peek(); }}; println(f());"),
        format!("{ALL} defer self.peek(); defer {{ take(self); }}"),
        // a lambda that only stores, and static calls, never read `self`
        "let f = || { self.x = 5; }; self.x = C::make(); self.y = 2; self.i = new Inner(3);"
            .to_string(),
        // a deferred store is not a read
        format!("defer {{ self.x = 9; }} {ALL}"),
        // the store-target `self` and receivers of assigned fields
        format!("{ALL} self.i.v = 4; println(self.i.v);"),
    ] {
        assert_eq!(codes(&class_c(&body)), [], "{body}");
    }
}

const ESCAPING_BASE: &str = "open class Base { pub tag: i64; \
     pub init(self) { self.tag = 0; self.tag = self.describe(); } \
     pub open fn describe(self) -> i64 { return 0; } }";

const QUIET_BASE: &str = "open class Base { pub tag: i64; pub init(self, t: i64) { self.tag = t; } \
     pub open fn describe(self) -> i64 { return 0; } }";

#[test]
fn virtual_call_from_base_constructor_is_rejected() {
    // 17. dogfood repro 3: base calls an override before its own field and
    // the subclass assigns its field after `super.init`.
    let source = format!(
        "{INNER} open class Base {{ pub tag: i64; pub init(self) {{ self.tag = self.describe(); }} \
         pub open fn describe(self) -> i64 {{ return 0; }} }} \
         class Sub extends Base {{ inner: Inner; pub init(self) {{ super.init(); self.inner = new Inner(7); }} \
         pub override fn describe(self) -> i64 {{ return self.inner.get(); }} }} fn main() {{}}"
    );
    assert_codes(
        &source,
        &[ErrorCode::E0853, ErrorCode::E0853],
        "field `inner` must be assigned before `super.init(...)`",
    );
}

#[test]
fn subclass_of_escaping_base_must_assign_fields_first() {
    let sub = |body: &str| {
        format!(
            "{INNER} {ESCAPING_BASE} class Sub extends Base {{ inner: Inner; pub init(self) {{ {body} }} \
             pub override fn describe(self) -> i64 {{ return self.inner.get(); }} }} fn main() {{}}"
        )
    };
    // 18. assigning after `super.init` lets the override read it unassigned
    assert_codes(
        &sub("super.init(); self.inner = new Inner(7);"),
        &[ErrorCode::E0853],
        "field `inner` must be assigned before `super.init(...)`",
    );
    // accepted: the Swift-style order, including reads of assigned fields
    assert_eq!(
        codes(&sub(
            "self.inner = new Inner(7); println(self.inner.get()); super.init(); println(self.tag);"
        )),
        []
    );
}

#[test]
fn escaping_base_requirement_is_transitive() {
    // 19. Mid itself never uses `self`, but its `super.init` reaches Base.
    let source = |leaf: &str| {
        format!(
            "{ESCAPING_BASE} open class Mid extends Base {{ m: i64; pub init(self) {{ self.m = 1; super.init(); }} }} \
             class Leaf extends Mid {{ l: i64; pub init(self) {{ {leaf} }} }} fn main() {{}}"
        )
    };
    assert_codes(
        &source("super.init(); self.l = 1;"),
        &[ErrorCode::E0853],
        "field `l` must be assigned before",
    );
    assert_eq!(codes(&source("self.l = 1; super.init();")), []);
}

#[test]
fn quiet_base_keeps_fields_after_super_init() {
    // AC: existing constructors keep compiling when the base never lets
    // `self` escape.
    let source = format!(
        "{INNER} {QUIET_BASE} class Sub extends Base {{ inner: Inner; \
         pub init(self) {{ super.init(1); self.inner = new Inner(self.tag); self.describe(); }} }} fn main() {{}}"
    );
    assert_eq!(codes(&source), []);
}

#[test]
fn base_initialization_order_is_checked() {
    let sub = |body: &str| {
        format!(
            "{QUIET_BASE} fn take(b: Base) {{}} class Sub extends Base {{ own: i64; \
             pub init(self, flag: bool) {{ {body} }} }} fn main() {{}}"
        )
    };
    // 20. an inherited field read before the base is initialized
    assert_codes(
        &sub("self.own = 1; super.init(self.tag);"),
        &[ErrorCode::E0852],
        "inherited field `tag` is read before `super.init(...)`",
    );
    // 21. `self` handed out in the base initializer's arguments
    assert_codes(
        &sub("self.own = 1; take(self); super.init(1);"),
        &[ErrorCode::E0853],
        "`self` is used before `super.init(...)` runs",
    );
    // 22. `super.m(...)` runs on the partial object
    assert_codes(
        &sub("super.init(super.describe()); self.own = 1;"),
        &[ErrorCode::E0853],
        "`self` is used before",
    );
    // 23. a return that skips the base initializer
    assert_codes(
        &sub("self.own = 1; if flag { return; } super.init(1);"),
        &[ErrorCode::E0848],
        "can return without calling `super.init(...)`",
    );
    // accepted: own reads before, inherited reads and escapes after
    assert_eq!(
        codes(&sub(
            "self.own = 2; super.init(self.own); self.own = self.tag; take(self);"
        )),
        []
    );
}

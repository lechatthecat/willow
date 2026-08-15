use super::support::*;

// Contextual Option<Interface> construction — willow-glaj.2.
//
// These perspectives distinguish construction-time payload coercion from
// forbidden generic covariance. A concrete object is boxed while building the
// target Option<Interface>; an already-built Option<Concrete> never widens.
// The runtime cases call through the extracted interface, so storing a raw
// concrete pointer in the enum payload cannot pass silently.
//
// 27 perspectives:
//   1-2   bare/qualified Some in annotated lets
//   3-4   bare/qualified Some in return position
//   5-6   bare/qualified Some in argument position
//   7     reassignment into Option<Interface>
//   8-9   field assignment and memberwise construction
//  10-11  array literal and indexed assignment
//  12-13  nested Option and Result payload context
//  14     generic interface instantiation
//  15     ternary branches inherit the same payload context
//  16-18  GC stress: allocating payload, minor collection, heap field
//  19     an already-boxed interface is not boxed a second time
//  20     None remains the no-payload control
//  21     AST/LIR-selection differential
//  22-24  reject Option/class and unrelated generic covariance
//  25-27  explicit constructor, Array.push, and interface-call argument context

const PRELUDE: &str = r#"
import std::collections::Array;

interface Greeter {
    fn greet(self) -> String;
}

class Dog implements Greeter {
    pub name: String;

    pub fn greet(self) -> String {
        return "dog:" + self.name;
    }
}

class Cat implements Greeter {
    pub name: String;

    pub fn greet(self) -> String {
        return "cat:" + self.name;
    }
}

class Holder {
    pub value: Option<Greeter>;
}

class ExplicitHolder {
    pub value: Option<Greeter>;

    pub init(self, value: Option<Greeter>) {
        self.value = value;
    }
}

interface Consumer {
    fn consume(self, value: Option<Greeter>) -> String;
}

class Reader implements Consumer {
    pub fn consume(self, value: Option<Greeter>) -> String {
        match value {
            Some(item) => return item.greet(),
            None => return "none",
        }
    }
}

fn read(value: Option<Greeter>) -> String {
    match value {
        Some(item) => return item.greet(),
        None => return "none",
    }
}
"#;

fn source(body: &str) -> String {
    format!("{PRELUDE}\n{body}")
}

fn run(body: &str) -> String {
    let source = source(body);
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    out
}

#[test]
fn option_iface_01_bare_some_in_annotated_let_boxes_payload() {
    assert_eq!(
        run(r#"fn main() { let v: Option<Greeter> = Some(new Dog("rex")); println(read(v)); }"#),
        "dog:rex\n"
    );
}

#[test]
fn option_iface_02_qualified_some_in_annotated_let_boxes_payload() {
    assert_eq!(
        run(
            r#"fn main() { let v: Option<Greeter> = Option::Some(new Cat("mimi")); println(read(v)); }"#
        ),
        "cat:mimi\n"
    );
}

#[test]
fn option_iface_03_bare_some_in_return_boxes_payload() {
    assert_eq!(
        run(r#"
fn make() -> Option<Greeter> { return Some(new Dog("return")); }
fn main() { println(read(make())); }
"#),
        "dog:return\n"
    );
}

#[test]
fn option_iface_04_qualified_some_in_return_boxes_payload() {
    assert_eq!(
        run(r#"
fn make() -> Option<Greeter> { return Option::Some(new Cat("return")); }
fn main() { println(read(make())); }
"#),
        "cat:return\n"
    );
}

#[test]
fn option_iface_05_bare_some_in_argument_boxes_payload() {
    assert_eq!(
        run(r#"fn main() { println(read(Some(new Dog("arg")))); }"#),
        "dog:arg\n"
    );
}

#[test]
fn option_iface_06_qualified_some_in_argument_boxes_payload() {
    assert_eq!(
        run(r#"fn main() { println(read(Option::Some(new Cat("arg")))); }"#),
        "cat:arg\n"
    );
}

#[test]
fn option_iface_07_assignment_boxes_payload() {
    assert_eq!(
        run(r#"
fn main() {
    let mut value: Option<Greeter> = None;
    value = Some(new Dog("assigned"));
    println(read(value));
}
"#),
        "dog:assigned\n"
    );
}

#[test]
fn option_iface_08_field_assignment_boxes_payload() {
    assert_eq!(
        run(r#"
fn main() {
    let holder = new Holder(None);
    holder.value = Some(new Cat("field"));
    println(read(holder.value));
}
"#),
        "cat:field\n"
    );
}

#[test]
fn option_iface_09_memberwise_constructor_boxes_payload() {
    assert_eq!(
        run(r#"
fn main() {
    let holder = new Holder(Some(new Dog("member")));
    println(read(holder.value));
}
"#),
        "dog:member\n"
    );
}

#[test]
fn option_iface_10_array_literal_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let values: Array<Option<Greeter>> = [Some(new Dog("array")), None];
    println(read(values[0]));
    println(read(values[1]));
}
"#),
        "dog:array\nnone\n"
    );
}

#[test]
fn option_iface_11_index_assignment_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let values: Array<Option<Greeter>> = [None];
    values[0] = Some(new Cat("index"));
    println(read(values[0]));
}
"#),
        "cat:index\n"
    );
}

#[test]
fn option_iface_12_nested_option_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let outer: Option<Option<Greeter>> = Some(Some(new Dog("nested")));
    match outer { Some(inner) => println(read(inner)), None => println("outer-none") }
}
"#),
        "dog:nested\n"
    );
}

#[test]
fn option_iface_13_result_payload_propagates_option_context() {
    assert_eq!(
        run(r#"
fn main() {
    let result: Result<Option<Greeter>, String> = Ok(Some(new Cat("result")));
    match result { Ok(value) => println(read(value)), Err(e) => println(e) }
}
"#),
        "cat:result\n"
    );
}

#[test]
fn option_iface_14_generic_interface_instantiation_is_boxed() {
    let source = r#"
interface Container<T> { fn get(self) -> T; }
class TextBox implements Container<String> {
    pub value: String;
    pub fn get(self) -> String { return self.value; }
}
fn read_box(value: Option<Container<String>>) -> String {
    match value { Some(item) => return item.get(), None => return "none" }
}
fn main() {
    let value: Option<Container<String>> = Some(new TextBox("generic"));
    println(read_box(value));
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "generic\n");
}

#[test]
fn option_iface_15_ternary_branches_share_context() {
    assert_eq!(
        run(r#"
fn choose(flag: bool) -> Option<Greeter> {
    return flag ? Some(new Dog("left")) : Some(new Cat("right"));
}
fn main() { println(read(choose(true))); println(read(choose(false))); }
"#),
        "dog:left\ncat:right\n"
    );
}

#[test]
fn option_iface_16_allocating_payload_is_rooted_during_boxing() {
    let source = source(
        r#"fn main() { let v: Option<Greeter> = Some(new Dog("ro" + "oted")); println(read(v)); }"#,
    );
    let (out, ok) = compile_and_run_with_env(
        &source,
        &[
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "dog:rooted\n");
}

#[test]
fn option_iface_17_payload_survives_minor_collection() {
    assert_eq!(
        run(r#"
fn main() {
    let value: Option<Greeter> = Some(new Dog("minor"));
    gc_minor_collect();
    let mut i = 0;
    while i < 256 { let noise = new Dog("x"); i = i + 1; }
    println(read(value));
}
"#),
        "dog:minor\n"
    );
}

#[test]
fn option_iface_18_heap_field_survives_allocation_stress() {
    let source = source(
        r#"
fn main() {
    let holder = new Holder(Some(new Cat("heap")));
    let mut i = 0;
    while i < 64 { let noise = "n" + i.toString(); i = i + 1; }
    println(read(holder.value));
}
"#,
    );
    let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_GC_STRESS", "alloc")]);
    assert!(ok, "{out}");
    assert_eq!(out, "cat:heap\n");
}

#[test]
fn option_iface_19_existing_interface_payload_is_identity() {
    assert_eq!(
        run(r#"
fn main() {
    let item: Greeter = new Dog("boxed-once");
    let value: Option<Greeter> = Some(item);
    println(read(value));
}
"#),
        "dog:boxed-once\n"
    );
}

#[test]
fn option_iface_20_none_control_has_no_payload() {
    assert_eq!(
        run(r#"fn main() { let value: Option<Greeter> = None; println(read(value)); }"#),
        "none\n"
    );
}

#[test]
fn option_iface_21_ast_and_lir_selection_agree() {
    let source = source(
        r#"
fn make() -> Option<Greeter> { return Some(new Dog("parity")); }
fn main() { println(read(make())); }
"#,
    );
    let (ast, ast_ok) = compile_and_run_with_env(&source, &[("WILLOW_LIR_BACKEND", "0")]);
    let (lir, lir_ok) = compile_and_run_with_env(&source, &[("WILLOW_LIR_BACKEND", "1")]);
    assert!(ast_ok, "AST: {ast}");
    assert!(lir_ok, "LIR selection: {lir}");
    assert_eq!(ast, lir);
    assert_eq!(ast, "dog:parity\n");
}

#[test]
fn option_iface_22_existing_option_concrete_does_not_widen_on_assignment() {
    let source = source(
        r#"
fn main() {
    let concrete: Option<Dog> = Some(new Dog("no"));
    let widened: Option<Greeter> = concrete;
}
"#,
    );
    assert_compile_error_contains(
        &source,
        &[
            "error[E0201]",
            "expected `Option<Greeter>`",
            "found `Option<Dog>`",
        ],
    );
}

#[test]
fn option_iface_23_existing_option_concrete_does_not_widen_as_argument() {
    let source = source(
        r#"
fn main() {
    let concrete: Option<Cat> = Some(new Cat("no"));
    println(read(concrete));
}
"#,
    );
    assert_compile_error_contains(
        &source,
        &[
            "error[E0201]",
            "expected `Option<Greeter>`",
            "found `Option<Cat>`",
        ],
    );
}

#[test]
fn option_iface_24_unrelated_generic_does_not_gain_covariance() {
    let source = source(
        r#"
enum Wrap<T> { Value(T), Empty, }
fn main() {
    let concrete: Wrap<Dog> = Wrap::Value(new Dog("no"));
    let widened: Wrap<Greeter> = concrete;
}
"#,
    );
    assert_compile_error_contains(
        &source,
        &[
            "error[E0201]",
            "expected `Wrap<Greeter>`",
            "found `Wrap<Dog>`",
        ],
    );
}

#[test]
fn option_iface_25_explicit_constructor_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let holder = new ExplicitHolder(Some(new Dog("explicit")));
    println(read(holder.value));
}
"#),
        "dog:explicit\n"
    );
}

#[test]
fn option_iface_26_array_push_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let values: Array<Option<Greeter>> = [];
    values.push(Some(new Cat("push")));
    println(read(values[0]));
}
"#),
        "cat:push\n"
    );
}

#[test]
fn option_iface_27_interface_call_argument_propagates_payload_context() {
    assert_eq!(
        run(r#"
fn main() {
    let consumer: Consumer = new Reader();
    println(consumer.consume(Some(new Dog("dispatch-arg"))));
}
"#),
        "dog:dispatch-arg\n"
    );
}

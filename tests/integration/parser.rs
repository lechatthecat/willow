use super::support::*;

#[test]
fn test_arithmetic_i64() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 3;
    println(a + b);
    println(a - b);
    println(a * b);
    println(a / b);
    println(a % b);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "13\n7\n30\n3\n1\n");
}

#[test]
fn test_arithmetic_f64() {
    let src = r#"
fn main() {
    let x: f64 = 2.5;
    let y: f64 = 4.0;
    println(x * y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "10");
}

#[test]
fn test_operator_precedence_and_parentheses() {
    let src = r#"
fn main() {
    println(1 + 2 * 3);
    println((1 + 2) * (3 + 4));
    println(20 / (3 + 2));
    println(20 % (3 + 2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n21\n4\n0\n");
}

#[test]
fn test_arithmetic_left_associative() {
    let src = r#"
fn main() {
    println(20 - 5 - 3);
    println(100 / 5 / 2);
    println(29 % 10 % 4);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "12\n10\n1\n");
}

#[test]
fn test_negative_values_in_expressions() {
    let src = r#"
fn main() {
    let a = -10;
    let b = 4;

    println(a + b);
    println(a * -b);
    println((a - b) / 2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-6\n40\n-7\n");
}

#[test]
fn test_f64_comparison_and_unary_neg() {
    let src = r#"
fn main() {
    let x: f64 = -2.5;
    let y: f64 = 5.0;
    println(x < 0.0);
    println(y / 2.0);
    println(x != y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n2.5\ntrue\n");
}

#[test]
fn test_f64_equality_and_false_comparisons() {
    let src = r#"
fn main() {
    let x: f64 = 1.5;
    let y: f64 = 2.5;

    println(x == y);
    println(x != y);
    println(y <= x);
    println(y > x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\ntrue\n");
}

// ── Variables ────────────────────────────────────────────────────────────────

#[test]
fn test_let_mut() {
    let src = r#"
fn main() {
    let mut a = 10;
    a = 20;
    println(a);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "20");
}

#[test]
fn test_type_annotation() {
    let src = r#"
fn main() {
    let x: i64 = 99;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "99");
}

#[test]
fn test_mutable_f64_assignment() {
    let src = r#"
fn main() {
    let mut x: f64 = 1.5;
    x = x + 2.5;

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "4");
}

#[test]
fn test_mut_reference_i64_local_writeback() {
    let src = r#"
fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 10;
    increment(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "11\n");
}

#[test]
fn test_mut_reference_f64_local_writeback() {
    let src = r#"
fn add_half(x: &mut f64) {
    x = x + 0.5;
}

fn main() {
    let mut n: f64 = 2.0;
    add_half(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_mut_reference_bool_local_writeback() {
    let src = r#"
fn flip(x: &mut bool) {
    x = !x;
}

fn main() {
    let mut enabled = false;
    flip(&enabled);
    println(enabled);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n");
}

#[test]
fn test_immutable_reference_reads_from_immutable_local() {
    let src = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    let n = 10;
    println(read(&n));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n");
}

#[test]
fn test_immutable_reference_parameter_rejects_assignment() {
    assert_compile_error_contains(
        r#"
fn increment(x: & i64) {
    x = x + 1;
}

fn main() {
    let n = 10;
    increment(&n);
}
"#,
        &["cannot assign to immutable parameter `x`"],
    );
}

#[test]
fn test_gc_string_immutable_reference_survives_collect_in_callee() {
    let src = r#"
fn shout(text: & String) -> String {
    gc_collect();
    return text + "!";
}

fn main() {
    let text = "he" + "llo";
    println(shout(&text));
    gc_collect();
    println(text);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String & local should remain rooted across callee collect"
    );
    assert_eq!(out, "hello!\nhello\n");
}

#[test]
fn test_gc_string_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut text = "he" + "llo";
    replace(&text);
    gc_collect();
    println(text);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "hello!\ntrue\n");
}

#[test]
fn test_gc_class_immutable_reference_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn read(box: & Box) -> String {
    gc_collect();
    return box.value;
}

fn main() {
    let box = new Box("ke" + "pt");
    println(read(&box));
    gc_collect();
    println(box.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class & local should remain rooted across callee collect"
    );
    assert_eq!(out, "kept\nkept\n");
}

#[test]
fn test_gc_class_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn replace(box: &mut Box) {
    box = new Box("after" + "!");
    gc_collect();
}

fn main() {
    let mut box = new Box("before");
    replace(&box);
    gc_collect();
    println(box.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "after!\ntrue\n");
}

#[test]
fn test_mut_reference_object_field_i64_writeback() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let counter = new Counter(10);
    increment(&counter.value);
    println(counter.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as &mut i64");
    assert_eq!(out, "11\n");
}

#[test]
fn test_immutable_reference_object_field_read() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let counter = new Counter(21);
    println(read_twice(&counter.value));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_object_field_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
class User {
    pub name: String;
}

fn replace(name: &mut String) {
    name = name + "!";
    gc_collect();
}

fn main() {
    let user = new User("Al" + "ice");
    replace(&user.name);
    gc_collect();
    println(user.name);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String field &mut assignment should survive callee collect"
    );
    assert_eq!(out, "Alice!\ntrue\n");
}

#[test]
fn test_mut_reference_private_object_field_is_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    secret: i64;

    pub static fn new(v: i64) -> User {
        return new User(v);
    }
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let user = User::new(10);
    increment(&user.secret);
}
"#,
        &[
            "error[E0501]",
            "field `secret` of class `User` is private",
            "private field",
        ],
    );
}

#[test]
fn test_mut_reference_array_element_i64_writeback() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [10, 20];
    increment(&xs[0]);
    println(xs[0]);
    println(xs[1]);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as &mut i64");
    assert_eq!(out, "11\n20\n");
}

#[test]
fn test_immutable_reference_array_element_read() {
    let src = r#"
import std::collections::Array;

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let xs: Array<i64> = [21];
    println(read_twice(&xs[0]));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_array_element_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
import std::collections::Array;

fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut names: Array<String> = ["Al" + "ice", "willow"];
    replace(&names[0]);
    gc_collect();
    println(names[0]);
    println(names[1]);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String array element &mut assignment should survive callee collect"
    );
    assert_eq!(out, "Alice!\nwillow\ntrue\n");
}

#[test]
fn test_array_element_reference_out_of_bounds_reports_runtime_diagnostic() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
    println(99);
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "out-of-bounds array element reference should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
}

#[test]
fn test_block_scope_shadowing_restores_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;
        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn test_nested_block_shadowing_restores_each_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;

        if true {
            let x = 3;
            println(x);
        }

        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "3\n2\n1\n");
}

// ── Control flow ─────────────────────────────────────────────────────────────

#[test]
fn test_if_else() {
    let src = r#"
fn main() {
    let x = 5;
    if x > 3 {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_if_without_else() {
    let src = r#"
fn main() {
    let mut value = 1;

    if true {
        value = value + 41;
    }

    println(value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_while_loop() {
    let src = r#"
fn main() {
    let mut i = 0;
    while i < 5 {
        println(i);
        i = i + 1;
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "0\n1\n2\n3\n4\n");
}

#[test]
fn test_while_zero_iterations() {
    let src = r#"
fn main() {
    let mut count = 0;

    while false {
        count = count + 1;
    }

    println(count);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0");
}

#[test]
fn test_nested_if_inside_while() {
    let src = r#"
fn main() {
    let mut i = 0;
    let mut total = 0;

    while i < 6 {
        if i % 2 == 0 {
            total = total + i;
        } else {
            total = total + 1;
        }
        i = i + 1;
    }

    println(total);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "9");
}

#[test]
fn test_while_factorial_accumulator() {
    let src = r#"
fn main() {
    let mut n = 1;
    let mut acc = 1;

    while n <= 6 {
        acc = acc * n;
        n = n + 1;
    }

    println(acc);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_bool_condition_from_expression() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 20;

    if (a < b && b == 20) || false {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

// ── Functions ────────────────────────────────────────────────────────────────

#[test]
fn test_function_call() {
    let src = r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}
fn main() {
    println(add(10, 32));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_nested_calls_and_bool_return() {
    let src = r#"
fn midpoint(a: f64, b: f64) -> f64 {
    return (a + b) / 2.0;
}

fn above_midpoint(a: f64, b: f64, limit: f64) -> bool {
    return midpoint(a, b) > limit;
}

fn main() {
    println(midpoint(3.0, 5.0));
    println(above_midpoint(3.0, 5.0, 3.5));
    println(above_midpoint(3.0, 5.0, 4.0));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "4\ntrue\nfalse\n");
}

#[test]
fn test_recursive_fib() {
    let src = r#"
fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    println(fib(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "55");
}

#[test]
fn test_recursive_factorial_function() {
    let src = r#"
fn factorial(n: i64) -> i64 {
    if n <= 1 {
        return 1;
    }

    return n * factorial(n - 1);
}

fn main() {
    println(factorial(6));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_mutual_recursion() {
    let src = r#"
fn is_even(n: i64) -> bool {
    if n == 0 {
        return true;
    }

    return is_odd(n - 1);
}

fn is_odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }

    return is_even(n - 1);
}

fn main() {
    println(is_even(8));
    println(is_odd(8));
    println(is_odd(9));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn test_pub_function() {
    let src = r#"
pub fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    println(double(21));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_forward_function_call() {
    let src = r#"
fn main() {
    println(triple(14));
}

fn triple(x: i64) -> i64 {
    return x * 3;
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_return_from_both_if_branches() {
    let src = r#"
fn sign(n: i64) -> i64 {
    if n < 0 {
        return -1;
    } else {
        return 1;
    }
}

fn main() {
    println(sign(-8));
    println(sign(8));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-1\n1\n");
}

#[test]
fn test_function_returning_bool_used_as_if_condition() {
    let src = r#"
fn in_range(value: i64, min: i64, max: i64) -> bool {
    return value >= min && value <= max;
}

fn main() {
    if in_range(7, 1, 10) {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_void_function_without_explicit_return() {
    let src = r#"
fn emit_twice(value: i64) {
    println(value);
    println(value);
}

fn main() {
    emit_twice(9);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "9\n9\n");
}

#[test]
fn test_void_function_and_early_return() {
    let src = r#"
fn emit(flag: bool) {
    if flag {
        println(1);
        return;
    }

    println(0);
}

fn main() {
    emit(true);
    emit(false);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "1\n0\n");
}

// ── Classes ─────────────────────────────────────────────────────────────────

#[test]
fn test_class_declarations_parse_with_top_level_main() {
    let src = r#"
pub open class Animal {
    age: i64;

    pub open fn speak(self) -> i64 {
        return 1;
    }
}

pub class Dog extends Animal {
    pub override fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(42);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_class_subtype_assignment_accepts_child_as_base() {
    let src = r#"
pub open class Animal {
    pub open fn speak(self) -> i64 {
        return 1;
    }
}

pub class Dog extends Animal {
}

fn upcast(dog: Dog) -> Animal {
    let animal: Animal = dog;
    return dog;
}

fn call_inherited(dog: Dog) -> i64 {
    return dog.speak();
}

fn main() {
    println(42);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_class_subtype_assignment_rejects_base_as_child() {
    assert_compile_error_contains(
        r#"
pub open class Animal {
}

pub class Dog extends Animal {
}

fn downcast(animal: Animal) -> Dog {
    let dog: Dog = animal;
    return dog;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0704]",
            "cannot assign `Animal` to variable `dog` of type `Dog`",
            "expected `Dog` because of this type annotation",
        ],
    );
}

#[test]
fn test_class_extending_non_open_base_reports_e0701() {
    assert_compile_error_contains(
        r#"
class Animal {
}

class Dog extends Animal {
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0701]",
            "class `Animal` is not open for inheritance",
            "cannot extend this class",
            "base class defined here",
            "help: declare the base class as `open class Animal`",
        ],
    );
}

#[test]
fn test_class_override_requires_override_keyword() {
    assert_compile_error_contains(
        r#"
open class Animal {
    open fn speak(self) -> i64 {
        return 1;
    }
}

class Dog extends Animal {
    fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0702]",
            "method `speak` overrides `Animal` but is missing `override`",
            "missing `override`",
            "help: write `override fn speak`",
        ],
    );
}

#[test]
fn test_class_override_requires_open_base_method() {
    assert_compile_error_contains(
        r#"
open class Animal {
    fn speak(self) -> i64 {
        return 1;
    }
}

class Dog extends Animal {
    override fn speak(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0703]",
            "method `speak` in `Animal` is not open for override",
            "cannot override",
            "base method defined here",
            "help: declare the base method as `open fn speak`",
        ],
    );
}

#[test]
fn test_class_cross_module_qualified_base_type_checks() {
    let animal = r#"
pub open class Animal {
    pub open fn speak(self) -> i64 {
        return 1;
    }
}
"#;
    let main = r#"
import animal;

pub class Dog extends animal::Animal {
    pub override fn speak(self) -> i64 {
        return 2;
    }
}

fn upcast(dog: Dog) -> animal::Animal {
    return dog;
}

fn main() {
    println(42);
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animal.wi", animal), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified base class project failed to compile or run");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_direct_imported_module_subclass_upcasts_and_keeps_base_layout() {
    let animal = r#"
pub open class Animal {
    pub init(self, age: i64) {
        self.age = age;
    }
    age: i64;

    pub open fn speak(self) -> i64 {
        return self.age + 1;
    }
}

pub class Dog extends Animal {
    pub init(self, age: i64, bonus: i64) {
        super.init(age);
        self.bonus = bonus;
    }
    bonus: i64;

    pub fn total(self) -> i64 {
        return self.speak() + self.bonus;
    }
}
"#;
    let main = r#"
import animal::Animal;
import animal::Dog;

fn describe(animal: Animal) -> i64 {
    return animal.speak();
}

fn main() {
    let dog = new Dog(40, 2);
    let animal: Animal = dog;
    println(describe(dog));
    println(animal.speak());
    println(dog.total());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animal.wi", animal), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "direct-imported module subclass project failed to compile or run"
    );
    assert_eq!(out, "41\n41\n43\n");
}

// ───────────────────────────────────────────────────────────────────────────
// Module class inheritance across the module boundary (willow-2egr). A class
// `extends Base` declared in an imported module must: inherit base methods,
// inherit the base's `implements` (incl. transitively, with a real vtable so
// interface dispatch does not segfault), and resolve its qualified base chain.
// The 20 perspectives below are split across the focused tests that follow.
//
//   P1  override on an imported subclass dispatches to the override
//   P2  method inherited from a TRANSITIVE base (Dog -> Mid -> Animal)
//   P3  method inherited from the INTERMEDIATE base (Dog -> Mid)
//   P4  un-overridden method falls through to the base (Cat -> Animal.speak)
//   P5  base method inherited by a direct subclass (Cat.base_only)
//   P6  imported subclass inherits base's `implements`, used as that interface
//       (the segfault case): iface dispatch hits the override
//   P7  another subclass inherits `implements` and dispatches to the inherited
//   P8  DEFAULT interface method reachable via inherited `implements` (override)
//   P9  default interface method via inherited `implements` (no override)
//   P10 upcast an imported subclass to its (imported) base CLASS type
//   P11 pass an imported subclass where the base class type is expected
//   P12 call through a base-class-typed binding dispatches to the override
//   P13 an ENTRY subclass extending an imported base, with an override
//   P14 an entry subclass inherits the imported base's concrete method
//   P15 an entry subclass is usable where the imported base type is expected
//   P16 inherited method works after a cross-module `super.init`
//   P17 a subclass's own method calls an inherited method + its own field
//   P18 inherited field read on an imported subclass
//   P19 own field read on an imported subclass
//   P20 a missing method on an imported subclass still reports E0502
// ───────────────────────────────────────────────────────────────────────────

const ZOO_MODULE_2EGR: &str = r#"
pub interface Speaker {
    fn speak(self) -> i64;
    fn intro(self) -> i64 { return self.speak() + 1; }
}

pub open class Animal implements Speaker {
    pub value: i64;
    pub open fn speak(self) -> i64 { return self.value; }
    pub fn base_only(self) -> i64 { return self.value + 1; }
}

pub open class Mid extends Animal {
    pub fn mid_only(self) -> i64 { return self.value + 2; }
}

pub class Dog extends Mid {
    pub override fn speak(self) -> i64 { return self.value + 1000; }
}

pub class Cat extends Animal {
}
"#;

#[test]
fn test_2egr_module_subclass_inherited_concrete_methods() {
    // P1 override, P2 transitive-base method, P3 intermediate-base method,
    // P4 un-overridden falls through, P5 direct-subclass inherited method.
    let main = r#"
import zoo::Dog;
import zoo::Cat;

fn main() {
    let d = new Dog(5);
    println(d.speak());      // P1: override -> 1005
    println(d.base_only());  // P2: inherited from Animal through Mid -> 6
    println(d.mid_only());   // P3: inherited from Mid -> 7
    let c = new Cat(3);
    println(c.speak());      // P4: un-overridden -> Animal.speak -> 3
    println(c.base_only());  // P5: inherited -> 4
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("zoo.wi", ZOO_MODULE_2EGR), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "module subclass inherited methods failed to compile/run"
    );
    assert_eq!(out, "1005\n6\n7\n3\n4\n");
}

#[test]
fn test_2egr_module_subclass_inherits_implements_via_interface() {
    // P6/P7 inherited `implements` + iface dispatch (the segfault case),
    // P8/P9 default interface method reached through inherited `implements`.
    let main = r#"
import zoo::Dog;
import zoo::Cat;
import zoo::Speaker;

fn via(s: Speaker) -> i64 { return s.speak(); }
fn via_default(s: Speaker) -> i64 { return s.intro(); }

fn main() {
    let d = new Dog(5);
    let c = new Cat(3);
    println(via(d));          // P6: Dog inherits implements (Mid->Animal) -> override -> 1005
    println(via(c));          // P7: Cat inherits implements -> 3
    println(via_default(d));  // P8: default `intro` over inherited implements -> 1006
    println(via_default(c));  // P9: default `intro` -> 4
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("zoo.wi", ZOO_MODULE_2EGR), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "inherited cross-module `implements` failed to compile/run"
    );
    assert_eq!(out, "1005\n3\n1006\n4\n");
}

#[test]
fn test_2egr_upcast_and_entry_subclass_of_module_base() {
    // P10 upcast, P11 pass-as-base, P12 dispatch through base binding,
    // P13 entry subclass + override, P14 entry subclass inherits base method,
    // P15 entry subclass usable as the imported base type.
    let main = r#"
import zoo::Animal;
import zoo::Dog;

fn describe(a: Animal) -> i64 { return a.speak(); }

pub class EntryDog extends Animal {
    pub override fn speak(self) -> i64 { return self.value + 500; }
}

fn main() {
    let d = new Dog(5);
    let a: Animal = d;        // P10: upcast imported subclass to imported base
    println(describe(d));     // P11: pass subclass where base expected -> 1005
    println(a.speak());       // P12: dispatch through base binding -> 1005
    let e = new EntryDog(2);
    println(e.speak());       // P13: entry subclass override -> 502
    println(e.base_only());   // P14: entry subclass inherits Animal.base_only -> 3
    println(describe(e));     // P15: entry subclass as imported base -> 502
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("zoo.wi", ZOO_MODULE_2EGR), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "upcast / entry-subclass-of-module-base failed to compile/run"
    );
    assert_eq!(out, "1005\n1005\n502\n3\n502\n");
}

#[test]
fn test_2egr_cross_module_super_init_and_fields() {
    // P16 inherited method after cross-module super.init, P17 own method calls
    // inherited method + own field, P18 inherited field, P19 own field.
    let base = r#"
pub open class Base {
    pub age: i64;
    pub init(self, age: i64) { self.age = age; }
    pub open fn val(self) -> i64 { return self.age; }
}

pub class Sub extends Base {
    pub bonus: i64;
    pub init(self, age: i64, bonus: i64) {
        super.init(age);
        self.bonus = bonus;
    }
    pub fn total(self) -> i64 { return self.val() + self.bonus; }
}
"#;
    let main = r#"
import base::Sub;

fn main() {
    let s = new Sub(40, 2);
    println(s.val());    // P16: inherited method after super.init -> 40
    println(s.total());  // P17: own method calls inherited val + own field -> 42
    println(s.age);      // P18: inherited field -> 40
    println(s.bonus);    // P19: own field -> 2
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("base.wi", base), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "cross-module super.init / inherited fields failed to compile/run"
    );
    assert_eq!(out, "40\n42\n40\n2\n");
}

#[test]
fn test_2egr_missing_method_on_module_subclass_reports_e0502() {
    // P20: resolution must not become so permissive that a genuinely missing
    // method slips through — it still reports E0502.
    let main = r#"
import zoo::Dog;

fn main() {
    println(new Dog(1).nonexistent());
}
"#;
    let stderr = compile_temp_project_error_stderr(
        &[("zoo.wi", ZOO_MODULE_2EGR), ("main.wi", main)],
        "main.wi",
    );
    assert!(
        stderr.contains("error[E0502]") && stderr.contains("nonexistent"),
        "expected E0502 for a missing method on an imported subclass:\n{stderr}"
    );
}

#[test]
fn test_class_new_replaces_object_literal_construction() {
    let src = r#"
pub class AA {
    pub value: i64;
}

pub class A {
    pub value: i64;
    pub aa: AA;

    pub fn member_aa(self) -> AA {
        return self.aa;
    }

    pub fn member_aa_value(self) -> i64 {
        return self.aa.value;
    }
}

fn consume(a: A) -> i64 {
    return 7;
}

fn make_a(value: i64) -> A {
    return new A(value, new AA(value + 1));
}

fn main() {
    let a = make_a(40);
    println(consume(a));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "new constructor program failed to compile or run");
    assert_eq!(out.trim(), "7");
}

#[test]
fn test_class_object_literal_rejected_with_new_guidance() {
    assert_compile_error_contains(
        r#"
class Point {
    x: i64;
    y: i64;
}

fn make_point() -> Point {
    return Point { x: 1, y: 2 };
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0847]",
            "object literal construction for `Point` is no longer supported",
            "use `new Point(...)`",
        ],
    );
}

#[test]
fn test_class_methods_can_read_private_self_fields() {
    let src = r#"
class Box {
    value: i64;

    pub fn value(self) -> i64 {
        return self.value;
    }
}

fn main() {
    println(1);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class method private self-field access should type-check"
    );
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_class_object_literal_reaches_private_member_diagnostic() {
    assert_compile_error_contains(
        r#"
pub class Account {
    balance: i64;

    pub static fn new(balance: i64) -> Account {
        return new Account(balance);
    }
}

fn main() {
    let account = Account::new(500);
    println(account.balance);
}
"#,
        &[
            "error[E0501]",
            "field `balance` of class `Account` is private",
            "private field",
            "field defined here",
        ],
    );
}

#[test]
fn test_class_diagnostic_private_field_points_to_definition() {
    assert_compile_error_contains(
        r#"
class User {
    name: i64;
}

fn leak(user: User) -> i64 {
    return user.name;
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0501]",
            "field `name` of class `User` is private",
            "private field",
            "field defined here",
            "help: expose it using `pub name: i64` or provide a public getter method",
        ],
    );
}

#[test]
fn test_class_diagnostic_private_method_points_to_definition() {
    assert_compile_error_contains(
        r#"
class User {
    fn secret(self) -> i64 {
        return 7;
    }
}

fn leak(user: User) -> i64 {
    return user.secret();
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0501]",
            "method `secret` of class `User` is private",
            "private method",
            "method defined here",
            "help: make it public with `pub fn secret`",
        ],
    );
}

#[test]
fn test_class_diagnostic_method_not_found_suggests_similar_name() {
    assert_compile_error_contains(
        r#"
class User {
    pub fn greet(self) -> i64 {
        return 1;
    }
}

fn call(user: User) -> i64 {
    return user.greett();
}

fn main() {
    println(1);
}
"#,
        &[
            "error[E0502]",
            "no method `greett` on class `User`",
            "method not found",
            "help: there is a method with a similar name: `greet`",
            "return user.greet();",
        ],
    );
}

// ── Boolean operators ─────────────────────────────────────────────────────────

#[test]
fn test_bool_operators() {
    let src = r#"
fn main() {
    println(true && false);
    println(true || false);
    println(!true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\n");
}

#[test]
fn test_bool_operators_short_circuit_rhs() {
    let src = r#"
fn marker(value: bool) -> bool {
    println(99);
    return value;
}

fn main() {
    println(false && marker(true));
    println(true || marker(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn test_bool_operator_precedence() {
    let src = r#"
fn main() {
    println(true || false && false);
    println((true || false) && false);
    println(!false && true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

// ── Comparisons ───────────────────────────────────────────────────────────────

#[test]
fn test_comparisons() {
    let src = r#"
fn main() {
    println(1 == 1);
    println(1 != 2);
    println(3 < 5);
    println(5 <= 5);
    println(6 > 4);
    println(7 >= 7);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\ntrue\n");
}

#[test]
fn test_comparison_false_cases() {
    let src = r#"
fn main() {
    println(1 == 2);
    println(1 != 1);
    println(3 < 2);
    println(5 <= 4);
    println(6 > 9);
    println(7 >= 8);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\nfalse\nfalse\nfalse\nfalse\nfalse\n");
}

// ── Negative numbers ──────────────────────────────────────────────────────────

#[test]
fn test_unary_neg() {
    let src = r#"
fn main() {
    let x = -42;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "-42");
}

// ── Compile error cases ───────────────────────────────────────────────────────

use super::*;

// Channel/select/lock acceptance matrix (willow-0g8j.2.9), exercised by three
// runnable examples: the two below, and `shared_call_graph` in the test after
// them, where the lock is the point:
//  1 Channel return type               11 default arm
//  2 Channel parameter                 12 timeout arm
//  3 inferred Channel local            13 bounded scheduler drive
//  4 explicitly typed Channel local    14 unbounded blocking wait
//  5 temporary channel expression      15 deadlock panic
//  6 rooted probe-loop channel          16 recover after select panic
//  7 recv readiness                     17 select in a loop
//  8 recv binding                       18 async producer interoperability
//  9 String channel payload            19 lock lowered through typed HIR
// 10 GC during select retry             20 async lock is walker-owned too
#[test]
fn lirreq_channels_select_and_lock_20_perspectives() {
    for (name, source) in [
        (
            "channel_temporaries",
            include_str!("../../../example/channel_temporaries.wi"),
        ),
        (
            "select_blocking",
            include_str!("../../../example/select_blocking.wi"),
        ),
    ] {
        let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
        assert!(ok, "example/{name}.wi must compile: {stderr}");
    }
}

/// Perspective 20 on its own, because it is the case the matrix took longest
/// to reach. `lock` acquires and releases around a body that may suspend, and
/// the protocol used to be AST-owned; since willow-0g8j.2.13 the walker owns it
/// too, so a lock-bearing async body — including the class `Ledger` that holds
/// a `Mutex<i64>` field — compiles from lowered IR like any other.
#[test]
fn lir_async_lock_compiles_from_lir_with_the_expected_output() {
    let source = include_str!("../../../example/shared_call_graph.wi");
    let expected = "11\n12\nrecovered: BadStep has no amount\nafter the recover\n103\n108\n110\n";

    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(ok, "an async body holding a lock must compile: {stderr}");

    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

#[test]
fn lir_diff_select_send_stashes_operands_and_executes_once() {
    assert_program_output(
        r#"
fn mark() -> i64 { println("value"); return 4; }
fn main() {
    let ch = Channel<i64>::with_capacity(1);
    select {
        ch.send(mark()) => { println("sent"); }
        default => { println("default"); }
    }
    println(ch.recv());
}
"#,
        "value\nsent\n4\n",
    );
}

#[test]
fn lir_select_rotates_across_simultaneously_ready_cases() {
    let source = r#"
fn round() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    a.send(1);
    b.send(2);
    select {
        let _ = a.recv() => { println("a"); }
        let _ = b.recv() => { println("b"); }
    }
}
fn main() {
    let mut i = 0;
    while i < 40 { round(); i = i + 1; }
}
"#;
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "forced-LIR fairness run failed: {out}");
    assert!(
        out.lines().any(|line| line == "a"),
        "first case starved: {out}"
    );
    assert!(
        out.lines().any(|line| line == "b"),
        "second case starved: {out}"
    );
}

// Reference-parameter acceptance matrix (willow-0g8j.2.7), exercised by the
// two runnable examples below under forced LIR and compared with AST codegen:
//  1. shared scalar read                 11. array-element place
//  2. mutable scalar write              12. direct-function pointer ABI
//  3. mutable bool write                13. concrete-method pointer ABI
//  4. shared String read                14. interface virtual dispatch
//  5. mutable String replacement        15. inherited interface slot
//  6. collection during shared access   16. default-method forwarding
//  7. collection after GC-value write   17. concrete override dispatch
//  8. shared class-object read          18. two reference arguments
//  9. mutable class-object replacement  19. adjacent by-value argument
// 10. class-field place                 20. runtime-polymorphic helper
#[test]
fn lir_diff_reference_params_20_perspectives() {
    assert_program_output(
        include_str!("../../../example/references.wi"),
        "11\n22\ntrue\nhi!\nhi?\nold box\nold box!\nnew box\n3\n",
    );
}

#[test]
fn lir_diff_interface_reference_params() {
    assert_program_output(
        include_str!("../../../example/interface_reference_params.wi"),
        "15\n20\n15\n75\n45\n5\n25\n<name!>\n6\n1\n6\n11\n18\n105\n300\n",
    );
}

/// A debug build used to keep every `&place` call site on the AST emitter,
/// because a debug build also records the reference-call context a panic
/// reports and only that emitter wrote it. The walker writes it now, so the
/// call site compiles from lowered IR and the program is still right
/// (willow-0g8j.2.17).
#[test]
fn lir_debug_reference_call_is_walker_owned() {
    let source =
        "fn read(n: &i64) -> i64 { return n; } fn main() { let n = 7; println(read(&n)); }";
    assert_program_output(source, "7\n");
}

// Generic-interface acceptance matrix (willow-0g8j.2.8), exercised by the
// runnable examples:
//  1. i64 type argument                 11. parameter typed by instantiation
//  2. String type argument              12. return typed by instantiation
//  3. substituted method return         13. argument typed by instantiation
//  4. substituted method parameter      14. generic interface local
//  5. class-to-interface boxing         15. generic interface return value
//  6. virtual get dispatch              16. GC-managed String result
//  7. virtual replace dispatch          17. mutation through implementation
//  8. two concrete implementations      18. phantom class type argument
//  9. two interface instantiations      19. shared bare-name vtable
// 10. one class implements both         20. calls through helper functions
#[test]
fn lir_diff_generic_interfaces_20_perspectives() {
    assert_program_output(
        include_str!("../../../example/generic_interfaces.wi"),
        "10\nhello\nhello\nworld\n",
    );
    assert_program_output(
        include_str!("../../../example/generic_interface_multi_instantiation.wi"),
        "file\nfile\n",
    );
}

#[test]
fn lir_generic_interface_boxing_roots_field_and_array_owners_under_gc_stress() {
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;

interface Container<T> { fn get(self) -> T; }
class TextBox implements Container<String> {
    pub value: String;
    pub fn get(self) -> String { return self.value; }
}
class Shelf {
    pub item: Container<String>;
    pub fn read(self) -> String { return self.item.get(); }
}

fn exercise(n: i64) -> String {
    let shelf = new Shelf(new TextBox("old"));
    shelf.item = new TextBox("field-" + n.toString());
    let items: Array<Container<String>> = [new TextBox("first")];
    items[0] = new TextBox("array-" + n.toString());
    return shelf.read() + "/" + items[0].get();
}
fn main() {
    let mut n = 0;
    while n < 20 { println(exercise(n)); n = n + 1; }
}
"#,
        &(0..20)
            .map(|n| format!("field-{n}/array-{n}\n"))
            .collect::<String>(),
    );
}

#[test]
fn lir_diff_01_recursion_fib() {
    assert_program_output(
        r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() { println(fib(10)); }
"#,
        "55\n",
    );
}

#[test]
fn lir_diff_02_loops() {
    assert_program_output(
        r#"
fn sum_to(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { t = t + i; }
    while t > 100 { t = t - 100; }
    return t;
}
fn main() { println(sum_to(20)); }
"#,
        "90\n",
    );
}

#[test]
fn lir_diff_03_f64_arithmetic() {
    assert_program_output(
        r#"
fn area(r: f64) -> f64 { return r * r * 3.14159; }
fn big(x: f64) -> bool { return x > 10.0; }
fn main() { println(big(area(2.0))); println(big(area(1.0))); }
"#,
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_04_bool_and_unary() {
    assert_program_output(
        r#"
fn flip(b: bool) -> bool { return !b; }
fn neg(n: i64) -> i64 { return -n; }
fn main() { println(flip(false)); println(neg(-42)); }
"#,
        "true\n42\n",
    );
}

#[test]
fn lir_diff_05_nested_calls() {
    assert_program_output(
        r#"
fn double(n: i64) -> i64 { return n * 2; }
fn add(a: i64, b: i64) -> i64 { return a + b; }
fn main() { println(add(double(3), double(4))); }
"#,
        "14\n",
    );
}

#[test]
fn lir_diff_06_early_returns() {
    assert_program_output(
        r#"
fn sign(n: i64) -> i64 {
    if n > 0 { return 1; }
    if n < 0 { return -1; }
    return 0;
}
fn main() { println(sign(9)); println(sign(-9)); println(sign(0)); }
"#,
        "1\n-1\n0\n",
    );
}

#[test]
fn lir_diff_07_prints_inside_lir_fn() {
    assert_program_output(
        r#"
fn show(n: i64) {
    print(n);
    println(n % 2 == 0);
}
fn main() { show(4); show(7); }
"#,
        "4true\n7false\n",
    );
}

#[test]
fn lir_diff_08_panic_call_chain_names_every_frame() {
    // The panic call-chain must carry the walker-compiled frames: `boom`, and
    // the `outer` that called it.
    let source = r#"
fn boom(n: i64) -> i64 {
    if n > 2 { panic("too big"); }
    return n;
}
fn outer(n: i64) -> i64 { return boom(n + 2); }
fn main() { println(outer(5)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(out.contains("runtime panic: too big at"), "{out}");
    assert!(out.contains("  0: boom at"), "the panicking frame: {out}");
    assert!(out.contains("  1: outer at"), "its caller: {out}");
}

#[test]
fn lir_callstack_static_method_panic_has_its_own_frame() {
    let source = r#"
class Crash {
    pub static fn explode() { panic("static boom"); }
}
fn invoke() { Crash::explode(); }
fn main() { invoke(); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    let method = out
        .find("0: explode")
        .unwrap_or_else(|| panic!("trace has no static-method frame: {out}"));
    let caller = out
        .find("1: invoke")
        .unwrap_or_else(|| panic!("trace has no caller frame: {out}"));
    assert!(method < caller, "trace is out of order: {out}");
}

#[test]
fn lir_callstack_constructor_panic_has_its_own_frame() {
    let source = r#"
class Item {
    pub init(self) { panic("constructor boom"); }
}
fn build() { let item = new Item(); }
fn main() { build(); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    let init = out
        .find("0: init")
        .unwrap_or_else(|| panic!("trace has no constructor frame: {out}"));
    let caller = out
        .find("1: build")
        .unwrap_or_else(|| panic!("trace has no caller frame: {out}"));
    assert!(init < caller, "trace is out of order: {out}");
}

#[test]
fn lir_callstack_constructor_frame_starts_after_argument_evaluation() {
    let source = r#"
fn bad() -> i64 { return 1 / 0; }
class Item {
    pub init(self, value: i64) {}
}
fn build() { let item = new Item(bad()); }
fn main() { build(); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(out.contains("0: bad"), "trace has no bad frame: {out}");
    assert!(
        !out.contains("0: init") && !out.contains("1: init"),
        "trace attributed an argument panic to init: {out}"
    );
}

#[test]
fn lir_diff_09_short_circuit_is_lazy() {
    // With eager evaluation `a / b` would trap on b == 0; `-1` proves the
    // short-circuit skipped the rhs on both paths.
    assert_program_output(
        r#"
fn safe_ratio(a: i64, b: i64) -> i64 {
    return b != 0 && a / b > 2 ? a / b : -1;
}
fn main() {
    println(safe_ratio(10, 2));
    println(safe_ratio(10, 0));
    println(false && true);
    println(true || false);
}
"#,
        "5\n-1\nfalse\ntrue\n",
    );
}

#[test]
fn lir_diff_10_ternary_branches_are_lazy() {
    assert_program_output(
        r#"
fn pick(c: bool, a: i64, b: i64) -> i64 { return c ? a * 2 : b * 3; }
fn main() { println(pick(true, 5, 100)); println(pick(false, 100, 5)); }
"#,
        "10\n15\n",
    );
}

#[test]
fn lir_diff_11_simple_main_compiles_from_lir() {
    // A parameterless void main in the scalar subset takes the LIR path too.
    assert_program_output(
        r#"
fn main() {
    let mut t = 0;
    for i in 1..6 { t = t + i; }
    println(t);
    println(t > 10 && t < 20);
}
"#,
        "15\ntrue\n",
    );
}

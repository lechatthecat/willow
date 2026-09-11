use super::support::compile_and_run;

#[test]
fn lir_constant_folding_preserves_evaluation_and_recovery() {
    let source = r#"
fn base() -> i64 { print("base "); return 2; }
fn divide() {
    defer { match recover() { Some(_) => println("recovered"), None => {} } }
    println(10 / (2 - 2));
}
fn main() {
    println(base() ** (1 + 2));
    println(base() ** (3 - 3));
    println(base() * (3 - 3));
    println(9223372036854775807 + 1);
    println((-7) / (1 + 2));
    println((-7) % (1 + 2));
    println(false && (base() == 2));
    println(true || (base() == 2));
    divide();
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "base 8\nbase 1\nbase 0\n-9223372036854775808\n-2\n-1\nfalse\ntrue\nrecovered\n"
    );
}

#[test]
fn lir_dead_results_preserve_faults_and_call_effects() {
    let (out, ok) = compile_and_run(r#"
fn effect() -> i64 { println("called"); return 2; }
fn discarded(x: i64, zero: i64) {
    defer { match recover() { Some(_) => println("recovered"), None => {} } }
    (x * 2) + 1;
    effect() * 0;
    (x + 1) / zero;
    println("unreachable");
}
fn main() { discarded(5, 0); println("done"); }
"#);
    assert!(ok, "{out}");
    assert_eq!(out, "called\nrecovered\ndone\n");
}

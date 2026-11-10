use super::support::*;

#[test]
fn verified_example_and_signed_boundaries_both_modes() {
    let mut source = include_str!("../../example/static_verification.wi").to_string();
    // Each generated contract is a proof of the corresponding backend result.
    let mut functions = String::new();
    let mut calls = String::new();
    let mut expected = String::new();
    let mut id = 0;
    for x in [i64::MIN, -7, -1, 0, 1, 7, i64::MAX] {
        for y in [-7, -2, -1, 1, 2, 7] {
            if x == i64::MIN && y == -1 {
                continue;
            }
            for (op, result) in [("/", x / y), ("%", x % y)] {
                functions += &format!(
                    "@pure @verified fn boundary{id}()->i64 ensures result=={result} {{return {x}{op}({y});}}\n"
                );
                calls += &format!("println(boundary{id}());\n");
                expected += &format!("{result}\n");
                id += 1;
            }
        }
    }
    source = source.replace("fn main()", "fn example_main()");
    source += &functions.replace("-9223372036854775808", "(-9223372036854775807-1)");
    source += &format!("fn main() {{example_main();{calls}}}");
    for (output, ok) in [compile_and_run(&source), compile_and_run_release(&source)] {
        assert!(ok, "{output}");
        assert_eq!(output, format!("9\n-3\n-1\n{expected}"));
    }
}
#[test]
fn verified_rejections_reach_build_gate() {
    for (source, reason) in [
        ("@verified fn f()->i64 {return 1;}", "@pure"),
        (
            "@pure @verified fn f(x:i64)->i64 ensures result>x {return x;}",
            "COUNTEREXAMPLE",
        ),
        (
            "@pure @verified fn f()->i64 requires false {return 1;}",
            "VACUOUS_PRECONDITION",
        ),
        (
            "@pure @verified fn f()->i64 {while false {} return 1;}",
            "UNKNOWN",
        ),
        ("@pure @verified fn f(x:i64)->i64 {return f(x);}", "UNKNOWN"),
        (
            "@pure @verified fn f(x:i64)->i64 {return x+1;}",
            "COUNTEREXAMPLE",
        ),
        (
            "@pure @verified fn f()->i64 {return (-9223372036854775807-1)%-1;}",
            "COUNTEREXAMPLE",
        ),
    ] {
        assert_compile_error_contains(&format!("{source} fn main() {{}}"), &[reason]);
    }
}
#[test]
fn verified_counterexample_reexecutes_as_runtime_violation() {
    let source =
        "@pure fn wrong(x:i64)->i64 ensures result>x {return x;} fn main() {println(wrong(0));}";
    for (output, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(!ok, "{output}");
    }
}

#[test]
fn verified_methods_and_generic_templates_reject_without_panicking() {
    for source in [
        "class C {@pure @verified pub fn f(self)->i64 {return 1;}}",
        "interface I {@verified fn f(self)->i64;}",
        "@pure @verified fn f<T>(x:i64)->i64 {return x;}",
        "@verified @pure fn f(x:i64)->i64 {return x+1;}",
        "@pure @verified fn f()->i64 {println(1);return 1;}",
    ] {
        assert_compile_error_contains(&format!("{source} fn main() {{}}"), &["error"]);
    }
}

#[test]
fn verified_unknown_and_artifact_failure_stop_cli() {
    let id = unique_test_id();
    let path = temp_path(format!("verification_unknown_{id}.wi"));
    let artifacts = temp_path(format!("verification_unknown_{id}"));
    fs::write(&path,"@pure @verified fn f(x:i64)->i64 requires x>=0 requires x<100 ensures result==x*x {return x*x;} fn main() {}").unwrap();
    for (key, value) in [
        ("WILLOW_VERIFICATION_RLIMIT", "1"),
        ("WILLOW_VERIFICATION_TIMEOUT_MS", "invalid"),
        ("WILLOW_Z3", "missing-willow-solver-executable"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["check", &path])
            .env(key, value)
            .env("WILLOW_VERIFICATION_DIR", &artifacts)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("E2632"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = fs::remove_dir_all(&artifacts);
    fs::write(&artifacts, "not a directory").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["check", &path])
        .env("WILLOW_VERIFICATION_DIR", &artifacts)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("E2632"));
    fs::remove_file(&artifacts).unwrap();
    fs::remove_file(&path).unwrap();
}

#[test]
fn verified_diagnostic_codes_preserve_integer_exponent_error() {
    for (source, code) in [
        (
            "@pure @verified fn f(x:i64)->i64 ensures result>x {return x;}",
            "E2630",
        ),
        (
            "@pure @verified fn f()->i64 requires false {return 1;}",
            "E2630",
        ),
        (
            "@pure @verified fn f()->i64 {while false {} return 1;}",
            "E2631",
        ),
        ("@verified fn f()->i64 {return 1;}", "E2631"),
        (
            "class C {@verified pub fn f(self)->i64 {return 1;}}",
            "E2631",
        ),
        ("fn f()->i64 {return 2 ** -1;}", "E0204"),
    ] {
        assert_compile_error_contains(&format!("{source} fn main() {{}}"), &[code]);
    }
}

#[test]
fn contract_repair_example_runs() {
    let source = include_str!("../../example/contract_repair.wi");
    let (output, ok) = compile_and_run(source);
    assert!(ok, "{output}");
    assert_eq!(output, "0\n7\n");
}

#[test]
fn verified_loop_example_both_modes() {
    let source = include_str!("../../example/verified_loops.wi");
    for (output, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(ok, "{output}");
        assert_eq!(output, "0\n0\n7\n");
    }
}

#[test]
fn verified_loop_rejections_reach_build_gate() {
    for (annotations, body, reason) in [
        ("invariant x>=0 variant 1", "x=x-1;", "COUNTEREXAMPLE"),
        ("invariant x>0 variant x", "x=x-1;", "COUNTEREXAMPLE"),
        ("invariant x>=0 variant x", "x=x-2;", "COUNTEREXAMPLE"),
        ("invariant x>=0 variant -x", "x=x-1;", "COUNTEREXAMPLE"),
        ("invariant x>=0", "x=x-1;", "UNKNOWN"),
        ("variant x", "x=x-1;", "UNKNOWN"),
        ("", "x=x-1;", "UNKNOWN"),
        ("invariant x>=0 variant true", "x=x-1;", "loop variant"),
        ("invariant 1 variant x", "x=x-1;", "loop invariant"),
    ] {
        assert_compile_error_contains(
            &format!(
                "@pure @verified fn f(n:i64)->i64 requires n>=0 {{let mut x=n;while x>0 {annotations} {{{body}}}return x;}} fn main() {{}}"
            ),
            &[reason],
        );
    }
}

#[test]
fn verified_immutable_data_example_both_modes() {
    let source = include_str!("../../example/verified_data.wi");
    for (output, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(ok, "{output}");
        assert_eq!(output, "4\n7\n0\n3\n3\ntrue\ntrue\nfalse\ntrue\n");
    }
}

#[test]
fn verified_quantifier_runtime_short_circuit_and_bounds() {
    let source = r#"
fn main() {
    println(forall(0, 3, |i:i64| i == 0 ? false : 1 / 0 > 0));
    println(exists(0, 3, |i:i64| i == 0 ? true : 1 / 0 > 0));
    println(forall(9223372036854775806, 9223372036854775807, |i:i64| i == 9223372036854775806));
    println(exists(1, 1, |i:i64| 1 / 0 > 0));
    let iteration = 9;
    let a = [1,2].freeze();
    let mut total = 0;
    for value in a invariant total == iteration * 9 variant a.len() - iteration {
        total = total + iteration;
    }
    println(total);
}
"#;
    for (output, ok) in [compile_and_run(source), compile_and_run_release(source)] {
        assert!(ok, "{output}");
        assert_eq!(output, "false\ntrue\ntrue\nfalse\n18\n");
    }
}

#[test]
fn verified_immutable_data_fail_closed_build_gate() {
    for (source, code) in [
        (
            "@pure @verified fn f(a:Array<i64>)->i64{return a[0];}",
            "E2631",
        ),
        (
            "class C {pub x:i64;} fn write(c:C){c.x=2;} @pure @verified fn f(c:C)->i64{return c.x;}",
            "E2631",
        ),
        (
            "@pure @verified fn f(a:FrozenArray<i64>)->i64{return a[0];}",
            "E2630",
        ),
        (
            "@pure @verified fn f()->bool ensures result {return forall(0,3,|i:i64|forall(0,3,|j:i64|i==j));}",
            "E2630",
        ),
    ] {
        assert_compile_error_contains(&format!("{source} fn main(){{}}"), &[code]);
    }
}

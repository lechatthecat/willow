//! Contract perspectives: entry, exit, multiple clauses, early/implicit returns,
//! result scope, parameter shadowing, signed division, short-circuit &&/||,
//! ternary, checked +,-,*,/,%,**,negation,shift, constants, caller recovery,
//! cleanup order, generic/async functions, and unchanged contract-free modes.
use super::support::*;

fn run(source: &str, release: bool) -> (String, String, bool) {
    let id = unique_test_id();
    let path = temp_path(format!("contracts_{id}.wi"));
    let bin = temp_path(format!("contracts_{id}"));
    fs::write(&path, source).unwrap();
    let mut args = vec!["build", &path, "-o", &bin];
    if release {
        args.push("--release");
    }
    let built = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(args)
        .output()
        .unwrap();
    let _ = fs::remove_file(path);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = Command::new(&bin).output().unwrap();
    remove_output_artifacts(&bin);
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn contracts_both_modes_recovery_matrix() {
    let cases = [
        (
            "ensures result == 0 || 10 / result > 0",
            "return x;",
            "0",
            "ok",
        ),
        (
            "ensures result != 0 && 10 / result > 0",
            "return x;",
            "0",
            "contract violation: ensures",
        ),
        (
            "requires x > 0",
            "return x;",
            "0",
            "contract violation: requires",
        ),
        (
            "ensures result > 0",
            "return x;",
            "0",
            "contract violation: ensures",
        ),
        (
            "requires true requires x > 0",
            "return x;",
            "0",
            "contract violation: requires",
        ),
        (
            "ensures true ensures result > 0",
            "return x;",
            "0",
            "contract violation: ensures",
        ),
        (
            "requires x != 0 && 10 / x > 0",
            "return x;",
            "0",
            "contract violation: requires",
        ),
        ("requires x == 0 || 10 / x > 0", "return x;", "0", "ok"),
        (
            "requires x == 0 ? true : 10 / x > 0",
            "return x;",
            "0",
            "ok",
        ),
        (
            "requires x * 9 / 10 > 0",
            "return x;",
            "9223372036854775807",
            "evaluation",
        ),
        (
            "requires x + 1 > 0",
            "return x;",
            "9223372036854775807",
            "evaluation",
        ),
        (
            "requires x - 1 < 0",
            "return x;",
            "(0 - 9223372036854775807 - 1)",
            "evaluation",
        ),
        (
            "requires -x > 0",
            "return x;",
            "(0 - 9223372036854775807 - 1)",
            "evaluation",
        ),
        ("requires 10 / x > 0", "return x;", "0", "evaluation"),
        ("requires 10 % x == 0", "return x;", "0", "evaluation"),
        (
            "requires x / -1 > 0",
            "return x;",
            "(0 - 9223372036854775807 - 1)",
            "evaluation",
        ),
        (
            "requires x % -1 == 0",
            "return x;",
            "(0 - 9223372036854775807 - 1)",
            "evaluation",
        ),
        (
            "requires x ** 2 > 0",
            "return x;",
            "9223372036854775807",
            "evaluation",
        ),
        ("requires 2 ** x > 0", "return x;", "-1", "evaluation"),
        ("requires 1 << x > 0", "return x;", "64", "evaluation"),
        ("requires 1 >> x > 0", "return x;", "-1", "evaluation"),
        (
            "ensures result * 9 / 10 > 0",
            "return x;",
            "9223372036854775807",
            "evaluation",
        ),
        (
            "requires x / 2 == -3 && x % 2 == -1",
            "return x;",
            "-7",
            "ok",
        ),
        (
            "requires (-2) ** 63 == x",
            "return x;",
            "(0 - 9223372036854775807 - 1)",
            "ok",
        ),
        (
            "ensures result == x",
            "if x < 0 { return x; } return x;",
            "-2",
            "ok",
        ),
        (
            "ensures result == x",
            "let x = 99; return x;",
            "2",
            "contract violation: ensures",
        ),
        (
            "requires true || 9223372036854775807 + 1 > 0",
            "return x;",
            "0",
            "ok",
        ),
        (
            "requires 9223372036854775807 + 1 > 0",
            "return x;",
            "0",
            "evaluation",
        ),
    ];
    let mut source = String::new();
    let mut body = String::new();
    let mut expected = String::new();
    for (i, (clauses, code, arg, outcome)) in cases.iter().enumerate() {
        source.push_str(&format!("fn f{i}(x:i64)->i64 {clauses} {{ {code} }}\n"));
        body.push_str(&format!("if true {{ defer match recover() {{ Some(info) => println(info.message), None => {{}} }} let ignored = f{i}({arg}); println(\"ok\"); }}\n"));
        expected.push_str(if *outcome == "evaluation" {
            "contract evaluation error: invalid or overflowing i64 arithmetic"
        } else {
            outcome
        });
        expected.push('\n');
    }
    source.push_str(&format!("fn main() {{ {body} }}"));
    for release in [false, true] {
        let (out, err, ok) = run(&source, release);
        assert!(ok, "{out}\n{err}");
        assert_eq!(out, expected, "release={release}, {err}");
    }
}

#[test]
fn contracts_cleanup_void_generic_and_async() {
    let source = r#"
const LIMIT:i64 = 10;
fn f<T>(x:i64, value:T)->i64 requires x < LIMIT ensures result == x {
 defer println("cleanup"); return x;
}
fn empty() ensures false {}
async fn asynchronous(x:i64)->i64 requires x > 0 ensures result > x { return x + 1; }
async fn main() {
 println(f(3, true));
 if true { defer match recover() { Some(info) => println(info.message), None => {} } empty(); }
 println(await asynchronous(4));
}
"#;
    for release in [false, true] {
        let (out, err, ok) = run(source, release);
        assert!(ok, "{out}\n{err}");
        assert_eq!(out, "cleanup\n3\ncontract violation: ensures\n5\n");
    }
}

#[test]
fn contracts_uncaught_error_has_location_in_both_modes() {
    for release in [false, true] {
        let (out, err, ok) = run("fn f() requires false {} fn main() { f(); }", release);
        assert!(!ok, "{out}");
        assert!(err.contains("contract violation: requires"), "{err}");
        assert!(err.contains(".wi:1:"), "{err}");
    }
}

#[test]
fn contracts_void_return_and_exception_cleanup() {
    let source = r#"
fn say() { println("evaluated"); }
fn value()->i64 { println("once"); return 1; }
fn returned()->i64 ensures result == 1 { return value(); }
fn unit() ensures true { return say(); }
fn fault()->i64 ensures false { defer println("cleanup"); panic("body panic"); }
fn main() {
 unit();
 println(returned());
 if true { defer match recover() { Some(info) => println(info.message), None => {} } fault(); }
}
"#;
    for release in [false, true] {
        let (out, err, ok) = run(source, release);
        assert!(ok, "{out}\n{err}");
        assert_eq!(out, "evaluated\nonce\n1\ncleanup\nbody panic\n");
    }
}

#[test]
fn contracts_imports_and_cache_revisions() {
    let project = TestProject::new(
        "contracts_modules",
        &[
            ("limits.wi", "pub const LIMIT:i64 = 4;"),
            (
                "lib.wi",
                "import limits; pub fn f(x:i64)->i64 requires x < limits::LIMIT ensures result == x { return x; }",
            ),
            (
                "main.wi",
                "import lib; fn main() { if true { defer match recover() { Some(info) => println(info.message), None => {} } println(lib::f(3)); } }",
            ),
        ],
    );
    for (clause, expected) in [
        ("x < limits::LIMIT", "3\n"),
        ("false", "contract violation: requires\n"),
        ("true", "3\n"),
        ("", "3\n"),
        ("false", "contract violation: requires\n"),
        ("", "3\n"),
    ] {
        let clauses = if clause.is_empty() {
            String::new()
        } else {
            format!("requires {clause} ensures result == x")
        };
        project.write_file(
            "lib.wi",
            &format!("import limits; pub fn f(x:i64)->i64 {clauses} {{ return x; }}"),
        );
        for _ in 0..2 {
            let built = project.compile("main.wi");
            assert!(
                built.status.success(),
                "{}",
                String::from_utf8_lossy(&built.stderr)
            );
            let out = project.run_with_env_timeout(&[], std::time::Duration::from_secs(15));
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
        }
    }
}

#[test]
fn contracts_example_runs() {
    for release in [false, true] {
        let (out, err, ok) = run(include_str!("../../example/runtime_contracts.wi"), release);
        assert!(ok, "{out}\n{err}");
        assert_eq!(out, "90\ncontract violation: requires\nrecovered\n");
    }
}

#[test]
fn contracts_native_check_sites_do_not_multiply_with_returns() {
    for (exits, clauses) in [(1, 1), (8, 1), (8, 8), (32, 8)] {
        let mut source = "fn f(x:i64)->i64 ".to_owned();
        for i in 0..clauses {
            source.push_str(&format!("ensures result + {i} >= x "));
        }
        source.push('{');
        for i in 0..exits {
            source.push_str(&format!("if x == {i} {{ return x; }}"));
        }
        source.push_str("return x; } fn main() { println(f(0)); }");
        let targets = compile_and_collect_relocation_targets_all(&source, &[]);
        for symbol in ["willow_contract_check", "willow_contract_i64"] {
            let calls = targets
                .iter()
                .filter(|target| target.ends_with(symbol))
                .count();
            assert_eq!(
                calls, clauses,
                "exits={exits}, clauses={clauses}: {targets:?}"
            );
            println!("native exits={exits} clauses={clauses} {symbol} sites={calls}");
        }
    }
}

#[test]
fn contracts_async_failure_matches_ordinary_task_panic() {
    let source = r#"
async fn f()->i64 ensures false { await yield(); return 1; }
async fn main() {
 if true { defer match recover() { Some(info) => println(info.message), None => {} } println(await f()); }
 println("after");
}
"#;
    for release in [false, true] {
        let (out, err, ok) = run(source, release);
        assert!(!ok, "{out}\n{err}");
        assert!(err.contains("contract violation: ensures"), "{err}");
        let ordinary = source
            .replace("ensures false", "")
            .replace("return 1;", "panic(\"ordinary task panic\"); return 1;");
        let (out, err, ok) = run(&ordinary, release);
        assert!(!ok, "{out}\n{err}");
        assert!(err.contains("ordinary task panic"), "{err}");
    }
}

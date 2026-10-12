use super::*;

// ── Which functions are function values (willow-yf49) ───────────────────────
//
// A named function used as a value compiles to the address of its symbol. An
// item-imported function has one too, under the name its import binds; an
// async function (a call creates its Task) and a builtin lowered inline at
// each call have none, so the checker rejects them with E1012 instead of
// leaving code generation to fail. `pow`/`powf` are real functions.
//
// 23 perspectives:
//   1 local function value (baseline)      13 E1012: imported async function
//   2 imported value bound to a local      14 E1012: builtin `sleep`
//   3 imported value to imported apply     15 E1012: zero-parameter builtin
//   4 imported value to local apply        16 `pow`/`powf` stay values
//   5 imported value returned from a fn    17 E1012: async passed as argument
//   6 imported String function value      18 E1012 alone, no follow-on error
//   7 imported zero-parameter value        19 async call through a lambda runs
//   8 imported value to Option.map         20 builtin through a lambda runs
//   9 imported value in an async frame     21 zero-arg builtin through a lambda
//  10 same name in two units               22 direct async/builtin calls stay
//  11 imported value inside a library      23 imported value under a lock
//  12 E1012: local async function

const WORKER: &str = "pub fn twice(x: i64) -> i64 { return x * 2; }\n\
pub fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }\n\
pub fn shout(s: String) -> String { return s + \"!\"; }\n\
pub fn seven() -> i64 { return 7; }\n\
pub async fn later(x: i64) -> i64 { await sleep(1); return x; }\n";

fn run_project(name: &str, files: &[(&str, &str)]) -> (String, String) {
    let project = TestProject::new(name, files);
    let compiled = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&compiled.stderr).into_owned();
    assert!(compiled.status.success(), "{name}: {stderr}");
    let output = project.run();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{name}: {stderr}");
    (String::from_utf8_lossy(&output.stdout).into_owned(), stderr)
}

fn run_with_worker(name: &str, main: &str) -> String {
    run_project(name, &[("worker.wi", WORKER), ("main.wi", main)]).0
}

fn rejected_e1012(name: &str, files: &[(&str, &str)]) -> String {
    let project = TestProject::new(name, files);
    let output = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{name}: expected E1012");
    assert!(stderr.contains("error[E1012]"), "{name}: {stderr}");
    assert!(!stderr.contains("E0800"), "{name}: {stderr}");
    stderr
}

#[test]
fn fn_value_target_01_local_function() {
    let (out, ok) = compile_and_run(
        "fn twice(x: i64) -> i64 { return x * 2; }\nfn main() { let f = twice; println(f(21)); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn fn_value_target_02_imported_value_bound_to_local() {
    let out = run_with_worker(
        "fvt02",
        "import worker::twice;\nfn main() { let f = twice; println(f(21)); }\n",
    );
    assert_eq!(out, "42\n");
}

#[test]
fn fn_value_target_03_imported_value_to_imported_apply() {
    let out = run_with_worker(
        "fvt03",
        "import worker::twice;\nimport worker::apply;\nfn main() { println(apply(twice, 21)); }\n",
    );
    assert_eq!(out, "42\n");
}

#[test]
fn fn_value_target_04_imported_value_to_local_apply() {
    let out = run_with_worker(
        "fvt04",
        "import worker::twice;\nfn local(f: fn(i64) -> i64) -> i64 { return f(5) + 1; }\nfn main() { println(local(twice)); }\n",
    );
    assert_eq!(out, "11\n");
}

#[test]
fn fn_value_target_05_imported_value_returned() {
    let out = run_with_worker(
        "fvt05",
        "import worker::twice;\nfn pick() -> fn(i64) -> i64 { return twice; }\nfn main() { let f = pick(); println(f(4)); }\n",
    );
    assert_eq!(out, "8\n");
}

#[test]
fn fn_value_target_06_imported_string_function() {
    let out = run_with_worker(
        "fvt06",
        "import worker::shout;\nfn main() { let f: fn(String) -> String = shout; println(f(\"hi\")); }\n",
    );
    assert_eq!(out, "hi!\n");
}

#[test]
fn fn_value_target_07_imported_zero_parameter_value() {
    let out = run_with_worker(
        "fvt07",
        "import worker::seven;\nfn main() { let f = seven; println(f() + f()); }\n",
    );
    assert_eq!(out, "14\n");
}

#[test]
fn fn_value_target_08_imported_value_to_option_map() {
    let out = run_with_worker(
        "fvt08",
        "import worker::twice;\nfn main() { println(Option::Some(9).map(twice).unwrap_or(0)); }\n",
    );
    assert_eq!(out, "18\n");
}

#[test]
fn fn_value_target_09_imported_value_in_async_frame() {
    let out = run_with_worker(
        "fvt09",
        "import worker::twice;\nasync fn run(f: fn(i64) -> i64) -> i64 { await sleep(1); return f(3); }\nasync fn main() { let f = twice; println(await run(f)); }\n",
    );
    assert_eq!(out, "6\n");
}

/// Each unit's `twice` value is that unit's own function.
#[test]
fn fn_value_target_10_same_name_in_two_units() {
    let (out, _) = run_project(
        "fvt10",
        &[
            ("worker.wi", WORKER),
            (
                "other.wi",
                "fn twice(x: i64) -> i64 { return x * 3; }\npub fn run(x: i64) -> i64 { let f = twice; return f(x); }\n",
            ),
            (
                "main.wi",
                "import worker::twice;\nimport other;\nfn main() { let f = twice; println(f(10)); println(other::run(10)); }\n",
            ),
        ],
    );
    assert_eq!(out, "20\n30\n");
}

#[test]
fn fn_value_target_11_imported_value_inside_library() {
    let (out, _) = run_project(
        "fvt11",
        &[
            ("worker.wi", WORKER),
            (
                "lib.wi",
                "import worker::twice;\nimport worker::apply;\npub fn go(x: i64) -> i64 { return apply(twice, x); }\n",
            ),
            (
                "main.wi",
                "import lib;\nfn main() { println(lib::go(8)); }\n",
            ),
        ],
    );
    assert_eq!(out, "16\n");
}

#[test]
fn fn_value_target_12_local_async_function_rejected() {
    let out = rejected_e1012(
        "fvt12",
        &[(
            "main.wi",
            "async fn work(x: i64) -> i64 { await sleep(1); return x + 1; }\nasync fn main() { let f = work; println(1); }\n",
        )],
    );
    assert!(
        out.contains("`work` is an async function and cannot be used as a function value"),
        "{out}"
    );
    assert!(out.contains("`|a0: i64| work(a0)`"), "{out}");
}

#[test]
fn fn_value_target_13_imported_async_function_rejected() {
    let out = rejected_e1012(
        "fvt13",
        &[
            ("worker.wi", WORKER),
            (
                "main.wi",
                "import worker::later;\nasync fn main() { let f = later; println(1); }\n",
            ),
        ],
    );
    assert!(out.contains("`later` is an async function"), "{out}");
}

#[test]
fn fn_value_target_14_builtin_sleep_rejected() {
    let out = rejected_e1012(
        "fvt14",
        &[(
            "main.wi",
            "async fn main() { let s = sleep; await s(1); }\n",
        )],
    );
    assert!(
        out.contains("`sleep` is a builtin function and cannot be used as a function value"),
        "{out}"
    );
    assert!(out.contains("`|a0: i64| sleep(a0)`"), "{out}");
}

#[test]
fn fn_value_target_15_zero_parameter_builtin_rejected() {
    let out = rejected_e1012(
        "fvt15",
        &[("main.wi", "fn main() { let g = gc_collect; g(); }\n")],
    );
    assert!(out.contains("`|| gc_collect()`"), "{out}");
}

/// `pow` and `powf` are defined as real functions, so they remain values.
#[test]
fn fn_value_target_16_pow_builtins_stay_values() {
    let (out, ok) = compile_and_run(
        "fn main() { let p = pow; let q: fn(f64, f64) -> f64 = powf; println(p(2.0, 3.0) == q(2.0, 3.0)); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fn_value_target_17_async_passed_as_argument_rejected() {
    let out = rejected_e1012(
        "fvt17",
        &[(
            "main.wi",
            "async fn work(x: i64) -> i64 { return x; }\nfn apply(f: fn(i64) -> Task<i64>, x: i64) -> Task<i64> { return f(x); }\nasync fn main() { println(await apply(work, 1)); }\n",
        )],
    );
    assert!(out.contains("`work` is an async function"), "{out}");
}

/// The rejected value keeps the call's type, so awaiting its result reports
/// nothing further.
#[test]
fn fn_value_target_18_rejection_has_no_follow_on_errors() {
    let out = rejected_e1012(
        "fvt18",
        &[(
            "main.wi",
            "async fn work(x: i64) -> i64 { return x; }\nasync fn main() { let f = work; let t = f(1); println(await t); }\n",
        )],
    );
    assert_eq!(out.matches("error[").count(), 1, "{out}");
}

#[test]
fn fn_value_target_19_async_call_through_lambda_runs() {
    let (out, ok) = compile_and_run(
        "async fn work(x: i64) -> i64 { await sleep(1); return x + 1; }\nasync fn main() { let f = |a0: i64| work(a0); println(await f(41)); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn fn_value_target_20_builtin_through_lambda_runs() {
    let (out, ok) = compile_and_run(
        "async fn main() { let s = |a0: i64| sleep(a0); await s(1); let p = |a0: f64, a1: f64| pow(a0, a1); println(p(2.0, 3.0) == 8.0); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fn_value_target_21_zero_argument_builtin_through_lambda_runs() {
    let (out, ok) =
        compile_and_run("fn main() { let g = || gc_allocated_bytes(); println(g() >= 0); }\n");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fn_value_target_22_direct_async_and_builtin_calls_unaffected() {
    let (out, ok) = compile_and_run(
        "async fn work(x: i64) -> i64 { await sleep(1); return x; }\nasync fn main() { println(await work(5)); gc_collect(); println(pow(2.0, 2.0) == 4.0); }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\ntrue\n");
}

#[test]
fn fn_value_target_23_imported_value_under_lock() {
    let out = run_with_worker(
        "fvt23",
        "import worker::twice;\nimport worker::apply;\nasync fn main() { let m = Mutex::new(20); lock m as v { println(apply(twice, v + 1)); } }\n",
    );
    assert_eq!(out, "42\n");
}

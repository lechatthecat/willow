//! E2604 through function values (willow-h4t7): a call through a `fn` or
//! `closure` value has no static target, so a lock-held indirect call is
//! rejected when any function value with the same parameter count may wait.
use super::*;

const WAITING: &str = "fn helper(ch: Channel<i64>) -> i64 { return ch.recv(); }\n";
const VALUE_WITNESS: &str = "this function value may wait and has a matching parameter count";
const IMPORTED_LOCK: &str = "a waiting function value can be called while holding a Willow lock";

fn check(name: &str, files: &[(&str, &str)]) -> (String, bool) {
    let project = TestProject::new(name, files);
    let output = project.compile("main.wi");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

/// Type-check only, for shapes whose code generation is tracked separately.
fn checked(name: &str, files: &[(&str, &str)]) -> (String, bool) {
    let project = TestProject::new(name, files);
    let output = project.check("main.wi");
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        output.status.success(),
    )
}

fn rejected(name: &str, source: &str) -> String {
    let (diagnostics, ok) = check(name, &[("main.wi", source)]);
    assert!(!ok, "{name}: expected E2604");
    assert!(
        diagnostics.contains("error[E2604]"),
        "{name}: {diagnostics}"
    );
    diagnostics
}

fn accepted(name: &str, source: &str) {
    let (diagnostics, ok) = check(name, &[("main.wi", source)]);
    assert!(ok, "{name}: {diagnostics}");
    assert!(!diagnostics.contains("E2604"), "{name}: {diagnostics}");
}

fn locked(prelude: &str, body: &str) -> String {
    format!(
        "{prelude}async fn main() {{ let m = Mutex::new(0); let ch = Channel<i64>::new(); lock m as v {{ {body} }} }}\n"
    )
}

/// Perspective 1: a local holding a waiting named function, called under the
/// lock, is rejected at the call with the value as the witness.
#[test]
fn indirect_lock_01_direct_local_call() {
    let out = rejected("ind01", &locked(WAITING, "let op = helper; op(ch);"));
    assert!(
        out.contains("cannot call a waiting function value while holding a Willow lock"),
        "{out}"
    );
    assert!(out.contains(VALUE_WITNESS), "{out}");
    assert!(
        out.contains("call the function value outside the critical section"),
        "{out}"
    );
}

/// Perspective 2: the same shape with a function value that cannot wait is
/// accepted.
#[test]
fn indirect_lock_02_non_waiting_local_call_accepted() {
    accepted(
        "ind02",
        &locked(
            "fn inc(x: i64) -> i64 { return x + 1; }\n",
            "let op = inc; println(op(1));",
        ),
    );
}

/// Perspective 3: a lock-held call to a Willow helper that calls its parameter
/// reaches the waiting value through the call graph.
#[test]
fn indirect_lock_03_helper_parameter_call() {
    rejected(
        "ind03",
        &locked(
            &format!(
                "{WAITING}fn apply(f: fn(Channel<i64>) -> i64, ch: Channel<i64>) -> i64 {{ return f(ch); }}\n"
            ),
            "apply(helper, ch);",
        ),
    );
}

/// Perspective 4: reach is transitive across a chain of synchronous helpers.
#[test]
fn indirect_lock_04_transitive_helper_chain() {
    rejected(
        "ind04",
        &locked(
            &format!(
                "{WAITING}fn inner(f: fn(Channel<i64>) -> i64, ch: Channel<i64>) -> i64 {{ return f(ch); }}\nfn outer(f: fn(Channel<i64>) -> i64, ch: Channel<i64>) -> i64 {{ return inner(f, ch); }}\n"
            ),
            "outer(helper, ch);",
        ),
    );
}

/// Perspective 5: helpers that only reach non-waiting values stay accepted.
#[test]
fn indirect_lock_05_helper_with_pure_values_accepted() {
    accepted(
        "ind05",
        &locked(
            "fn inc(x: i64) -> i64 { return x + 1; }\nfn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }\n",
            "let op = inc; println(apply(op, 1)); println(op(2));",
        ),
    );
}

/// Perspective 6: a waiting lambda handed to a builtin that calls it (`map`)
/// under the lock is rejected.
#[test]
fn indirect_lock_06_waiting_lambda_through_builtin() {
    rejected(
        "ind06",
        &locked(
            "",
            "println(Option::Some(7).map(|x: i64| { let c = Channel<i64>::new(); return x + c.recv(); }).unwrap_or(0));",
        ),
    );
}

/// Perspective 7: a pure lambda handed to the same builtin is accepted.
#[test]
fn indirect_lock_07_pure_lambda_through_builtin_accepted() {
    accepted(
        "ind07",
        &locked(
            "",
            "println(Option::Some(7).map(|x: i64| x * 2).unwrap_or(0));",
        ),
    );
}

/// Perspective 8: a waiting lambda stored in a local and called under the lock
/// is rejected.
#[test]
fn indirect_lock_08_waiting_lambda_local_call() {
    rejected(
        "ind08",
        "async fn main() { let m = Mutex::new(0); let ch = Channel<i64>::new(); let f = |c: Channel<i64>| c.recv(); lock m as v { f(ch); } }\n",
    );
}

/// Perspective 9: calling an async function value only creates a Task, so it
/// never waits in the critical section.
#[test]
fn indirect_lock_09_async_function_value_accepted() {
    let (out, _) = checked(
        "ind09",
        &[(
            "main.wi",
            "async fn work(x: i64) -> i64 { await sleep(1); return x; }\nasync fn main() { let m = Mutex::new(0); let f = work; lock m as v { let t = f(1); } }\n",
        )],
    );
    assert!(!out.contains("E2604"), "{out}");
}

/// Perspective 10: a builtin used as a value (`sleep`) creates a Future and is
/// accepted.
#[test]
fn indirect_lock_10_builtin_value_accepted() {
    let (out, _) = checked(
        "ind10",
        &[(
            "main.wi",
            "async fn main() { let m = Mutex::new(0); let s = sleep; lock m as v { s(1); } }\n",
        )],
    );
    assert!(!out.contains("E2604"), "{out}");
}

/// Perspective 11: an indirect call can only reach values with its own
/// parameter count; a waiting two-parameter value does not taint a
/// one-parameter call.
#[test]
fn indirect_lock_11_arity_mismatch_accepted() {
    accepted(
        "ind11",
        &locked(
            "fn slow(a: Channel<i64>, b: i64) -> i64 { return a.recv() + b; }\nfn inc(x: i64) -> i64 { return x + 1; }\n",
            "let w = slow; let op = inc; println(op(1));",
        ),
    );
}

/// Perspective 12: a function value returned from another function is still
/// recorded where it is made.
#[test]
fn indirect_lock_12_returned_function_value() {
    rejected(
        "ind12",
        "fn make() -> fn(i64) -> i64 { return slow; }\nfn slow(x: i64) -> i64 { let ch = Channel<i64>::new(); return ch.recv(); }\nasync fn main() { let m = Mutex::new(0); let f = make(); lock m as v { println(f(1)); } }\n",
    );
}

/// Perspective 13: a value that waits only through its own indirect call is
/// conditional, and becomes waiting once the arity it calls holds a waiting
/// value.
#[test]
fn indirect_lock_13_conditional_value_closes() {
    rejected(
        "ind13",
        "fn slow() -> i64 { let ch = Channel<i64>::new(); return ch.recv(); }\nfn relay(g: fn() -> i64) -> i64 { return g(); }\nasync fn main() { let m = Mutex::new(0); let s = slow; let r = relay; lock m as v { println(r(s)); } }\n",
    );
}

/// Perspective 14: the same conditional value is accepted when nothing of the
/// arity it calls waits.
#[test]
fn indirect_lock_14_conditional_value_without_wait_accepted() {
    accepted(
        "ind14",
        "fn one() -> i64 { return 1; }\nfn relay(g: fn() -> i64) -> i64 { return g(); }\nasync fn main() { let m = Mutex::new(0); let s = one; let r = relay; lock m as v { println(r(s)); } }\n",
    );
}

/// Perspective 15: the waiting value outside the critical section is fine.
#[test]
fn indirect_lock_15_call_outside_lock_accepted() {
    accepted(
        "ind15",
        &format!(
            "{WAITING}async fn main() {{ let m = Mutex::new(0); let ch = Channel<i64>::with_capacity(1); ch.send(1); let op = helper; lock m as v {{ println(v); }} println(op(ch)); }}\n"
        ),
    );
}

/// Perspective 16: read locks follow the same rule.
#[test]
fn indirect_lock_16_read_lock() {
    rejected(
        "ind16",
        &format!(
            "{WAITING}async fn main() {{ let m = RwLock::new(0); let ch = Channel<i64>::new(); let op = helper; lock read m as v {{ op(ch); }} }}\n"
        ),
    );
}

/// Perspective 17: write locks follow the same rule.
#[test]
fn indirect_lock_17_write_lock() {
    rejected(
        "ind17",
        &format!(
            "{WAITING}async fn main() {{ let m = RwLock::new(0); let ch = Channel<i64>::new(); let op = helper; lock write m as v {{ op(ch); }} }}\n"
        ),
    );
}

/// Perspective 18: a lambda nested inside the critical section that calls a
/// waiting value is itself a waiting value; handing it to `map` is rejected.
#[test]
fn indirect_lock_18_nested_lambda_under_lock() {
    rejected(
        "ind18",
        &locked(
            WAITING,
            "let op = helper; println(Option::Some(ch).map(|c: Channel<i64>| op(c)).unwrap_or(0));",
        ),
    );
}

/// Perspective 19: a critical section in an imported module that calls a
/// function value is reported at the waiting value made by the importer.
#[test]
fn indirect_lock_19_imported_critical_section_reported_at_value() {
    let (out, ok) = check(
        "ind19",
        &[
            (
                "worker.wi",
                "pub async fn run(f: fn(Channel<i64>) -> i64, ch: Channel<i64>) -> i64 { let m = Mutex::new(0); lock m as v { return f(ch); } }",
            ),
            (
                "main.wi",
                "import worker;\nfn helper(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let ch = Channel<i64>::new(); println(await worker::run(helper, ch)); }",
            ),
        ],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("error[E2604]"), "{out}");
    assert!(out.contains(IMPORTED_LOCK), "{out}");
    assert!(out.contains("main.wi:3"), "{out}");
}

/// Perspective 20: an imported waiting function passed to an imported helper
/// under the importer's lock is rejected.
#[test]
fn indirect_lock_20_imported_value_and_helper() {
    let (out, ok) = check(
        "ind20",
        &[
            (
                "worker.wi",
                "pub fn apply(f: fn(Channel<i64>) -> i64, ch: Channel<i64>) -> i64 { return f(ch); }\npub fn wait(ch: Channel<i64>) -> i64 { return ch.recv(); }",
            ),
            (
                "main.wi",
                "import worker;\nimport worker::wait;\nfn mine(ch: Channel<i64>) -> i64 { return worker::apply(wait, ch); }\nasync fn main() { let m = Mutex::new(0); let ch = Channel<i64>::new(); lock m as v { println(mine(ch)); } }",
            ),
        ],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("error[E2604]"), "{out}");
}

/// Perspective 21: the same two-module shape with a pure function value
/// compiles.
#[test]
fn indirect_lock_21_imported_pure_value_accepted() {
    let (out, ok) = checked(
        "ind21",
        &[
            (
                "worker.wi",
                "pub fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }\npub fn twice(x: i64) -> i64 { return x * 2; }",
            ),
            (
                "main.wi",
                "import worker;\nimport worker::twice;\nasync fn main() { let m = Mutex::new(0); lock m as v { println(worker::apply(twice, 21)); } }",
            ),
        ],
    );
    assert!(ok, "{out}");
}

/// Perspective 22: an accepted program still runs the indirect calls inside the
/// critical section with the expected results.
#[test]
fn indirect_lock_22_accepted_program_runs() {
    let (out, ok) = compile_and_run(
        "fn inc(x: i64) -> i64 { return x + 1; }\nfn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }\nasync fn main() { let m = Mutex::new(40); let op = inc; lock m as v { println(apply(op, v)); println(Option::Some(v).map(|x: i64| x + 2).unwrap_or(0)); } }\n",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "41\n42\n");
}

/// Perspective 23: one diagnostic per lock-held call even when several waiting
/// values share the arity.
#[test]
fn indirect_lock_23_single_report_per_site() {
    let out = rejected(
        "ind23",
        &locked(
            &format!("{WAITING}fn other(ch: Channel<i64>) -> i64 {{ return ch.recv(); }}\n"),
            "let a = helper; let b = other; a(ch);",
        ),
    );
    assert_eq!(out.matches("error[E2604]").count(), 1, "{out}");
}

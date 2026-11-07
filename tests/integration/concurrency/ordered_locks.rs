use super::*;

fn assert_ordered_run(source: &str, expected: &str) {
    let (out, ok, timed_out) =
        compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(15));
    assert!(!timed_out, "ordered acquisition or cleanup hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn ordered_locks_same_class_parameters_preserve_source_bindings() {
    assert_ordered_run(
        r#"
async fn combine(a: Mutex<i64>, b: Mutex<i64>) -> i64 {
    lock b as mut right, a as mut left {
        right = right + 2;
        left = left + 1;
        return left * 100 + right;
    }
}
async fn main() {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    println(await combine(a, b));
    lock a as left, b as right { println(left * 100 + right); }
}
"#,
        "1122\n1122\n",
    );
}

#[test]
fn ordered_locks_heterogeneous_mutex_and_rwlock_modes() {
    assert_ordered_run(
        r#"
async fn main() {
    let number = Mutex::new(10);
    let flag = Mutex::new(false);
    let source = RwLock::new(4);
    let destination = RwLock::new(0);
    lock write destination as mut out, flag as mut enabled,
         read source as input, number as mut value {
        enabled = true;
        value = value + input;
        out = value;
    }
    lock number as value, flag as enabled, read destination as out {
        println(value);
        println(enabled);
        println(out);
    }
}
"#,
        "14\ntrue\n14\n",
    );
}

#[test]
fn ordered_locks_targets_run_once_left_to_right_before_bindings() {
    assert_ordered_run(
        r#"
fn pick(m: Mutex<i64>, calls: BlockingCell<i64>, digit: i64) -> Mutex<i64> {
    calls.set(calls.get() * 10 + digit);
    return m;
}
async fn main() {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    let calls = BlockingCell::new(0);
    lock pick(b, calls, 2) as a, pick(a, calls, 1) as b {
        println(a);
        println(b);
    }
    println(calls.get());
}
"#,
        "20\n10\n21\n",
    );
}

#[test]
fn ordered_locks_break_and_continue_commit_all_bindings() {
    assert_ordered_run(
        r#"
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    let mut n = 0;
    while n < 3 {
        n = n + 1;
        lock b as mut y, a as mut x {
            x = x + 1;
            y = y + 10;
            if n < 3 { continue; }
            break;
        }
    }
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "3\n30\n",
    );
}

#[test]
fn ordered_locks_recovered_panic_commits_and_releases_all() {
    assert_ordered_run(
        r#"
async fn risky(a: Mutex<i64>, b: Mutex<i64>, fail: bool) {
    defer match recover() {
        Some(_) => println("recovered"),
        None => { }
    }
    lock b as mut y, a as mut x {
        x = x + 1;
        y = y + 10;
        if fail { panic("group body"); }
    }
}
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    await risky(a, b, true);
    await risky(a, b, false);
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "recovered\n2\n20\n",
    );
}

#[test]
fn ordered_locks_distinct_nested_acquisitions_commit_on_return() {
    assert_ordered_run(
        r#"
async fn nested() -> i64 {
    let a = Mutex::new(10);
    let b = Mutex::new(20);
    lock a as x {
        lock b as y { return x + y; }
    }
}
async fn main() { println(await nested()); }
"#,
        "30\n",
    );
}

#[test]
fn ordered_locks_static_alias_is_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let alias = a;
    lock a as x, alias as y { println(x + y); }
}
"#,
        &["E2605"],
    );
}

#[test]
fn ordered_locks_unknown_duplicate_recovers_without_leaking_prefix() {
    assert_ordered_run(
        r#"
async fn duplicate(prefix: Mutex<i64>, a: Mutex<i64>, b: Mutex<i64>) {
    defer match recover() {
        Some(_) => println("duplicate rejected"),
        None => println("unexpected success"),
    }
    lock prefix as p, a as x, b as y { println("unexpected entry"); }
}
async fn main() {
    let prefix = Mutex::new(1);
    let a = Mutex::new(2);
    await duplicate(prefix, a, a);
    lock prefix as p, a as x { println(p + x); }
}
"#,
        "duplicate rejected\n3\n",
    );
}

#[test]
fn ordered_locks_wait_in_group_remains_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x, b as y { await sleep(1); }
}
"#,
        &["E2604"],
    );
}

#[test]
fn ordered_locks_wait_in_nested_section_remains_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x { lock b as y { await yield(); } }
}
"#,
        &["E2604"],
    );
}

#[test]
fn ordered_locks_held_cancellation_commits_before_outer_cleanup() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn victim(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicBool,
                next_entered: AtomicBool) {
    defer {
        while !next_entered.load() { }
        println("outer");
    }
    lock b as mut y, a as mut x {
        defer { x = x + 10; y = y + 20; println("inner"); }
        x = 1;
        y = 2;
        entered.store(true);
        while true { x = x; }
    }
}
async fn contender(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicBool) {
    lock a as x, b as y { entered.store(true); }
}
async fn main() {
    let a = Mutex::new(0);
    let b = Mutex::new(0);
    let entered = AtomicBool::new(false);
    let next_entered = AtomicBool::new(false);
    let stopped = victim(a, b, entered, next_entered);
    while !entered.load() { await yield(); }
    let next = contender(a, b, next_entered);
    stopped.cancel();
    match await stopped.result() {
        Ok(_) => println("unexpected completion"),
        Err(Cancelled) => { }
    }
    await next;
    lock a as x, b as y { println(x); println(y); }
}
"#,
        &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "group cancellation cleanup hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, "inner\nouter\n11\n22\n");
}

#[test]
fn ordered_locks_calls_under_locks_do_not_need_a_lock_proof() {
    assert_ordered_run(
        r#"
interface Step extends Sync { fn step(self, n: i64) -> i64; }
class Add implements Step {
    pub fn step(self, n: i64) -> i64 { return n + 1; }
}
fn helper(n: i64) -> i64 { return n * 2; }
fn twice(n: i64) -> i64 { return n * 2; }
async fn update(m: Mutex<i64>, flag: AtomicBool, s: Step, f: fn(i64) -> i64) {
    lock m as mut x {
        flag.store(true);
        x = f(s.step(helper(x)));
    }
}
async fn main() {
    let text = Mutex::new("abc");
    let number = Mutex::new(1);
    let flag = AtomicBool::new(false);
    await update(number, flag, new Add(), twice);
    lock text as t, number as n {
        println(t.len());
        println(n);
    }
    println(flag.load());
}
"#,
        "3\n6\ntrue\n",
    );
}

#[test]
fn ordered_locks_panic_commits_live_gc_member_value() {
    assert_ordered_run(
        r#"
async fn risky(a: Mutex<i64>, b: Mutex<String>, fail: bool) {
    defer match recover() {
        Some(_) => println("recovered"),
        None => { }
    }
    lock a as x, b as mut y {
        println(y);
        if fail { panic("group body"); }
        y = "new";
    }
}
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new("old");
    await risky(a, b, true);
    lock b as y { println(y); }
    await risky(a, b, false);
    lock b as y { println(y); }
}
"#,
        "old\nrecovered\nold\nold\nnew\n",
    );
}

#[test]
fn ordered_locks_factory_locks_are_told_to_use_a_group() {
    assert_compile_error_contains(
        r#"
fn make(value: i64) -> Mutex<i64> { return Mutex::new(value); }
async fn main() {
    let a = make(1);
    let b = make(2);
    lock a as x { lock b as y { println(x + y); } }
}
"#,
        &[
            "E2605",
            "created at the same place",
            "lock first as a, second as b",
        ],
    );
    assert_ordered_run(
        r#"
fn make(value: i64) -> Mutex<i64> { return Mutex::new(value); }
async fn main() {
    let a = make(1);
    let b = make(2);
    lock a as x, b as y { println(x + y); }
}
"#,
        "3\n",
    );
}

#[test]
fn ordered_locks_parent_and_child_allocation_nest() {
    assert_ordered_run(
        r#"
async fn main() {
    let parent = Mutex::new(Mutex::new(7));
    lock parent as child {
        lock child as mut value { value = value + 1; }
        lock child as value { println(value); }
    }
}
"#,
        "8\n",
    );
}

#[test]
fn ordered_locks_parent_and_child_parameter_nest() {
    assert_ordered_run(
        r#"
async fn bump(parent: Mutex<Mutex<i64>>) -> i64 {
    lock parent as child {
        lock child as mut value {
            value = value + 1;
            return value;
        }
    }
}
async fn main() {
    let parent = Mutex::new(Mutex::new(7));
    println(await bump(parent));
    println(await bump(parent));
}
"#,
        "8\n9\n",
    );
}

#[test]
fn ordered_locks_replaced_child_stays_in_its_type_class() {
    assert_ordered_run(
        r#"
async fn main() {
    let parent = Mutex::new(Mutex::new(1));
    lock parent as mut child {
        child = Mutex::new(5);
        lock child as value { println(value); }
    }
}
"#,
        "5\n",
    );
}

#[test]
fn ordered_locks_child_then_parent_is_a_cycle() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let child = Mutex::new(1);
    let parent = Mutex::new(child);
    lock parent as held { lock held as value { println(value); } }
    lock child as value { lock parent as held { println(value); } }
}
"#,
        &["E2605", "cycle"],
    );
}

#[test]
fn ordered_locks_child_and_known_lock_of_its_type_are_rejected() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let child = Mutex::new(1);
    let parent = Mutex::new(child);
    lock parent as held {
        lock held as a {
            lock child as b { println(a + b); }
        }
    }
}
"#,
        &["E2605"],
    );
}

#[test]
fn ordered_locks_same_type_parameters_need_a_group() {
    assert_compile_error_contains(
        r#"
async fn f(a: Mutex<i64>, b: Mutex<i64>) {
    lock a as x { lock b as y { println(x + y); } }
}
async fn main() { await f(Mutex::new(1), Mutex::new(2)); }
"#,
        &["E2605", "cannot be told apart", "acquire them together"],
    );
    assert_ordered_run(
        r#"
async fn f(a: Mutex<i64>, b: Mutex<i64>) {
    lock a as x, b as y { println(x + y); }
}
async fn main() { await f(Mutex::new(1), Mutex::new(2)); }
"#,
        "3\n",
    );
}

#[test]
fn ordered_locks_distinct_type_parameters_nest_in_one_order() {
    assert_ordered_run(
        r#"
async fn f(a: Mutex<i64>, b: Mutex<String>) {
    lock a as x { lock b as y { println(x); println(y); } }
}
async fn main() { await f(Mutex::new(1), Mutex::new("two")); }
"#,
        "1\ntwo\n",
    );
    assert_compile_error_contains(
        r#"
async fn f(a: Mutex<i64>, b: Mutex<String>) {
    lock a as x { lock b as y { println(x); } }
}
async fn g(a: Mutex<i64>, b: Mutex<String>) {
    lock b as y { lock a as x { println(x); } }
}
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new("two");
    await f(a, b);
    await g(a, b);
}
"#,
        &["E2605", "cycle"],
    );
}

#[test]
fn ordered_locks_recursive_lock_type_is_rejected() {
    assert_compile_error_contains(
        r#"
class Node {
    pub value: i64;
    pub next: Mutex<Node>;
}
async fn walk(head: Mutex<Node>) {
    lock head as node {
        lock node.next as following { println(following.value); }
    }
}
async fn main() { }
"#,
        &["E2605", "reached only through another held lock"],
    );
}

#[test]
fn ordered_locks_collection_elements_of_one_type_are_rejected() {
    assert_compile_error_contains(
        r#"
async fn f(a: Mutex<i64>, b: Mutex<i64>) {
    let locks = [a, b];
    lock locks[0] as x { lock locks[1] as y { println(x + y); } }
}
async fn main() { await f(Mutex::new(1), Mutex::new(2)); }
"#,
        &["E2605"],
    );
}

#[test]
fn ordered_locks_type_parameter_has_no_type_class() {
    assert_compile_error_contains(
        r#"
async fn f<T>(a: Mutex<T>, b: Mutex<i64>) {
    lock a as x { lock b as y { println(y); } }
}
async fn main() { await f(Mutex::new("x"), Mutex::new(2)); }
"#,
        &["E2605", "lock identity or ordering is unknown"],
    );
}

#[test]
fn ordered_locks_unnested_parameter_does_not_merge_known_locks() {
    assert_ordered_run(
        r#"
async fn read(m: Mutex<i64>) -> i64 {
    lock m as value { return value; }
}
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x { lock b as y { println(x + y); } }
    println(await read(a));
}
"#,
        "3\n1\n",
    );
}

#[test]
fn ordered_locks_unrelated_type_nesting_keeps_known_locks_distinct() {
    assert_ordered_run(
        r#"
async fn unrelated(x: Mutex<String>, y: Mutex<i64>) {
    lock x as s { lock y as v { println(s); println(v); } }
}
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    lock a as x { lock b as y { println(x + y); } }
}
"#,
        "3\n",
    );
}

#[test]
fn ordered_locks_type_order_applies_to_each_known_lock() {
    assert_compile_error_contains(
        r#"
async fn typed(x: Mutex<String>, y: Mutex<i64>) {
    lock x as s { lock y as v { println(s); println(v); } }
}
async fn main() {
    let number = Mutex::new(1);
    let text = Mutex::new("one");
    lock number as n { lock text as s { println(n); println(s); } }
    await typed(text, number);
}
"#,
        &["E2605", "cycle"],
    );
}

#[test]
fn ordered_locks_same_named_types_of_two_modules_stay_distinct() {
    let project = TestProject::new(
        "ordered_lock_same_names",
        &[
            (
                "project.toml",
                "[project]\nname=\"lock-names\"\nversion=\"0.1.0\"\n[willow]\nmanifest-version=1\n",
            ),
            (
                "src/left.wi",
                "pub class Item { pub n: i64; pub init(self, n: i64) { self.n = n; } }",
            ),
            (
                "src/right.wi",
                "pub class Item { pub n: i64; pub init(self, n: i64) { self.n = n; } }",
            ),
            (
                "src/main.wi",
                "import left; import right; \
                 async fn f(a: Mutex<left::Item>, b: Mutex<right::Item>) { \
                     lock a as x { lock b as y { println(x.n + y.n); } } \
                 } async fn main() { }",
            ),
        ],
    );
    let output = project.package_command("check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
}

#[test]
fn ordered_locks_type_classes_agree_across_project_units() {
    let provider = "pub class Item { pub n: i64; pub init(self, n: i64) { self.n = n; } } \
        pub async fn forward(a: Mutex<Item>, b: Mutex<i64>) { \
            lock a as x { lock b as y { println(y); } } \
        }";
    for (import, item, body, rejected) in [
        (
            "import provider;",
            "provider::Item",
            "lock a as x { println(x.n); }",
            false,
        ),
        (
            "import provider;",
            "provider::Item",
            "lock b as y { lock a as x { println(y); } }",
            true,
        ),
        (
            "import provider::Item as Picked;",
            "Picked",
            "lock b as y { lock a as x { println(y); } }",
            true,
        ),
    ] {
        let main = format!(
            "{import} async fn backward(a: Mutex<{item}>, b: Mutex<i64>) {{ {body} }} async fn main() {{ }}"
        );
        let project = TestProject::new(
            "ordered_lock_type_classes",
            &[
                (
                    "project.toml",
                    "[project]\nname=\"lock-types\"\nversion=\"0.1.0\"\n[willow]\nmanifest-version=1\n",
                ),
                ("src/provider.wi", provider),
                ("src/main.wi", &main),
            ],
        );
        let output = project.package_command("check");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(!output.status.success(), rejected, "{main}\n{stderr}");
        if rejected {
            assert!(stderr.contains("E2605"), "{stderr}");
        }
    }
}

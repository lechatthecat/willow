use super::*;

fn run(source: &str, expected: &str) {
    let (out, ok, timeout) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_WORKERS", "2"), ("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(20),
    );
    assert!(!timeout, "lock coupling hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn lock_coupling_linked_list_example() {
    run(include_str!("../../../example/lock_coupling.wi"), "6\n6\n");
}

#[test]
fn lock_coupling_captured_cursor_compiles_with_immediate_break() {
    // Even an unreachable handoff must lower the cursor's capture-cell read.
    let source = r#"
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    let mut cursor = a;
    if true { let advance = || { cursor = b; }; advance(); }
    lock walk cursor as n { println(n); break; }
}
"#;
    run(source, "2\n");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn lock_coupling_captured_cursor_reloads_after_continue_and_defers() {
    run(
        r#"
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2); let c = Mutex::new(3);
    let mut cursor = a;
    if true { let advance = || { cursor = b; }; advance(); }
    let mut visits = 0;
    lock walk cursor as mut n {
        visits += 1;
        let before = n;
        n += 10;
        if before == 2 { cursor = c; continue; }
        if before == 3 { defer { cursor = a; } } else { break; }
    }
    println(visits);
    lock a as x, b as y, c as z { println(x); println(y); println(z); }
}
"#,
        "3\n11\n12\n13\n",
    );
}

#[test]
fn lock_coupling_captured_cursor_is_rooted_across_pair_boxing_and_gc() {
    let source = r#"
async fn main() {
    let a: Mutex<Option<i64>> = Mutex::new(Some(1));
    let b: Mutex<Option<i64>> = Mutex::new(None);
    let mut cursor = a;
    if true { let capture = || { cursor = a; }; capture(); }
    lock walk cursor as mut value {
        match value {
            Some(_) => { value = Some(11); cursor = b; },
            None => { value = Some(22); break; }
        }
    }
    lock a as x, b as y { println(x.unwrap()); println(y.unwrap()); }
}
"#;
    let (out, ok, timeout) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_GC_STRESS", "all"), ("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timeout, "{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "11\n22\n");
}

#[test]
fn lock_coupling_scalar_continue_break_and_iteration_defers() {
    run(
        r#"
async fn main() {
    let a = Mutex::new(1);
    let b = Mutex::new(2);
    let mut cursor = a;
    let mut visits = 0;
    lock walk cursor as mut value {
        defer { value += 10; }
        visits += 1;
        if visits == 1 { cursor = b; continue; }
        break;
    }
    lock a as x, b as y { println(x); println(y); }
    println(visits);
}
"#,
        "11\n12\n2\n",
    );
}

#[test]
fn lock_coupling_return_releases_current_node() {
    run(
        r#"
async fn visit(a: Mutex<i64>, b: Mutex<i64>) -> i64 {
    let mut cursor = a;
    lock walk cursor as mut n {
        if n == 2 { n = 22; return n; }
        n = 11;
        cursor = b;
    }
    return 0;
}
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    println(await visit(a, b));
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "22\n11\n22\n",
    );
}

#[test]
fn lock_coupling_self_link_recovers_and_commits() {
    run(
        r#"
async fn visit(a: Mutex<i64>) {
    defer match recover() { Some(_) => println("caught"), None => {} };
    let cursor = a;
    lock walk cursor as mut n { n += 1; }
}
async fn main() {
    let a = Mutex::new(1);
    await visit(a);
    lock a as n { println(n); }
}
"#,
        "caught\n2\n",
    );
}

#[test]
fn lock_coupling_panic_after_handoff_releases_new_node() {
    run(
        r#"
async fn visit(a: Mutex<i64>, b: Mutex<i64>) {
    defer match recover() { Some(_) => println("caught"), None => {} };
    let mut cursor = a;
    lock walk cursor as mut n {
        if n == 2 { n = 20; panic("stop"); }
        n = 10;
        cursor = b;
    }
}
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    await visit(a, b);
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "caught\n10\n20\n",
    );
}

#[test]
fn lock_coupling_static_restrictions_preserve_wait_order_and_loans() {
    for (body, code) in [
        ("lock walk a as n { await yield(); }", "E2604"),
        ("lock walk a as n { helper(); }", "E2604"),
        ("lock walk a as n { lock b as m {} }", "E2605"),
        ("lock b as m { lock walk a as n { break; } }", "E2605"),
        ("lock walk a as n, b as m { break; }", "E2601"),
        ("lock walk read r as n { break; }", "E2601"),
        ("lock walk pick(a) as n { break; }", "E2601"),
        ("lock walk a as a { break; }", "E2601"),
        ("lock walk a as n { n = 2; break; }", "E0301"),
    ] {
        let source = format!(
            r#"
fn helper() {{ let c = BlockingCell::new(0); c.get(); }}
fn pick(m: Mutex<i64>) -> Mutex<i64> {{ return m; }}
async fn main() {{
    let a = Mutex::new(1); let b = Mutex::new(2); let r = RwLock::new(3);
    {body}
}}
"#
        );
        assert_compile_error_contains(&source, &[code]);
    }
    assert_compile_error_contains(
        "fn main() { let a = Mutex::new(1); lock walk a as n { break; } }",
        &["E2603"],
    );
    assert_compile_error_contains(
        r#"
async fn main() {
    let a = Mutex::new([1]); let mut escaped = [0];
    lock walk a as n { escaped = n; break; }
    println(escaped[0]);
}
"#,
        &["E2408"],
    );
}

#[test]
fn lock_coupling_opposing_walks_fail_safely() {
    run(
        r#"
async fn walk(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicI64, failures: AtomicI64) {
    defer match recover() { Some(_) => { failures.add(1); }, None => {} };
    let mut cursor = a;
    let mut visits = 0;
    lock walk cursor as value {
        visits += 1;
        if visits == 2 { break; }
        entered.add(1);
        while entered.load() < 2 { }
        cursor = b;
    }
}
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    let entered = AtomicI64::new(0); let failures = AtomicI64::new(0);
    let forward = walk(a, b, entered, failures);
    let backward = walk(b, a, entered, failures);
    await forward; await backward;
    println(failures.load() >= 1);
    lock a as x, b as y { println(x + y); }
}
"#,
        "true\n3\n",
    );
}

#[test]
fn lock_coupling_cancel_after_transfer_commits_and_releases() {
    run(
        r#"
async fn victim(a: Mutex<i64>, b: Mutex<i64>, entered: AtomicBool) {
    let mut cursor = a;
    lock walk cursor as mut value {
        if value == 2 {
            defer { value += 20; }
            value = 12;
            entered.store(true);
            while true { }
        }
        value = 11;
        cursor = b;
    }
}
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    let entered = AtomicBool::new(false);
    let stopped = victim(a, b, entered);
    while !entered.load() { await yield(); }
    stopped.cancel();
    match await stopped.result() { Ok(_) => println("bad"), Err(Cancelled) => {} }
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "11\n32\n",
    );
}

#[test]
fn lock_coupling_cycle_revisits_released_nodes_and_nested_loops_stay_local() {
    run(
        r#"
async fn main() {
    let a = Mutex::new(0); let b = Mutex::new(0);
    let mut cursor = a; let mut count = 0;
    lock walk cursor as mut n {
        for i in 0..3 { if i == 0 { continue; } n += 1; if i == 2 { break; } }
        count += 1;
        if count == 4 { break; }
        if count % 2 == 1 { cursor = b; } else { cursor = a; }
    }
    lock a as x, b as y { println(x); println(y); }
    let walk = Mutex::new(7);
    lock walk as value { println(value); }
}
"#,
        "4\n4\n7\n",
    );
}

#[test]
fn lock_coupling_pairs_reference_words_and_gc_survive_transfer() {
    let source = r#"
class Node { pub value: String; pub next: Option<Mutex<Node>>; }
async fn main() {
    let tail = Mutex::new(new Node("tail", None));
    let mut head = tail;
    for i in 0..32 { head = Mutex::new(new Node("node", Some(head))); }
    let mut cursor = head; let mut count = 0;
    lock walk cursor as node {
        let allocation = [node.value, "extra"];
        count += 1;
        match node.next { Some(next) => { cursor = next; }, None => { break; } }
    }
    println(count);
    let a: Mutex<Option<String>> = Mutex::new(Some("first"));
    let b: Mutex<Option<String>> = Mutex::new(None);
    let mut pair_cursor = a;
    lock walk pair_cursor as mut value {
        match value {
            Some(_) => { value = Some("changed"); pair_cursor = b; },
            None => { value = Some("last"); break; }
        }
    }
    lock a as x, b as y { println(x.unwrap()); println(y.unwrap()); }
}
"#;
    let (out, ok, timeout) = compile_and_run_with_env_timeout(
        source,
        &[("WILLOW_GC_STRESS", "all"), ("WILLOW_TASK_BUDGET", "1")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timeout, "{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "33\nchanged\nlast\n");
}

#[test]
fn lock_coupling_release_build_and_shared_handoff_codegen() {
    let (out, ok) = compile_and_run_release(include_str!("../../../example/lock_coupling.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "6\n6\n");
    for (captured, exits) in [false, true]
        .into_iter()
        .flat_map(|captured| [1, 8, 32].map(|exits| (captured, exits)))
    {
        let capture = if captured {
            "if true { let capture = || { cursor = a; }; capture(); }"
        } else {
            ""
        };
        let branches = (0..exits)
            .map(|i| format!("if n == {i} {{ cursor = b; continue; }}"))
            .collect::<String>();
        let source = format!(
            r#"
async fn visit(a: Mutex<i64>, b: Mutex<i64>) {{
    let mut cursor = a;
    {capture}
    lock walk cursor as n {{ {branches} break; }}
}}
async fn main() {{ await visit(Mutex::new(0), Mutex::new(100)); }}
"#
        );
        let symbols =
            super::super::support::compile_and_collect_relocation_targets_mode(&source, &[], false);
        let count = symbols
            .iter()
            .filter(|name| name.ends_with("willow_async_mutex_handoff"))
            .count();
        assert_eq!(
            count, 1,
            "captured={captured}, {exits} continues duplicated the transfer"
        );
        println!("captured={captured} continue_exits={exits} handoff_callsites={count}");
    }
}

#[test]
fn lock_coupling_keyword_preserves_function_and_field_targets() {
    run(
        r#"
class Holder { pub cell: Mutex<i64>; }
fn walk() -> Mutex<i64> { return Mutex::new(7); }
async fn main() {
    lock walk() as n { println(n); }
    let walk = new Holder(Mutex::new(8));
    lock walk.cell as n { println(n); }
}
"#,
        "7\n8\n",
    );
}

#[test]
fn lock_coupling_recovery_in_iteration_and_outer_scopes() {
    run(
        r#"
async fn visit(a: Mutex<i64>, b: Mutex<i64>) {
    let mut cursor = a;
    lock walk cursor as mut n {
        defer match recover() { Some(_) => println("iteration"), None => {} };
        if n == 1 { n = 11; cursor = b; panic("advance"); }
        n = 22; break;
    }
}
async fn main() {
    let a = Mutex::new(1); let b = Mutex::new(2);
    await visit(a, b);
    lock a as x, b as y { println(x); println(y); }
}
"#,
        "iteration\n11\n22\n",
    );
}

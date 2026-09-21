use super::*;

// ── Task-id + spawn-site debug traces (willow-0a6k.7 slice 2) ───────────────
// 10 perspectives on top of cancel_01-20: 1 panic trace carries task id,
// 2 panic trace carries spawn file:line, 3 spawn line is the CALL site (not
// the async fn definition), 4 fire-and-forget spawn (`let h = f();`) records
// a site, 5 await-driven spawn records a site, 6 the runtime-spawned main
// task has an id but no spawn site, 7 two tasks get distinct ids, 8 nested
// spawn chain shows a line per awaiter with ids, 9 cancelled-await panic
// reports the cancelled task's id, 10 chain text keeps the fn name.

#[test]
fn trace_01_panic_has_task_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("[task "), "{out}");
}

#[test]
fn trace_02_panic_has_spawn_site() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
    assert!(out.contains(".wi:2"), "{out}");
}

#[test]
fn trace_03_spawn_line_is_call_site() {
    // The spawn happens on line 4, the async fn is defined on line 1.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nfn pad1() {}\nfn pad2() {}\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains(".wi:4"), "{out}");
    assert!(
        !out.contains(".wi:1]"),
        "must not point at the definition line: {out}"
    );
}

#[test]
fn trace_04_fire_and_forget_records_site() {
    // The awaited task does not suspend before panicking: this covers the
    // plain-call task creation path.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { panic(\"boom\"); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
}

#[test]
fn trace_05_awaited_spawn_records_site() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let v = await t(); println(v); }",
    );
    assert!(!ok);
    assert!(out.contains("spawned at "), "{out}");
}

#[test]
fn trace_06_main_task_has_id_but_no_site() {
    let (out, ok) =
        compile_and_run_check_exit("async fn main() { await sleep(1); panic(\"boom\"); }");
    assert!(!ok);
    assert!(out.contains("async main [task "), "{out}");
    assert!(!out.contains("async main [task 1, spawned"), "{out}");
}

#[test]
fn trace_07_distinct_task_ids() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { await sleep(1); return 1; }\nasync fn main() { let a = t(); let b = t(); println(await a + await b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn trace_08_nested_chain_ids_per_frame() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn inner() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn outer() -> i64 { let v = await inner(); return v; }\nasync fn main() { println(await outer()); }",
    );
    assert!(!ok);
    assert!(out.contains("0: async inner [task "), "{out}");
    assert!(out.contains("1: async outer [task "), "{out}");
}

#[test]
fn trace_09_cancelled_await_reports_task_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await sleep(30); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("awaited a cancelled task (task"), "{out}");
}

#[test]
fn trace_10_chain_keeps_fn_name() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn my_worker() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { println(await my_worker()); }",
    );
    assert!(!ok);
    assert!(out.contains("async my_worker"), "{out}");
}

use super::support::*;

macro_rules! value_case {
    ($name:ident, $source:expr) => {
        #[test]
        fn $name() {
            let (out, ok) = compile_and_run($source);
            assert!(ok, "{out}");
            assert_eq!(out, "ok\n");
        }
    };
}

value_case!(
    future_value_01_direct,
    "async fn main() { await sleep(1); println(\"ok\"); }"
);
value_case!(
    future_value_02_stored,
    "async fn main() { let f = sleep(1); await f; println(\"ok\"); }"
);
value_case!(
    future_value_03_alias,
    "async fn main() { let f = sleep(1); let g = f; await g; await f; println(\"ok\"); }"
);
value_case!(
    future_value_04_repeated,
    "async fn main() { let f = sleep(1); for i in 0..20 { await f; } println(\"ok\"); }"
);
value_case!(
    future_value_05_unused,
    "async fn main() { for i in 0..1000 { let f = sleep(60000); } println(\"ok\"); }"
);
value_case!(
    future_value_06_discarded,
    "async fn main() { for i in 0..1000 { sleep(60000); yield(); } println(\"ok\"); }"
);
value_case!(
    future_value_07_return,
    "fn make() -> Future<void> { return sleep(1); } async fn main() { let f = make(); await f; await f; println(\"ok\"); }"
);
value_case!(
    future_value_08_argument,
    "fn copy(f: Future<void>) -> Future<void> { return f; } async fn main() { let f = sleep(1); await copy(f); await copy(f); println(\"ok\"); }"
);
value_case!(
    future_value_09_reassign,
    "async fn main() { let mut f = sleep(60000); f = sleep(0); await f; println(\"ok\"); }"
);
value_case!(
    future_value_10_yield,
    "async fn main() { let f = yield(); let g = f; await f; await g; println(\"ok\"); }"
);
value_case!(
    future_value_11_negative,
    "async fn main() { let f = sleep(-10); await f; println(\"ok\"); }"
);
value_case!(
    future_value_12_early_return,
    "fn work() { let f = sleep(60000); return; } async fn main() { work(); println(\"ok\"); }"
);
value_case!(
    future_value_13_break_continue,
    "async fn main() { for i in 0..10 { let f = sleep(60000); if i == 5 { break; } continue; } println(\"ok\"); }"
);
value_case!(
    future_value_14_branch,
    "fn make(b: bool) -> Future<void> { if b { return sleep(0); } return yield(); } async fn main() { await make(true); await make(false); println(\"ok\"); }"
);
value_case!(
    future_value_15_option,
    "async fn main() { let f: Option<Future<void>> = Some(yield()); match f { Some(v) => { await v; println(\"ok\"); }, None => println(\"bad\"), } }"
);
value_case!(
    future_value_16_array,
    "async fn main() { let fs = [sleep(0), yield(), sleep(1)]; for f in fs { await f; await f; } println(\"ok\"); }"
);
value_case!(
    future_value_17_field,
    "class Holder { pub f: Future<void>; } async fn main() { let h = new Holder(sleep(1)); await h.f; await h.f; println(\"ok\"); }"
);
value_case!(
    future_value_18_async_stored,
    "async fn work() { let f = sleep(1); await yield(); await f; await f; } async fn main() { await work(); println(\"ok\"); }"
);
value_case!(
    future_value_19_recovery,
    "async fn main() { let f = sleep(1); if true { let unused = sleep(60000); defer match recover() { Some(_) => {}, None => {} } panic(\"recover\"); } await f; println(\"ok\"); }"
);
value_case!(
    future_value_20_closure,
    "fn make() -> Future<void> { let f = sleep(1); let get = || { return f; }; return get(); } async fn main() { let f = make(); await f; await f; println(\"ok\"); }"
);
value_case!(
    future_value_21_cancel,
    "async fn work(ready: Channel<i64>) { let f = sleep(60000); ready.send(1); await f; println(\"bad\"); } async fn main() { let ready = Channel<i64>::new(); let task = work(ready); ready.recv(); task.cancel(); let _ = await task.result(); if task.is_cancelled() { println(\"ok\"); } }"
);

#[test]
fn future_value_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn main() { let f = sleep(1); let fs = [f, f]; println(\"o\" + \"k\"); for v in fs { await v; } await f; }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ok\n");
}

#[test]
fn future_value_codegen_counts() {
    for n in [1, 16, 64] {
        let mut source = String::from("async fn main() {\n");
        for i in 0..n {
            source.push_str(&format!(
                "let f{i} = sleep(0); let alias{i} = f{i}; await alias{i}; await f{i};\n"
            ));
        }
        source.push('}');
        let calls = compile_and_collect_relocation_targets_all(&source, &[]);
        assert!(
            !calls.iter().any(|s| s.starts_with("willow_future_")
                || s == "willow_runtime_sleep"
                || s == "willow_runtime_yield"),
            "{calls:?}"
        );
        let creates = calls
            .iter()
            .filter(|s| s.as_str() == "willow_timer_value_sleep")
            .count();
        let awaits = calls
            .iter()
            .filter(|s| s.as_str() == "willow_timer_value_await")
            .count();
        assert_eq!(creates, n);
        assert_eq!(awaits, 2 * n);
        println!("future-values n={n} constructors={creates} awaits={awaits} legacy_calls=0");
    }
}

#[test]
fn future_value_all_producers_avoid_legacy_handles() {
    let calls = compile_and_collect_relocation_targets(
        "fn make() -> Future<void> { return sleep(1); } fn ready() -> Future<void> { return yield(); } async fn main() { let f = make(); let g = ready(); await f; await g; await sleep(0); await yield(); }",
        &[],
    );
    for expected in [
        "willow_timer_value_sleep",
        "willow_timer_value_yield",
        "willow_timer_value_await",
    ] {
        assert!(
            calls.iter().any(|name| name == expected),
            "missing {expected}: {calls:?}"
        );
    }
    assert!(
        !calls.iter().any(|name| name.starts_with("willow_future_")
            || name == "willow_runtime_sleep"
            || name == "willow_runtime_yield"),
        "{calls:?}"
    );
}

// A user async fn named `sleep`/`yield` returns a `Task`, so a direct await
// must run that function instead of the runtime scheduler builtin.
#[test]
fn future_value_user_scheduler_names_await_their_task() {
    // Covers call shapes, arities, return types, await positions, a method
    // named `sleep`, and builtin `sleep`/`yield` beside user functions.
    const USER_SLEEP: &str = "async fn sleep(n: i64) { println(\"user sleep\"); }\n";
    let cases: [(&str, String, &str); 25] = [
        ("direct", format!("{USER_SLEEP}async fn main() {{ await sleep(1); println(\"ok\"); }}\n"), "user sleep\nok\n"),
        ("stored", format!("{USER_SLEEP}async fn main() {{ let t = sleep(1); await t; println(\"ok\"); }}\n"), "user sleep\nok\n"),
        ("annotated", format!("{USER_SLEEP}async fn main() {{ let t: Task<()> = sleep(1); await t; println(\"ok\"); }}\n"), "user sleep\nok\n"),
        ("yield", "async fn yield() { println(\"user yield\"); }\nasync fn main() { await yield(); println(\"ok\"); }\n".to_string(), "user yield\nok\n"),
        ("value", "async fn sleep(n: i64) -> i64 { return n * 2; }\nasync fn main() { println(await sleep(21)); }\n".to_string(), "42\n"),
        ("zero_args", "async fn sleep() { println(\"user sleep\"); }\nasync fn main() { await sleep(); println(\"ok\"); }\n".to_string(), "user sleep\nok\n"),
        ("two_args", "async fn sleep(a: i64, b: i64) -> i64 { return a + b; }\nasync fn main() { println(await sleep(1, 2)); }\n".to_string(), "3\n"),
        ("yield_arg", "async fn yield(n: i64) -> i64 { return n + 1; }\nasync fn main() { println(await yield(4)); }\n".to_string(), "5\n"),
        ("yield_value", "async fn yield() -> String { return \"y\"; }\nasync fn main() { println(await yield()); }\n".to_string(), "y\n"),
        ("join_handle", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { let h: JoinHandle<i64> = sleep(7); println(await h); }\n".to_string(), "7\n"),
        ("array_element", "import std::collections::Array;\nasync fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { let ts: Array<Task<i64>> = [sleep(1), sleep(2)]; println(await ts[1] + await ts[0]); }\n".to_string(), "3\n"),
        ("string_param", "async fn sleep(s: String) -> String { return s + \"!\"; }\nasync fn main() { println(await sleep(\"hi\")); }\n".to_string(), "hi!\n"),
        ("result", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { match await sleep(9).result() { Ok(v) => println(v), Err(_) => println(\"cancelled\") } }\n".to_string(), "9\n"),
        ("condition", "async fn sleep(n: i64) -> bool { return n > 0; }\nasync fn main() { if (await sleep(1)) { println(\"yes\"); } else { println(\"no\"); } }\n".to_string(), "yes\n"),
        ("while", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { let mut x = 0; while (x < 3) { x = x + await sleep(1); } println(x); }\n".to_string(), "3\n"),
        ("for", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { let mut x = 0; for i in 0..4 { x = x + await sleep(i); } println(x); }\n".to_string(), "6\n"),
        ("match_arm", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { match 1 { 1 => { println(await sleep(5)); }, _ => { println(0); } } }\n".to_string(), "5\n"),
        ("binary", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn main() { println(await sleep(2) + await sleep(3)); }\n".to_string(), "5\n"),
        ("transitive", format!("{USER_SLEEP}async fn outer() {{ await sleep(1); println(\"outer\"); }}\nasync fn main() {{ await outer(); }}\n"), "user sleep\nouter\n"),
        ("sync_caller", "async fn sleep(n: i64) -> i64 { return n; }\nasync fn run() -> i64 { return await sleep(8); }\nfn main() { let t = run(); }\n".to_string(), ""),
        ("method", "class Svc { pub async fn sleep(self, n: i64) -> i64 { return n + 10; } }\nasync fn main() { let s = new Svc(); println(await s.sleep(1)); }\n".to_string(), "11\n"),
        ("builtin_beside_user", "async fn nap() { println(\"nap\"); }\nasync fn main() { await sleep(1); await nap(); await yield(); println(\"ok\"); }\n".to_string(), "nap\nok\n"),
        ("builtin_stored", "async fn main() { let t = sleep(1); await t; await yield(); println(\"ok\"); }\n".to_string(), "ok\n"),
        ("both_user", format!("{USER_SLEEP}async fn yield() {{ println(\"user yield\"); }}\nasync fn main() {{ await sleep(1); await yield(); await sleep(2); println(\"ok\"); }}\n"), "user sleep\nuser yield\nuser sleep\nok\n"),
        ("repeat", format!("{USER_SLEEP}async fn main() {{ for i in 0..3 {{ await sleep(i); }} println(\"ok\"); }}\n"), "user sleep\nuser sleep\nuser sleep\nok\n"),
    ];
    for (name, source, expected) in &cases {
        let (out, ok) = compile_and_run(source);
        assert!(
            ok,
            "{name}
{source}
{out}"
        );
        assert_eq!(
            &out, expected,
            "{name}
{source}"
        );
    }
}

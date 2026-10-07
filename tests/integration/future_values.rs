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

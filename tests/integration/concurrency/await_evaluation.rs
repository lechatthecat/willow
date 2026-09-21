use super::*;

// ── The awaited expression is evaluated ONCE, and awaitability follows the
// TYPE, not the syntax (willow-qrj9) ────────────────────────────────────────
// Cooperative resume re-enters the poll fn at a resume block. It used to
// RE-EMIT the awaited expression there, so anything but a call/method-call ran
// twice: `await tasks[next()]` could observe a side effect twice and, worse,
// resume on a DIFFERENT task than it suspended on, reading an unresolved
// result slot. Every non-`Var` await now gets a frame slot and reloads it.
// Perspectives: 1 an index await calls its subscript once, 2 it yields the
// value of the task it actually awaited, 3 a field-read await resumes on the
// same task, 4 a ternary await likewise, 5 a `TaskResult<T>` hoisted into a
// local is awaitable, 6 it is selectable, 7 it maps a cancelled task to
// `Err`, 8 the plain-`Task` select case still binds bare `T`, 9 a
// `JoinHandle<T>` local is awaitable, 10 a `JoinHandle<T>` is selectable,
// 11 a user `result()` method returning Task stays plain in select, 12 a user
// method returning TaskResult is evaluated before a cancellation-aware await.

#[test]
fn tone_01_indexed_await_evaluates_the_subscript_once() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\n\
         async fn w(n: i64) -> i64 { await sleep(n); return n; }\n\
         fn pick(i: i64) -> i64 { println(\"pick\"); return i; }\n\
         async fn main() { let tasks: Array<Task<i64>> = [w(20), w(1)]; let v = await tasks[pick(0)]; println(v); }",
    );
    assert!(ok, "{out}");
    // One "pick" — not one per resume.
    assert_eq!(out, "pick\n20\n");
}

#[test]
fn tone_02_indexed_await_yields_the_task_it_suspended_on() {
    // The subscript would select a different (unresolved) task if it were
    // re-evaluated after the resume, so the value pins the identity.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\n\
         async fn w(n: i64) -> i64 { await sleep(n); return n; }\n\
         async fn main() { let mut i = 0; let tasks: Array<Task<i64>> = [w(20), w(1)]; \
         let v = await tasks[i]; i = 1; println(v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

#[test]
fn tone_03_field_read_await_resumes_on_the_same_task() {
    let (out, ok) = compile_and_run(
        "class Holder { pub t: Task<i64>; }\n\
         async fn w(n: i64) -> i64 { await sleep(n); return n; }\n\
         async fn main() { let h = new Holder(w(20)); println(await h.t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

#[test]
fn tone_04_ternary_await_resumes_on_the_same_task() {
    let (out, ok) = compile_and_run(
        "async fn w(n: i64) -> i64 { await sleep(n); return n; }\n\
         async fn main() { let a = w(20); let b = w(1); let flag = true; \
         println(await (flag ? a : b)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

#[test]
fn tone_05_task_result_in_a_local_is_awaitable() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         async fn main() { let t = w(); let view = t.result(); \
         match await view { Ok(v) => println(v), Err(e) => println(-1), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tone_06_task_result_in_a_local_is_selectable() {
    // Through a local the case must reach the same lowering as an inline
    // `.result()` via the awaited type.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         async fn main() { let t = w(); let view = t.result(); \
         select { let r = await view => { match r { Ok(v) => println(v), Err(e) => println(-1), } } \
         sleep(500) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tone_07_task_result_local_maps_cancellation_to_err() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(200); return 7; }\n\
         async fn main() { let t = w(); let view = t.result(); t.cancel(); \
         match await view { Ok(v) => println(v), Err(e) => println(-1), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn tone_08_plain_task_select_case_still_binds_the_bare_value() {
    // The type-driven decision must not make every case cancel-aware.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         async fn main() { let t = w(); \
         select { let v = await t => { println(v + 1); } sleep(500) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn tone_09_join_handle_is_awaitable() {
    // E0812 tells users to migrate `join()` to `await task`; `JoinHandle<T>` is
    // the same handle under a legacy name, so it must await.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         async fn main() { let h: JoinHandle<i64> = w(); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tone_10_join_handle_is_selectable() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         async fn main() { let h: JoinHandle<i64> = w(); \
         select { let v = await h => { println(v); } sleep(500) => { println(\"late\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tone_11_user_result_method_returning_task_stays_plain_in_select() {
    // The method name alone must not make this cancellation-aware. Its return
    // type is Task<i64>, so the binding is the bare i64 value.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         class Wrapper { pub fn result(self) -> Task<i64> { return w(); } }\n\
         async fn main() { let wrapper = new Wrapper(); \
         select { let v = await wrapper.result() => { println(v + 1); } \
         sleep(500) => { println(0); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn tone_12_user_method_returning_task_result_is_evaluated_before_await() {
    // Cancellation-awareness comes from TaskResult<i64>, but the method call
    // itself must still run; the Wrapper receiver is not the task frame.
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { await sleep(1); return 7; }\n\
         class Wrapper { pub fn view(self) -> TaskResult<i64> { return w().result(); } }\n\
         async fn main() { let wrapper = new Wrapper(); \
         match await wrapper.view() { Ok(v) => println(v + 1), Err(e) => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

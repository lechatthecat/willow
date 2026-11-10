use super::*;
use crate::{lexer::Lexer, parser::Parser, semantic::TypeChecker};

fn analyze(source: &str) -> Analysis {
    let (program, parse) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(parse.is_empty(), "{source}: {parse:?}");
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    assert!(
        checker
            .errors
            .iter()
            .all(|d| d.code == ErrorCode::E2607 || d.severity == Severity::Warning),
        "{source}: {:?}",
        checker.errors
    );
    let result = check(&program, &checker.expr_types, &checker.static_call_classes);
    assert_eq!(
        checker
            .errors
            .iter()
            .filter(|d| d.code == ErrorCode::E2607)
            .count(),
        result.diagnostics.len()
    );
    for d in &result.diagnostics {
        assert_eq!(d.severity, Severity::Error);
        assert!(d.labels.len() >= 2, "missing witness: {d:?}");
        assert!(d.labels.iter().any(|l| l.message.contains("witness")));
    }
    result
}

const WORKER: &str =
    "async fn waiter(inbox: Channel<Task<void>>) { let task = inbox.recv(); await task; }";
fn self_wait(suffix: &str) -> String {
    format!(
        "{WORKER} async fn main() {{ let ch = Channel<Task<void>>::new(); let t = waiter(ch); ch.send(t); {suffix} }}"
    )
}

macro_rules! case {
    ($name:ident, $source:expr, $errors:expr) => {
        #[test]
        fn $name() {
            assert_eq!(analyze(&$source).diagnostics.len(), $errors);
        }
    };
}

case!(
    receive_only,
    "fn main() { let ch = Channel<i64>::new(); let x = ch.recv(); }",
    1
);
case!(
    bounded_receive_only,
    "fn main() { let ch = Channel<i64>::with_capacity(1); println(ch.recv()); }",
    1
);
case!(
    optional_receive_only,
    "fn main() { let ch = Channel<i64>::new(); let x = ch.recv_opt(); }",
    1
);
case!(
    unused_helper_is_unknown,
    "fn receive() -> i64 { let ch = Channel<i64>::new(); return ch.recv(); } fn main() {}",
    0
);
case!(
    alias_receive,
    "fn main() { let ch = Channel<i64>::new(); let alias = ch; alias.recv(); }",
    1
);
case!(
    distinct_channel_identities,
    "fn main() { let ch = Channel<i64>::new(); ch.send(1); let other = Channel<i64>::new(); other.recv(); }",
    1
);
case!(self_await, self_wait("await t;"), 1);
case!(self_result_await, self_wait("await t.result();"), 1);
case!(
    worker_result_view,
    "async fn waiter(ch: Channel<Task<void>>) { let t = ch.recv(); let view = t.result(); await view; } async fn main() { let c = Channel<Task<void>>::new(); let t = waiter(c); c.send(t); await t; }",
    1
);
case!(
    cyclic_await,
    format!(
        "{WORKER} async fn main() {{ let a = Channel<Task<void>>::new(); let b = Channel<Task<void>>::new(); let x = waiter(a); let y = waiter(b); a.send(y); b.send(x); await x; }}"
    ),
    1
);
case!(
    awaited_worker_without_sender,
    "async fn worker(ch: Channel<i64>) { ch.recv(); } async fn main() { let ch = Channel<i64>::new(); let t = worker(ch); await t; }",
    1
);
case!(
    sender_in_other_task,
    "async fn producer(ch: Channel<i64>) { ch.send(1); } async fn main() { let ch = Channel<i64>::new(); let p = producer(ch); println(ch.recv()); await p; }",
    0
);
case!(
    same_task_sender,
    "fn main() { let ch = Channel<i64>::new(); ch.send(1); println(ch.recv()); }",
    0
);
case!(
    alias_sender,
    "fn main() { let ch = Channel<i64>::new(); let alias = ch; alias.send(1); ch.recv(); }",
    0
);
case!(
    closed_receive,
    "fn main() { let ch = Channel<i64>::new(); ch.close(); ch.recv(); }",
    0
);
case!(
    closed_optional_receive,
    "fn main() { let ch = Channel<i64>::new(); ch.close(); ch.recv_opt(); }",
    0
);
case!(
    close_in_other_task,
    "async fn closer(ch: Channel<i64>) { ch.close(); } async fn main() { let ch = Channel<i64>::new(); let t = closer(ch); ch.recv_opt(); await t; }",
    0
);
case!(
    select_timeout,
    "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { println(v); } sleep(1) => {} } }",
    0
);
case!(
    select_default,
    "fn main() { let ch = Channel<i64>::new(); select { let v = ch.recv() => {} default => {} } }",
    0
);
case!(
    task_select_timeout,
    self_wait("select { let v = await t => {} sleep(1) => {} }"),
    0
);
case!(
    cancelled_task,
    self_wait("t.cancel(); await t.result();"),
    0
);
case!(detached_cycle, self_wait(""), 0);
case!(
    unknown_parameter,
    "fn receive(ch: Channel<i64>) { ch.recv(); } fn main() {}",
    0
);
case!(
    escaped_to_function,
    "fn consume(ch: Channel<i64>) { ch.send(1); } fn main() { let ch = Channel<i64>::new(); consume(ch); ch.recv(); }",
    0
);
case!(
    escaped_to_collection,
    "import std::collections::Array; fn main() { let ch = Channel<i64>::new(); let values = [ch]; values[0].send(1); ch.recv(); }",
    0
);
case!(
    function_value,
    "fn produce(ch: Channel<i64>) { ch.send(1); } fn main() { let ch = Channel<i64>::new(); let f = produce; f(ch); ch.recv(); }",
    0
);
case!(
    conditional_receive,
    "fn main() { let ch = Channel<i64>::new(); if false { ch.recv(); } }",
    0
);
case!(
    unreachable_receive,
    "fn main() { let ch = Channel<i64>::new(); return; ch.recv(); }",
    0
);
case!(
    returned_channel,
    "fn make() -> Channel<i64> { let ch = Channel<i64>::new(); return ch; } fn main() { let ch = make(); ch.recv(); }",
    0
);
case!(
    namesake_method,
    "class Inbox { pub fn recv(self) -> i64 { return 1; } } fn main() { let ch = new Inbox(); println(ch.recv()); }",
    0
);
case!(
    unknown_prior_await,
    "async fn worker(t: Task<void>) { await t; let ch = Channel<i64>::new(); ch.recv(); } fn main() {}",
    0
);
case!(
    ambiguous_task_delivery,
    self_wait("ch.send(t); await t;"),
    0
);
case!(
    close_breaks_cycle,
    self_wait("ch.close(); await t.result();"),
    0
);

#[test]
fn linear_operation_counts_for_channels_tasks_and_wait_edges() {
    for n in [8, 32, 128, 512] {
        let mut source = format!("{WORKER} async fn main() {{");
        for i in 0..n {
            source.push_str(&format!(
                "let c{i} = Channel<Task<void>>::new(); let t{i} = waiter(c{i});"
            ));
        }
        for i in 0..n {
            source.push_str(&format!("c{i}.send(t{});", (i + 1) % n));
        }
        source.push_str("await t0; }");
        let result = analyze(&source);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.counts.channels, n);
        assert_eq!(result.counts.tasks, n + 1);
        assert_eq!(result.counts.waits, 2 * n + 1);
        assert_eq!(result.counts.operations, 5 * n + 1);
        assert!(result.counts.graph_steps <= 4 * n + 8);
        println!("ring n={n}: {:?}", result.counts);
    }
}

#[test]
fn repeated_calls_do_not_repeat_scalar_body_walks() {
    for n in [8, 32, 128] {
        let mut source = "async fn leaf(ch: Channel<i64>) {".to_owned();
        for i in 0..n {
            source.push_str(&format!("let x{i} = 1;"));
        }
        source.push_str("ch.send(1); } async fn main() { let ch = Channel<i64>::new();");
        for i in 0..n {
            source.push_str(&format!("let t{i} = leaf(ch);"));
        }
        source.push_str("ch.recv(); }");
        let result = analyze(&source);
        assert!(result.diagnostics.is_empty());
        assert!(result.counts.syntax < 60 * n + 100);
        assert!(result.counts.operations <= 2 * n + 4);
        println!("fanout n={n}: {:?}", result.counts);
    }
}

case!(
    namesake_static_factory,
    "class Factory { pub static fn new() -> Channel<i64> { let ch = Channel<i64>::new(); ch.send(1); return ch; } } fn main() { let ch = Factory::new(); ch.recv(); }",
    0
);
case!(
    bounded_send_before_cycle,
    "async fn waiter(ch: Channel<Task<void>>) { let t = ch.recv(); await t; } async fn main() { let c = Channel<Task<void>>::with_capacity(1); let t = waiter(c); c.send(t); await t; }",
    0
);

#[test]
fn ownership_errors_keep_the_wait_witness() {
    let source = self_wait("await t;");
    let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
    assert!(errors.is_empty());
    let mut checker = TypeChecker::new();
    checker.set_enforce_send_sync(true);
    checker.check_program(&program);
    assert!(checker.errors.iter().any(|d| d.code == ErrorCode::E2408));
    assert!(checker.errors.iter().any(|d| d.code == ErrorCode::E2607));
    assert!(
        checker
            .errors
            .iter()
            .all(|d| matches!(d.code, ErrorCode::E2408 | ErrorCode::E2607))
    );
}

#[test]
fn type_errors_do_not_trigger_wait_cascades() {
    let source = "fn main() { let ch = Channel<i64>::new(); ch.send(true); ch.recv(); }";
    let (program, _) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    assert!(checker.errors.iter().any(|d| d.severity == Severity::Error));
    assert!(!checker.errors.iter().any(|d| d.code == ErrorCode::E2607));
}

case!(
    annotated_channel,
    "fn main() { let ch: Channel<i64> = Channel::new(); ch.recv(); }",
    1
);
case!(
    loop_is_unknown,
    "fn main() { let ch = Channel<i64>::new(); while false { ch.recv(); } }",
    0
);
case!(
    assignment_is_unknown,
    "fn main() { let mut ch = Channel<i64>::new(); ch = Channel<i64>::new(); ch.send(1); ch.recv(); }",
    0
);
case!(
    worker_timeout_breaks_cycle,
    "async fn waiter(ch: Channel<Task<void>>) { let t = ch.recv(); select { let v = await t => {} sleep(1) => {} } } async fn main() { let c = Channel<Task<void>>::new(); let t = waiter(c); c.send(t); await t; }",
    0
);

#[test]
fn independent_channel_and_wait_dimensions() {
    for channels in [8, 32, 128] {
        for waits in [1, 8, 32] {
            let mut source = "fn main() {".to_owned();
            for i in 0..channels {
                source.push_str(&format!("let c{i} = Channel<i64>::new();"));
            }
            for _ in 0..waits {
                source.push_str("c0.recv();");
            }
            source.push('}');
            let result = analyze(&source);
            assert_eq!(result.diagnostics.len(), 1);
            assert_eq!(result.counts.channels, channels);
            assert_eq!(result.counts.waits, waits);
            assert_eq!(result.counts.operations, channels + waits);
            assert_eq!(result.counts.graph_steps, 2);
            println!("channels={channels}, waits={waits}: {:?}", result.counts);
        }
    }
}

case!(
    closed_send_prevents_later_wait,
    "fn main() { let closed = Channel<i64>::new(); closed.close(); closed.send(1); let empty = Channel<i64>::new(); empty.recv(); }",
    0
);
case!(
    peer_close_may_prevent_later_wait,
    "async fn closer(ch: Channel<i64>) { ch.close(); } async fn main() { let closed = Channel<i64>::new(); let t = closer(closed); closed.send(1); let empty = Channel<i64>::new(); empty.recv(); }",
    0
);
case!(
    outer_timeout_allows_local_blocked_task,
    "async fn blocked() { let c = Channel<i64>::new(); c.recv(); } async fn main() { let t = blocked(); select { let v = await t => {} sleep(1) => {} } }",
    0
);
case!(
    outer_timeout_allows_sync_helper,
    "fn blocked() { let c = Channel<i64>::new(); c.recv(); } async fn wrapper() { blocked(); } async fn main() { let t = wrapper(); select { let v = await t => {} sleep(1) => {} } }",
    0
);
case!(
    outer_cancellation_allows_local_blocked_task,
    "async fn blocked() { let c = Channel<i64>::new(); c.recv(); } async fn main() { let t = blocked(); t.cancel(); await t.result(); }",
    0
);
case!(
    detached_local_blocked_task,
    "async fn blocked() { let c = Channel<i64>::new(); c.recv(); } async fn main() { let t = blocked(); }",
    0
);
case!(
    awaited_local_blocked_task,
    "async fn blocked() { let c = Channel<i64>::new(); c.recv(); } async fn main() { let t = blocked(); await t; }",
    1
);

case!(
    direct_sync_leaf_without_sender,
    "fn blocked() { let c = Channel<i64>::new(); c.recv(); } fn main() { blocked(); }",
    1
);
case!(
    sync_return_receive,
    "fn receive(ch: Channel<i64>) -> i64 { return ch.recv(); } fn main() { let ch = Channel<i64>::new(); println(receive(ch)); }",
    1
);

case!(
    zero_capacity_panics_before_receive,
    "fn main() { let c = Channel<i64>::with_capacity(0); c.recv(); }",
    0
);
case!(
    unknown_capacity_is_not_proven,
    "fn main() { let size = 0; let c = Channel<i64>::with_capacity(size); c.recv(); }",
    0
);
case!(
    parameter_capacity_is_not_proven,
    "fn receive(size: i64) { let c = Channel<i64>::with_capacity(size); c.recv(); } fn main() { receive(0); }",
    0
);

#[test]
fn imported_main_is_not_a_process_root() {
    let source = "pub fn main() { let c = Channel<i64>::new(); c.recv(); }";
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty());
    let mut checker = TypeChecker::new();
    checker.check_module_program(&program);
    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
}

#[test]
fn repeated_send_summaries_saturate_before_instantiation() {
    for tasks in [8, 32, 128] {
        for sends in [8, 32, 128] {
            let mut source = "async fn producer(ch: Channel<i64>) {".to_owned();
            source.push_str(&"ch.send(1);".repeat(sends));
            source.push_str("} async fn main() { let ch = Channel<i64>::new();");
            for i in 0..tasks {
                source.push_str(&format!("let t{i} = producer(ch);"));
            }
            source.push_str("ch.recv(); }");
            let result = analyze(&source);
            assert!(result.diagnostics.is_empty());
            assert_eq!(result.counts.channels, 1);
            assert_eq!(result.counts.waits, 1);
            assert_eq!(result.counts.send_checks, 2 * tasks);
            assert_eq!(result.counts.operations, 3 * tasks + 2);
            assert!(result.counts.syntax < 20 * (tasks + sends) + 100);
            println!(
                "repeated sends tasks={tasks}, sends={sends}: {:?}",
                result.counts
            );
        }
    }
}
case!(
    later_send_arguments_are_still_analyzed,
    "fn main() { let out = Channel<i64>::new(); let input = Channel<i64>::new(); out.send(1); out.send(2); out.send(input.recv()); }",
    1
);

//! Diagnostic provenance for task-local function values. Retain one witness,
//! not the transitive set of captures. Expression snapshots share that witness;
//! direct binding facts provide provisional labels during checking. Final
//! execution-order labels are resolved by `capture_flow` before caching a body.
use super::*;

/// Mutable binding facts are distinct from immutable expression snapshots.
/// Persistent roots make branch entry O(1); diff skips shared tree nodes.
pub(super) type CaptureBindings = im::OrdMap<Span, Option<std::rc::Rc<Label>>>;

impl TypeChecker {
    pub(super) fn end_capture_scope(&mut self, block: &Block) {
        // Local facts cannot be read outside this scope. Keeping them in each
        // ancestor's branch delta would multiply deeply nested declarations.
        // Escaping values already own a shared snapshot of their witness.
        for stmt in &block.stmts {
            if let Stmt::Let(binding) = stmt
                && self
                    .local
                    .capture_flow
                    .writes
                    .contains_key(&binding.init.span())
            {
                self.local.capture_bindings.remove(&binding.span);
            }
        }
    }

    pub(super) fn record_callable_join(&mut self, expr: &Expr, ty: &Type) {
        if matches!(ty, Type::Fn(..) | Type::Closure(..)) {
            self.local.capture_flow.callables.insert(expr.span());
        }
        if !matches!(ty, Type::Closure(..)) {
            return;
        }
        match expr {
            Expr::Ternary(branch) => {
                self.local.callable_causes.remove(&branch.span);
                self.merge_capture_cause(branch.then_expr.span(), branch.span);
                self.merge_capture_cause(branch.else_expr.span(), branch.span);
            }
            Expr::Match(branch) => {
                self.local.callable_causes.remove(&branch.span);
                for arm in &branch.arms {
                    if let MatchBody::Expr(value) = &arm.body {
                        self.merge_capture_cause(value.span(), branch.span);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn copy_capture_cause(&mut self, source: Span, target: Span) {
        #[cfg(test)]
        CAUSE_COPIES.with(|count| count.set(count.get() + 1));
        if let Some(cause) = self.local.callable_causes.get(&source).cloned() {
            self.local.callable_causes.insert(target, cause);
        } else {
            self.local.callable_causes.remove(&target);
        }
    }

    fn merge_capture_cause(&mut self, source: Span, target: Span) {
        if !self.local.callable_causes.contains_key(&target) {
            self.copy_capture_cause(source, target);
        }
    }

    pub(super) fn bind_capture_cause(&mut self, source: Span, target: Span, ty: &Type) {
        if matches!(ty, Type::Fn(..) | Type::Closure(..)) {
            self.local.capture_flow.writes.insert(source, target);
            let cause = self.local.callable_causes.get(&source).cloned();
            self.local.capture_bindings.insert(target, cause);
        }
    }

    pub(super) fn with_capture_cause(&self, mut diagnostic: Diagnostic) -> Diagnostic {
        if !matches!(diagnostic.code, ErrorCode::E2402 | ErrorCode::E2408) {
            return diagnostic;
        }
        let Some(primary) = diagnostic.primary_span() else {
            return diagnostic;
        };
        // A cached body's witness was resolved at its own execution point.
        // Replaying it must not append the caller's provisional binding fact.
        if diagnostic
            .labels
            .iter()
            .any(|label| label.message.starts_with("captured variable `"))
            || diagnostic
                .notes
                .iter()
                .any(|note| note.starts_with("captured variable `"))
        {
            return diagnostic;
        }
        let cause = if let Some(resolved) = self.local.capture_flow.resolved.get(&primary) {
            resolved.as_ref()
        } else {
            self.local
                .capture_bindings
                .get(&primary)
                .map_or_else(|| self.local.callable_causes.get(&primary), Option::as_ref)
        };
        if let Some(cause) = cause {
            if cause.span.file_id == primary.file_id {
                if !diagnostic.labels.contains(cause) {
                    diagnostic.labels.push((**cause).clone());
                }
            } else if !diagnostic.notes.contains(&cause.message) {
                diagnostic.notes.push(cause.message.clone());
            }
        }
        diagnostic
    }
}

#[cfg(test)]
thread_local! {
    static CAUSE_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::super::capture_flow::{FLOW_DIFFS, FLOW_NODES, FLOW_STEPS};
    use super::*;

    const DECLS: &str = "import std::collections::Array;
async fn run(f: fn() -> i64) -> i64 { return f(); }
fn identity(f: fn() -> i64) -> fn() -> i64 { return f; }
fn make(a: Array<i64>) -> fn() -> i64 { return || a.len(); }
class Holder { pub f: fn() -> i64; }
interface Shared extends Sync {}
class SharedHolder implements Shared { pub f: fn() -> i64; }
class Value { pub n: i64; }
fn one() -> i64 { return 1; }
";

    fn check(body: &str) -> Vec<Diagnostic> {
        let source = format!("{DECLS}\nfn main() {{ {body} }}");
        check_source(&source)
    }

    fn check_source(source: &str) -> Vec<Diagnostic> {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{source}: {errors:?}");
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        checker.errors
    }

    #[test]
    fn capture_provenance_match_guards_bindings_and_calls() {
        for (body, expected) in [
            ("observe(&f); let _ = run(f);", "a"),
            ("while true { replace(&f); break; } let _ = run(f);", ""),
            (
                "let writer = || { f = || b.len(); }; let opaque = || { one(); }; invoke(writer); f = || a.len(); opaque(); let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = || b.len(); }; let opaque = || { f = || a.len(); one(); }; invoke(writer); opaque(); let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = || b.len(); }; let opaque = || { one(); f = || a.len(); }; invoke(writer); opaque(); let _ = run(f);",
                "a",
            ),
            (
                "let writer = || { f = || b.len(); }; let opaque = || { f = || a.len(); one(); }; opaque(); let _ = run(f);",
                "a",
            ),
            (
                "let writer = || { f = || b.len(); }; let opaque = || { f = || a.len(); one(); }; let wrapper = || { opaque(); opaque(); }; invoke(writer); wrapper(); let _ = run(f);",
                "",
            ),
            ("replace(&f); let _ = run(f);", ""),
            (
                "let writer = || { f = || b.len(); }; let callbacks = [writer]; invoke_all(callbacks); let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = || b.len(); }; let callbacks = new Callbacks(|| {}); callbacks.callback = writer; invoke_holder(callbacks); let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = one; }; match true { true if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; let save = || { let old = f; invoke(writer); return old; }; let g = save(); let _ = run(g);",
                "a",
            ),
            (
                "let writer = |g: fn() -> i64| { f = g; }; if true { defer writer(f); f = || b.len(); } let _ = run(f);",
                "a",
            ),
            (
                "let mut writer = || { f = || b.len(); }; if true { defer writer(); writer = || { f = one; }; } let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; let factory = || { return writer; }; let g = factory(); g(); let _ = run(f);",
                "b",
            ),
            (
                "match true { true if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; let _ = run(f);",
                "b",
            ),
            ("match f { g => { let _ = run(g); } };", "a"),
            (
                "match true { _ if let g = f => { let _ = run(g); }, _ => {} };",
                "a",
            ),
            (
                "let writer = || { f = || b.len(); }; writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; let alias = writer; alias(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; match writer { g => { g(); } }; let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = one; }; writer(); let _ = run(f);",
                "",
            ),
            (
                "match f { g if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, h => { let _ = run(h); } };",
                "a",
            ),
            (
                "match f { g if let h = g => { let _ = run(h); }, _ => {} };",
                "a",
            ),
            (
                "match true { _ if let g = f && let h = g => { let _ = run(h); }, _ => {} };",
                "a",
            ),
            (
                "match true { _ if let g = f && reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; let _ = run(f);",
                "b",
            ),
            (
                "match true { _ if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ if false => { f = || a.len(); }, _ => {} }; let _ = run(f);",
                "b",
            ),
            (
                "match false { true if reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; let _ = run(f);",
                "a",
            ),
            (
                "match true { _ if false && reject(match true { true => { f = || b.len(); }, false => {} }) => {}, _ => {} }; let _ = run(f);",
                "a",
            ),
            (
                "let saved = f; let writer = || { f = || b.len(); }; writer(); let _ = run(saved);",
                "a",
            ),
            (
                "let writer = || { f = || b.len(); }; let wrapper = || { writer(); }; wrapper(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; writer(); let _ = f(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; defer writer(); let _ = run(f);",
                "a",
            ),
            (
                "let writer = || { f = || b.len(); }; if true { defer writer(); } let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { defer { f = || b.len(); } }; writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { if true { f = || b.len(); return; } f = || a.len(); }; writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { while true { f = || b.len(); break; } }; writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { for i in 0..1 { f = || b.len(); break; } }; f = one; writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; f = one; while true { writer(); break; } let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; f = one; for i in 0..1 { writer(); break; } let _ = run(f);",
                "b",
            ),
            (
                "let mut g = f; let writer = || { f = g; }; g = || b.len(); writer(); let _ = run(f);",
                "b",
            ),
            (
                "let writer = |g: fn() -> i64| { f = g; }; writer(|| b.len()); let _ = run(f);",
                "b",
            ),
            (
                "let writer = || { f = || b.len(); }; let wrapper = || { invoke(writer); }; wrapper(); let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = || b.len(); }; if true { defer invoke(writer); } let _ = run(f);",
                "",
            ),
            (
                "let writer = || { f = || b.len(); }; invoke(writer); let _ = run(f);",
                "",
            ),
        ] {
            assert_capture_call_scenario(body, expected);
        }
    }

    fn assert_capture_call_scenario(body: &str, expected: &str) {
        let source = format!(
            "{DECLS}\nfn reject(value: void) -> bool {{ return false; }}\nfn invoke(f: fn()) {{ f(); }}\nfn replace(f: &mut fn() -> i64) {{ f = one; }}\nfn observe(f: &fn() -> i64) {{ }}\nfn invoke_all(callbacks: Array<fn()>) {{ for callback in callbacks {{ callback(); }} }}\nclass Callbacks {{ pub callback: fn(); }}\nfn invoke_holder(callbacks: Callbacks) {{ let callback = callbacks.callback; callback(); }}\nfn main() {{ let a = [1]; let b = [2]; let mut f = || a.len(); {body} }}"
        );
        let errors = check_source(&source);
        assert!(
            errors
                .iter()
                .all(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)),
            "{body}: {errors:?}"
        );
        let transfers: Vec<_> = errors
            .iter()
            .filter(|d| d.code == ErrorCode::E2402)
            .collect();
        assert_eq!(transfers.len(), 1, "{body}: {errors:?}");
        let labels: Vec<_> = transfers[0]
            .labels
            .iter()
            .filter(|l| l.message.starts_with("captured variable `"))
            .collect();
        if expected.is_empty() {
            assert!(labels.is_empty(), "{body}: {labels:?}");
        } else {
            assert_eq!(labels.len(), 1, "{body}: {labels:?}");
            assert!(
                labels[0]
                    .message
                    .contains(&format!("captured variable `{expected}`")),
                "{body}: {labels:?}"
            );
            assert!(
                source[labels[0].span.start..labels[0].span.end]
                    .starts_with(&format!("let {expected} =")),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn capture_call_summaries_bound_repeated_and_nested_calls() {
        use super::super::capture_flow::FLOW_SUMMARY_WORK;
        for symbolic in [false, true] {
            for size in [8, 16, 32, 64] {
                let mut body = String::from(
                    "let a = [1]; let b = [2]; let mut f = || a.len(); let mut g = || b.len(); let choice = true; let writer = || { g = one; }; ",
                );
                body.push_str(if symbolic {
                    "let w0 = || { if choice { f = g; } }; "
                } else {
                    "let w0 = || { if choice { f = || b.len(); } }; "
                });
                for i in 1..=size {
                    body.push_str(&format!(
                        "let w{i} = || {{ w{}(); w{}(); }}; ",
                        i - 1,
                        i - 1
                    ));
                }
                for _ in 0..size {
                    body.push_str(&format!("w{size}(); "));
                }
                body.push_str("let _ = run(f);");
                FLOW_NODES.with(|n| n.set(0));
                FLOW_STEPS.with(|n| n.set(0));
                FLOW_SUMMARY_WORK.with(|n| n.set(0));
                let errors = check(&body);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert_eq!(errors[0].code, ErrorCode::E2402);
                assert!(
                    errors[0]
                        .labels
                        .iter()
                        .any(|l| l.message.starts_with("captured variable `")),
                    "{errors:?}"
                );
                let nodes = FLOW_NODES.with(|n| n.get());
                let steps = FLOW_STEPS.with(|n| n.get());
                let work = FLOW_SUMMARY_WORK.with(|n| n.get());
                eprintln!(
                    "call summaries symbolic={symbolic}, size={size}, nodes={nodes}, steps={steps}, normalize={work}"
                );
                assert!(nodes <= 32 * size + 64, "{nodes}");
                assert!(steps <= 32 * size + 64, "{steps}");
                assert!(work <= 32 * size + 64, "{work}");
            }
        }
    }

    #[test]
    fn capture_provenance_obeys_deferred_and_abrupt_execution() {
        for (body, expected) in [
            (
                "match false { true => { f = || b.len(); }, false => { f = || c.len(); } }; let _ = run(f);",
                vec!["c"],
            ),
            (
                "match choice { true if false => { f = || b.len(); }, _ => { f = || c.len(); } }; let _ = run(f);",
                vec!["c"],
            ),
            (
                "let _ = choice && match true { true => { f = || b.len(); return; }, false => false }; let _ = run(f);",
                vec!["a"],
            ),
            (
                "let _ = choice || match true { true => { f = || b.len(); return; }, false => false }; let _ = run(f);",
                vec!["a"],
            ),
            ("defer { f = || b.len(); } let _ = run(f);", vec!["a"]),
            (
                "if true { defer { f = || b.len(); } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if true { defer { f = || b.len(); } let _ = run(f); } let _ = run(f);",
                vec!["a", "b"],
            ),
            (
                "if true { defer { f = || b.len(); } f = || c.len(); let _ = run(f); } let _ = run(f);",
                vec!["c", "b"],
            ),
            (
                "if true { defer { f = || b.len(); } defer { f = || c.len(); } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if true { defer { if true { defer { f = || b.len(); } f = || c.len(); } } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "let mut g = f; if true { defer { f = g; } g = || b.len(); } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if true { defer match choice { true => { f = || b.len(); }, false => { f = || b.len(); } } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "defer match choice { true => { f = || b.len(); }, false => { f = || c.len(); } } let _ = run(f);",
                vec!["a"],
            ),
            (
                "if choice { defer { f = || b.len(); } return; } let _ = run(f);",
                vec!["a"],
            ),
            (
                "while choice { f = || b.len(); return; } let _ = run(f);",
                vec!["a"],
            ),
            (
                "for i in 0..2 { f = || b.len(); return; } let _ = run(f);",
                vec!["a"],
            ),
            (
                "f = one; while true { if true { f = || b.len(); break; } else { f = one; } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; for i in 0..2 { if true { f = || b.len(); break; } else { f = one; } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "while true { f = || b.len(); break; f = || c.len(); } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; while choice { f = || b.len(); continue; f = || c.len(); } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; for i in 0..2 { f = || b.len(); continue; f = || c.len(); } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; while choice { if choice { f = || b.len(); return; } else { f = || c.len(); continue; } } let _ = run(f);",
                vec!["c"],
            ),
            (
                "f = one; for i in 0..2 { if choice { f = || b.len(); return; } else { f = || c.len(); continue; } } let _ = run(f);",
                vec!["c"],
            ),
            (
                "f = one; while true { if choice { f = || b.len(); continue; } break; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; while choice { let _ = run(f); f = || b.len(); continue; }",
                vec!["b"],
            ),
            (
                "f = one; for i in 0..2 { let _ = run(f); f = || b.len(); continue; }",
                vec!["b"],
            ),
            (
                "while true { defer { f = || b.len(); } break; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; while choice { defer { f = || b.len(); } continue; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; for i in 0..2 { defer { f = || b.len(); } continue; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "while choice { defer { f = || b.len(); } return; } let _ = run(f);",
                vec!["a"],
            ),
            (
                "while true { defer { f = || b.len(); } if choice { break; } defer { f = || c.len(); } break; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "while true { if choice { f = || b.len(); break; } defer { f = || c.len(); } break; } let _ = run(f);",
                vec!["bc"],
            ),
            (
                "while true { while true { f = || b.len(); break; } f = || c.len(); break; } let _ = run(f);",
                vec!["c"],
            ),
            (
                "while true { while choice { f = || b.len(); continue; } f = || c.len(); break; } let _ = run(f);",
                vec!["c"],
            ),
            (
                "f = one; while choice { while choice { f = || b.len(); return; } f = || c.len(); break; } let _ = run(f);",
                vec!["c"],
            ),
            (
                "while true { match choice { true => { f = || b.len(); break; }, false => { f = || c.len(); return; } }; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "f = one; while choice { select { channel.recv() => { f = || b.len(); break; } default => { f = || c.len(); return; } } } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if true { let saved = f; defer { f = || b.len(); } let _ = run(saved); } let _ = run(f);",
                vec!["a", "b"],
            ),
            (
                "let g = || { let a = [1]; let b = [2]; let mut f = || a.len(); defer { f = || b.len(); } let _ = run(f); }; g();",
                vec!["a"],
            ),
            (
                "let g = || { let a = [1]; let b = [2]; let mut f = || a.len(); f = one; while true { f = || b.len(); break; } let _ = run(f); }; g();",
                vec!["b"],
            ),
        ] {
            let body = format!(
                "let a = [1]; let b = [2]; let c = [3]; let mut f = || a.len(); let choice = true; let channel = Channel<i64>::new(); {body}"
            );
            let source = format!("{DECLS}\nfn main() {{ {body} }}");
            let errors = check(&body);
            assert!(
                errors
                    .iter()
                    .all(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)),
                "{body}: {errors:?}"
            );
            let transfers: Vec<_> = errors
                .iter()
                .filter(|d| d.code == ErrorCode::E2402)
                .collect();
            assert_eq!(transfers.len(), expected.len(), "{body}: {errors:?}");
            for (error, candidates) in transfers.into_iter().zip(expected) {
                let causes: Vec<_> = error
                    .labels
                    .iter()
                    .filter(|l| l.message.starts_with("captured variable"))
                    .collect();
                assert_eq!(causes.len(), 1, "{body}: {error:?}");
                assert!(
                    candidates.chars().any(|name| causes[0]
                        .message
                        .contains(&format!("captured variable `{name}`"))
                        && source[causes[0].span.start..causes[0].span.end]
                            .starts_with(&format!("let {name} ="))),
                    "{body}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn shareable_captures_cross_async_and_channel_boundaries() {
        for body in [
            "let n = 4; let _ = run(|| n);",
            "let n = 4; let f = || n; let _ = run(f);",
            "let s = \"hello\"; let _ = run(|| s.len());",
            "let a = [1, 2].freeze(); let _ = run(|| a.len());",
            "let a = AtomicI64::new(4); let _ = run(|| a.load());",
            "let v = new Value(4); let _ = run(|| v.n);",
            "let f = one; let _ = run(|| f());",
            "let n = 4; let f = || n; let g = || f(); let _ = run(g);",
            "let n = 4; let f: fn() -> i64 = || n; let _ = run(f);",
            "let n = 4; let h = new Holder(|| n); let _ = run(h.f);",
            "let n = 4; let h = new SharedHolder(|| n); let _ = run(h.f);",
            "let n = 4; let c = Channel<fn() -> i64>::new(); c.send(|| n);",
            "let n = 4; let c = Channel<fn() -> i64>::new(); let f = || n; c.send(f);",
            "let a = [1].freeze(); let c = Channel<fn() -> i64>::new(); c.send(|| a.len());",
            "let n = 4; let c = Channel<fn() -> i64>::new(); select { c.send(|| n) => {} }",
            "let n = 4; let c = Channel<fn() -> i64>::new(); c.send(|| n); let f = c.recv(); let _ = run(f);",
            "let n = 4; let f = if true { || n } else { one }; let _ = run(f);",
        ] {
            let errors = check(body);
            assert!(errors.is_empty(), "{body}: {errors:?}");
        }
    }

    #[test]
    fn rejected_captures_name_the_origin_through_aliases() {
        for (body, name) in [
            (
                "let mut n = 0; let _ = run(|| { n = n + 1; return n; });",
                "n",
            ),
            (
                "let mut n = 0; let f = || { n = n + 1; return n; }; let _ = run(f);",
                "n",
            ),
            (
                "let mut n = 0; let f = || n; let write = || { n = n + 1; }; let _ = run(f); write();",
                "n",
            ),
            (
                "let mut n = 0; let write = || { n = n + 1; }; let f = || n; let _ = run(f); write();",
                "n",
            ),
            ("let a = [1]; let _ = run(|| a.len());", "a"),
            (
                "let a = [1]; let f = || a.len(); let g = f; let _ = run(g);",
                "a",
            ),
            (
                "let a = [1]; let f = || a.len(); let g = || f(); let _ = run(g);",
                "a",
            ),
            (
                "let a = [1]; let f = if true { || a.len() } else { one }; let _ = run(f);",
                "a",
            ),
            (
                "let a = [1]; let f = match true { true => || a.len(), false => one }; let _ = run(f);",
                "a",
            ),
            (
                "let a = [1]; let c = Channel<fn() -> i64>::new(); c.send(|| a.len());",
                "a",
            ),
            (
                "let a = [1]; let c = Channel<fn() -> i64>::new(); let f = || a.len(); c.send(f);",
                "a",
            ),
            (
                "let mut n = 0; let c = Channel<fn() -> i64>::new(); c.send(|| { n = n + 1; return n; });",
                "n",
            ),
            (
                "let a = [1]; let c = Channel<fn() -> i64>::new(); select { c.send(|| a.len()) => {} }",
                "a",
            ),
            (
                "let a = [1]; let c = Channel<fn() -> i64>::new(); let f = || a.len(); select { c.send(f) => {} }",
                "a",
            ),
            (
                "let a = [1]; let c = Channel<fn() -> i64>::with_capacity(1); c.send(|| a.len());",
                "a",
            ),
            (
                "let mut n = 0; let c = Channel<fn() -> i64>::new(); let f = || n; let write = || { n = n + 1; }; c.send(f); write();",
                "n",
            ),
            ("let a = [1]; let _ = new SharedHolder(|| a.len());", "a"),
        ] {
            let errors = check(body);
            let source = format!("{DECLS}\nfn main() {{ {body} }}");
            assert!(
                errors
                    .iter()
                    .any(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)
                        && d.labels.iter().any(|l| l
                            .message
                            .contains(&format!("captured variable `{name}`"))
                            && source
                                .get(l.span.start..l.span.end)
                                .is_some_and(|origin| origin
                                    .starts_with(&format!("let {name} ="))
                                    || origin.starts_with(&format!("let mut {name} ="))))),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn function_slots_do_not_erase_task_local_state() {
        for body in [
            "let a = [1]; let f = identity(|| a.len()); let _ = run(f);",
            "let a = [1]; let f = make(a); let _ = run(f);",
            "let a = [1]; let h = new Holder(|| a.len()); let _ = run(h.f);",
            "let a = [1]; let h = new Holder(one); h.f = || a.len(); let _ = run(h.f);",
            "let a = [1]; let c = Channel<fn() -> i64>::new(); c.send(identity(|| a.len()));",
            "let a = [1]; let c = Channel<fn() -> i64>::new(); c.send(make(a));",
            "let a = [1]; let h = new Holder(|| a.len()); let c = Channel<fn() -> i64>::new(); c.send(h.f);",
        ] {
            let errors = check(body);
            assert!(
                errors
                    .iter()
                    .any(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn reassignment_replaces_capture_provenance_before_transfer() {
        for transfer in [
            "let _ = run(f);",
            "channel.send(f);",
            "select { channel.send(f) => {} }",
        ] {
            let body = format!(
                "let a = [1]; let b = [2]; let mut f = || a.len(); f = || b.len(); let channel = Channel<fn() -> i64>::new(); {transfer}"
            );
            let errors = check(&body);
            let error = errors
                .iter()
                .find(|error| error.code == ErrorCode::E2402)
                .expect("sharing rejected");
            assert!(
                error
                    .labels
                    .iter()
                    .any(|label| label.message.contains("captured variable `b`")),
                "{body}: {error:?}"
            );
            assert!(
                !error
                    .labels
                    .iter()
                    .any(|label| label.message.contains("captured variable `a`")),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn capture_provenance_tracks_assignments_and_reachable_branch_exits() {
        // Each list names the possible witnesses at successive transfers.
        // In particular, no overwritten initializer can survive a definite
        // assignment or an exhaustive set of overwriting alternatives.
        for (body, expected) in [
            ("f = || b.len(); f = || c.len(); let _ = run(f);", vec!["c"]),
            (
                "let saved = f; f = || b.len(); let _ = run(saved); let _ = run(f);",
                vec!["a", "b"],
            ),
            ("f = || b.len(); f = f; let _ = run(f);", vec!["b"]),
            (
                "f = || b.len(); let g = f; f = g; let _ = run(f);",
                vec!["b"],
            ),
            ("f = || b.len(); let g = || f(); let _ = run(g);", vec!["f"]),
            (
                "if choice { f = || b.len(); } else { f = || c.len(); } let _ = run(f);",
                vec!["bc"],
            ),
            (
                "if choice { f = || b.len(); } else { let _ = run(f); }",
                vec!["a"],
            ),
            (
                "if choice { f = || b.len(); } else { f = one; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if choice { f = one; } else { f = || b.len(); } let _ = run(f);",
                vec!["b"],
            ),
            ("if choice { f = || b.len(); } let _ = run(f);", vec!["ab"]),
            ("if choice { f = one; } let _ = run(f);", vec!["a"]),
            (
                "if choice { f = || b.len(); return; } else { f = || c.len(); } let _ = run(f);",
                vec!["c"],
            ),
            (
                "if choice { f = || b.len(); } else { f = || c.len(); return; } let _ = run(f);",
                vec!["b"],
            ),
            (
                "if choice { if choice { f = || b.len(); } else { f = || c.len(); } } else { f = || b.len(); } let _ = run(f);",
                vec!["bc"],
            ),
            (
                "if choice { f = || b.len(); } else { f = || c.len(); } f = || a.len(); let _ = run(f);",
                vec!["a"],
            ),
            (
                "match choice { true => { f = || b.len(); }, false => { f = || c.len(); } }; let _ = run(f);",
                vec!["bc"],
            ),
            (
                "match choice { true => { f = || b.len(); }, false => { let _ = run(f); } };",
                vec!["a"],
            ),
            (
                "match choice { true => { f = || b.len(); return; }, false => { f = || c.len(); } }; let _ = run(f);",
                vec!["c"],
            ),
            (
                "select { channel.recv() => { f = || b.len(); } default => { f = || c.len(); } } let _ = run(f);",
                vec!["bc"],
            ),
            (
                "select { channel.recv() => { f = || b.len(); } default => { let _ = run(f); } }",
                vec!["a"],
            ),
            ("while choice { f = one; } let _ = run(f);", vec!["a"]),
            ("for i in 0..2 { f = one; } let _ = run(f);", vec!["a"]),
            (
                "let writer = || { f = || b.len(); }; let _ = run(f);",
                vec!["a"],
            ),
            (
                "if choice { let f = || b.len(); let _ = run(f); } let _ = run(f);",
                vec!["b", "a"],
            ),
        ] {
            let body = format!(
                "let a = [1]; let b = [2]; let c = [3]; let mut f = || a.len(); let choice = true; let channel = Channel<i64>::new(); {body}"
            );
            let source = format!("{DECLS}\nfn main() {{ {body} }}");
            let errors = check(&body);
            assert!(
                errors
                    .iter()
                    .all(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)),
                "{body}: {errors:?}"
            );
            let transfers: Vec<_> = errors
                .iter()
                .filter(|d| d.code == ErrorCode::E2402)
                .collect();
            assert_eq!(transfers.len(), expected.len(), "{body}: {errors:?}");
            for (error, candidates) in transfers.into_iter().zip(expected) {
                let causes: Vec<_> = error
                    .labels
                    .iter()
                    .filter(|l| l.message.starts_with("captured variable"))
                    .collect();
                assert_eq!(causes.len(), 1, "{body}: {error:?}");
                assert!(
                    candidates.chars().any(|name| causes[0]
                        .message
                        .contains(&format!("captured variable `{name}`"))
                        && [format!("let {name} ="), format!("let mut {name} =")]
                            .iter()
                            .any(|prefix| source[causes[0].span.start..causes[0].span.end]
                                .starts_with(prefix))),
                    "{body}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn replacing_with_an_opaque_or_shareable_value_clears_old_capture_labels() {
        for value in [
            "one",
            "|| 3",
            "identity(|| b.len())",
            "make(b)",
            "new Holder(|| b.len()).f",
        ] {
            let body = format!(
                "let a = [1]; let b = [2]; let mut f = || a.len(); f = {value}; let _ = run(f);"
            );
            let errors = check(&body);
            let error = errors
                .iter()
                .find(|d| d.code == ErrorCode::E2402)
                .expect("opaque task-local contract still rejects sharing");
            assert!(
                !error
                    .labels
                    .iter()
                    .any(|l| l.message.contains("captured variable `a`")),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn capture_branch_merges_visit_changes_not_untouched_bindings() {
        for bindings in [16, 64, 256] {
            for arms in [16, 32, 64] {
                let mut body = String::from("let a = [1]; let b = [2]; let mut f = || a.len();");
                for index in 0..bindings {
                    body.push_str(&format!("let local{index} = || a.len();"));
                }
                for index in 0..arms {
                    body.push_str(&format!(
                        "let payload{index} = [{index}]; let op{index} = || payload{index}.len();"
                    ));
                }
                body.push_str("let selector = 0; match selector {");
                for index in 0..arms {
                    body.push_str(&format!("{index} => {{ f = op{index}; }},"));
                }
                body.push_str("_ => { f = op0; } }; let _ = run(f);");
                FLOW_DIFFS.with(|n| n.set(0));
                let errors = check(&body);
                let diffs = FLOW_DIFFS.with(|n| n.get());
                assert!(
                    (arms - 1..=2 * arms + 2).contains(&diffs),
                    "bindings={bindings}, arms={arms}: {diffs}"
                );
                let error = errors.iter().find(|d| d.code == ErrorCode::E2402).unwrap();
                assert!(
                    error
                        .labels
                        .iter()
                        .any(|l| l.message.contains("captured variable `payload0`")),
                    "{error:?}"
                );
                eprintln!("capture branch merge: bindings={bindings}, arms={arms}, diffs={diffs}");
            }
        }
    }

    #[test]
    fn capture_branch_merges_do_not_rescan_prior_disjoint_changes() {
        for count in [16, 32, 64, 128] {
            let mut body = String::from("let a = [1]; let b = [2];");
            for index in 0..count {
                body.push_str(&format!("let mut op{index} = || a.len();"));
            }
            body.push_str("let selector = 0; match selector {");
            for index in 0..count {
                body.push_str(&format!(
                    "{index} => {{ op{index} = || b.len(); let local{index} = || b.len(); }},"
                ));
            }
            body.push_str("_ => {} }; let _ = run(op0);");
            FLOW_DIFFS.with(|n| n.set(0));
            let errors = check(&body);
            let diffs = FLOW_DIFFS.with(|n| n.get());
            assert!((count..=2 * count).contains(&diffs), "{count}: {diffs}");
            assert!(errors.iter().any(|d| d.code == ErrorCode::E2402));
            eprintln!("capture branch merge: disjoint arms={count}, diffs={diffs}");
        }
    }

    #[test]
    fn capture_flow_returns_the_value_before_deferred_reassignment() {
        let source = format!(
            "{DECLS}\nasync fn produce() -> fn() -> i64 {{ let a = [1]; let b = [2]; let mut f = || a.len(); defer {{ f = || b.len(); }} return f; }} fn main() {{}}"
        );
        let errors = check_source(&source);
        let error = errors
            .iter()
            .find(|d| {
                d.code == ErrorCode::E2402
                    && d.primary_span()
                        .is_some_and(|span| &source[span.start..span.end] == "return")
            })
            .unwrap_or_else(|| panic!("return sharing diagnostic: {errors:?}"));
        assert!(
            error
                .labels
                .iter()
                .any(|l| l.message.contains("captured variable `a`")),
            "{errors:?}"
        );
        assert!(
            !error
                .labels
                .iter()
                .any(|l| l.message.contains("captured variable `b`")),
            "{errors:?}"
        );
    }

    #[test]
    fn capture_flow_bounds_nested_cleanup_across_exit_paths() {
        for alias in [false, true] {
            for depth in [8, 16, 32, 64] {
                let mut nested = if alias { "f = g;" } else { "f = || b.len();" }.to_string();
                for _ in 0..depth {
                    nested = format!("while choice {{ defer {{ {nested} }} }}");
                }
                let body = format!(
                    "let a = [1]; let b = [2]; let mut f = || a.len(); let g = || b.len(); let choice = true; while choice {{ defer {{ {nested} }} if choice {{ return; }} if choice {{ break; }} if choice {{ continue; }} }} let _ = run(f);"
                );
                FLOW_STEPS.with(|n| n.set(0));
                FLOW_NODES.with(|n| n.set(0));
                let errors = check(&body);
                assert!(
                    errors
                        .iter()
                        .all(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408)),
                    "{errors:?}"
                );
                assert!(
                    errors.iter().any(|d| d.code == ErrorCode::E2402
                        && d.labels
                            .iter()
                            .any(|l| l.message.starts_with("captured variable"))),
                    "{errors:?}"
                );
                let steps = FLOW_STEPS.with(|n| n.get());
                let nodes = FLOW_NODES.with(|n| n.get());
                assert!(steps <= 60 * depth + 100, "{alias}/{depth}: {steps}");
                assert!(nodes <= 60 * depth + 100, "{alias}/{depth}: {nodes}");
                eprintln!(
                    "capture flow nested cleanup: alias={alias}, depth={depth}, steps={steps}, nodes={nodes}"
                );
            }
        }
    }

    #[test]
    fn capture_flow_groups_many_exits_before_running_cleanup() {
        for exits in [16, 32, 64, 128] {
            let mut body = String::from(
                "let a = [1]; let b = [2]; let mut f = || a.len(); let choice = true; if true { defer { f = || b.len(); }",
            );
            for _ in 0..exits {
                body.push_str("if choice { return; }");
            }
            body.push_str("} let _ = run(f);");
            FLOW_STEPS.with(|n| n.set(0));
            FLOW_NODES.with(|n| n.set(0));
            let errors = check(&body);
            assert!(
                errors.iter().any(|d| d.code == ErrorCode::E2402
                    && d.labels
                        .iter()
                        .any(|l| l.message.contains("captured variable `b`"))),
                "{errors:?}"
            );
            let steps = FLOW_STEPS.with(|n| n.get());
            let nodes = FLOW_NODES.with(|n| n.get());
            assert!(steps <= 8 * exits + 100, "{exits}: {steps}");
            assert!(nodes <= 8 * exits + 100, "{exits}: {nodes}");
            eprintln!("capture flow cleanup exits={exits}, steps={steps}, nodes={nodes}");
        }
    }

    #[test]
    fn capture_provenance_scales_linearly_through_diamonds_and_nested_values() {
        for nested in [false, true] {
            for count in [16, 32, 64, 128] {
                let mut source = String::from("let a = [1]; let op0 = || a.len();");
                for index in 1..=count {
                    source.push_str(&if nested {
                        format!("let op{index} = || op{}();", index - 1)
                    } else {
                        format!(
                            "let op{index} = if true {{ op{} }} else {{ op{} }};",
                            index - 1,
                            index - 1
                        )
                    });
                }
                source.push_str(&format!("let _ = run(op{count});"));
                CAUSE_COPIES.with(|n| n.set(0));
                let errors = check(&source);
                let copies = CAUSE_COPIES.with(|n| n.get());
                assert!(
                    (count..=4 * count + 10).contains(&copies),
                    "{nested}/{count}: {copies}"
                );
                let error = errors.iter().find(|d| d.code == ErrorCode::E2402).unwrap();
                assert!(
                    error
                        .labels
                        .iter()
                        .any(|l| l.message.contains("captured variable `a`")),
                    "{error:?}"
                );
                assert!(error.labels.len() <= 2, "{error:?}");
                eprintln!(
                    "capture provenance: nested={nested}, values={count}, copies={copies}, labels={}",
                    error.labels.len()
                );
            }
        }
    }
}

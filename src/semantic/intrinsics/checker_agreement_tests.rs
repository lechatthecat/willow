//! Perspectives 37-52: the resolver and the type checker must answer the
//! same question the same way.
//!
//! The unit tests above pin what [`resolve`] returns in isolation. They
//! cannot catch the failure this table exists to prevent: [`resolve`]
//! staying self-consistent while drifting away from the type checker. That
//! is exactly what happened to the four hand-written string waterfalls the
//! table replaced — each was internally tidy and none agreed with the
//! others.
//!
//! So these tests type-check real source and then assert, for every builtin
//! method call in it, that the resolver's result type equals the type the
//! checker recorded in `expr_types`: the type the rest of the compiler
//! actually believes. Adding a method to one side and not the other fails
//! here rather than at some later miscompile.

use super::*;
type Type = crate::parser::ast::Type<String>;
use crate::parser::ast::{Block, Expr, Item, MethodCallExpr, Stmt};

/// Every builtin call the snippets reach. Only the expression and statement
/// forms the snippets below actually use are descended into; a snippet that
/// grows a new form collects fewer calls than its test asserts, which fails
/// rather than passing vacuously.
fn walk_expr<'a>(expr: &'a Expr, out: &mut Vec<&'a MethodCallExpr>) {
    match expr {
        Expr::MethodCall(m) => {
            walk_expr(&m.object, out);
            for arg in &m.args {
                walk_expr(&arg.expr, out);
            }
            out.push(m);
        }
        Expr::Await(a) => walk_expr(&a.expr, out),
        Expr::Print(inner, _, _, _) => walk_expr(inner, out),
        Expr::Binary(b) => {
            walk_expr(&b.lhs, out);
            walk_expr(&b.rhs, out);
        }
        Expr::Unary(u) => walk_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                walk_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(c) => {
            for arg in &c.args {
                walk_expr(&arg.expr, out);
            }
        }
        Expr::Index(base, index, _, _) => {
            walk_expr(base, out);
            walk_expr(index, out);
        }
        Expr::ArrayLiteral(items, _, _) => {
            for item in items {
                walk_expr(item, out);
            }
        }
        _ => {}
    }
}

fn walk_block<'a>(block: &'a Block, out: &mut Vec<&'a MethodCallExpr>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(s) => walk_expr(&s.init, out),
            Stmt::Expr(s) => walk_expr(&s.expr, out),
            Stmt::Return(s) => {
                if let Some(value) = &s.value {
                    walk_expr(value, out);
                }
            }
            _ => {}
        }
    }
}

/// Type-check `src` with the prelude registered, then check every builtin
/// method call in it against [`resolve`]. Returns the intrinsics reached, so
/// the coverage test at the end can prove no family lost its snippet.
///
/// `expected` is the number of calls the snippet is meant to contain: an
/// assertion that passes because the walker found nothing is worse than one
/// that fails.
fn assert_agrees(src: &str, expected: usize) -> Vec<Intrinsic> {
    assert_agrees_with_access(src, expected, false)
}

fn assert_agrees_with_access(src: &str, expected: usize, abi: bool) -> Vec<Intrinsic> {
    let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude registers");
    if abi {
        // Bootstrap declarations, then emulate the bundled file capability.
        let (empty, _) =
            crate::parser::Parser::new(crate::lexer::Lexer::new("").tokenize().unwrap()).parse();
        checker.check_program(&empty);
        checker
            .array_abi_files
            .insert(crate::diagnostics::FileId(0));
    }
    checker.check_program(&program);
    assert!(
        checker.errors.is_empty(),
        "snippet must type-check cleanly, got {:?}",
        checker.errors
    );

    let mut calls = Vec::new();
    for item in &program.items {
        if let Item::Function(f) = item {
            walk_block(&f.body, &mut calls);
        }
    }

    let mut reached = Vec::new();
    let mut builtin_calls = 0;
    for call in &calls {
        let recv = checker
            .expr_types
            .get(&call.object.id())
            .unwrap_or_else(|| panic!("checker recorded no receiver type for `{}`", call.method))
            .clone();
        let resolution = resolve(&recv, &call.method, call.args.len());
        assert_eq!(
            checker.builtin_calls.get(&call.id),
            resolution.as_ref().map(|sig| &sig.intrinsic)
        );
        let Some(resolved) = resolution else {
            if matches!(recv, Type::Array(_)) {
                // Public array methods must have an ordinary source target.
                let owner = checker
                    .static_call_classes
                    .get(&call.id)
                    .expect("source array method target");
                assert!(owner.to_string().contains("Array"));
                builtin_calls += 1;
            }
            continue;
        };
        builtin_calls += 1;
        let checker_ret = checker
            .expr_types
            .get(&call.id)
            .unwrap_or_else(|| panic!("checker recorded no result type for `{}`", call.method))
            .clone();
        reached.push(resolved.intrinsic);
        let resolved_ret = resolved.return_type(|i| {
            call.args
                .get(i)
                .and_then(|arg| checker.expr_types.get(&arg.expr.id()))
                .cloned()
        });
        assert_eq!(
            resolved_ret,
            checker_ret,
            "`{}::{}` — resolver says {resolved_ret:?}, type checker says {checker_ret:?}",
            type_name_for_message(&recv),
            call.method
        );
    }
    assert_eq!(
        builtin_calls, expected,
        "snippet should contain {expected} builtin calls, found {builtin_calls}"
    );
    reached
}

fn type_name_for_message(ty: &Type) -> String {
    format!("{ty:?}")
}

const STRINGS: &str = r#"
fn f(s: String) {
    let byte = s.byte_at(0);
    let part = s.substring(0, 1);
    let pieces = s.split(",");
    let contains = s.contains("a");
    let found = s.find("a");
    let trimmed = s.trim();
    let repeated = s.repeat(2);
    let starts = s.starts_with("a");
    let ends = s.ends_with("a");
    let replaced = s.replace("a", "b");
    let left = s.trim_start();
    let right = s.trim_end();
    let lower = s.to_lower();
    let upper = s.to_upper();
}
"#;

const SCALARS: &str = r#"
fn f() {
    let n = 42;
    let r = 1.5;
    let b = true;
    let s = "willow";
    let a = n.to_string();
    let c = r.to_string();
    let d = b.to_string();
    let e = s.to_string();
    let size = s.len();
}
"#;

const INTEGERS: &str = r#"
fn f() {
    let n = 42;
    let a = n.wrapping_add(1);
    let b = n.wrapping_sub(1);
    let c = n.wrapping_mul(2);
    let d = n.wrapping_neg();
    let e = n.checked_add(1);
    let g = n.checked_sub(1);
    let h = n.checked_mul(2);
    let i = n.checked_neg();
}
"#;

const ARRAYS: &str = r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    xs.push(4);
    let last = xs.pop();
    let size = xs.len();
    let text = xs.to_string();
    let frozen = xs.freeze();
    let frozen_size = frozen.len();
    let joined = ["a", "b"].join(",");
}
"#;

const MAPS: &str = r#"
import std::collections::Map;

fn f() {
    let m: Map<String, i64> = Map::new();
    m.insert("ann", 87);
    let removed = m.remove("ann");
    let keys = m.keys();
    let values = m.values();
    let entries = m.entries();
    let hit = m.get("ann");
    let has = m.contains("ann");
    let size = m.len();
    let text = m.to_string();
    let frozen = m.freeze();
    let frozen_hit = frozen.get("ann");
    let frozen_has = frozen.contains("ann");
    let frozen_size = frozen.len();
}
"#;

const ATOMICS: &str = r#"
fn f() {
    let counter = AtomicI64::new(0);
    counter.store(5);
    let before_add = counter.add(3);
    let before_sub = counter.sub(2);
    let before_swap = counter.swap(100);
    let changed = counter.compare_exchange(100, 101);
    let now = counter.load();
    let flag = AtomicBool::new(false);
    flag.store(true);
    let was = flag.swap(false);
    let changed_flag = flag.compare_exchange(false, true);
    let set = flag.load();
}
"#;

const CELLS: &str = r#"
fn f() {
    let mode = BlockingRwCell::new("dev");
    mode.write("prod");
    let current = mode.read();
    let ready = BlockingCell::new(false);
    ready.set(true);
    let done = ready.get();
}
"#;

const CHANNELS: &str = r#"
async fn f() {
    let ch = Channel<i64>::new();
    ch.send(7);
    let value = ch.recv();
    let maybe = ch.recv_opt();
    ch.close();
}
"#;

const TASKS: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let running = work();
    let stopped = running.is_cancelled();
    let outcome = await running.result();
    let other = work();
    other.cancel();
}
"#;

const TOKENS: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let token = CancellationToken::new();
    let idle = token.is_cancelled();
    let nested = token.child();
    let attached = nested.attach(work());
    token.cancel();
}
"#;

const SCOPES: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let scope = TaskScope::new();
    let idle = scope.is_cancelled();
    let nested = scope.child();
    let owned = scope.add(work());
    let done = await scope.finish();
    scope.cancel();
}
"#;

/// Perspective 37: `to_string` on i64, f64, bool and String. The checker and
/// the resolver must agree that all four produce a String even though each
/// lowers to a different runtime symbol.
#[test]
fn p37_scalar_to_string_agrees() {
    assert_agrees(SCALARS, 5);
}

/// Perspective 37b: the explicit `i64` overflow methods. `wrapping_*`
/// return `i64`; `checked_*` return `Option<i64>`.
#[test]
fn p37b_integer_overflow_methods_agree() {
    assert_agrees(INTEGERS, 8);
}

/// Perspective 38: the whole `Array<T>` surface, plus the `FrozenArray<T>`
/// that `freeze` produces. `pop` returns the element type and `freeze`
/// returns a differently-named family, so both are places a hand-written
/// waterfall would get wrong.
#[test]
fn p38_array_and_frozen_array_agree() {
    assert_agrees(ARRAYS, 7);
}

/// Perspective 39: the `Map<K,V>` surface plus the `FrozenMap<K,V>` reads.
/// `get` returns `Option<V>` — the value type, not the key type — which the
/// backend's structural walk had no answer for at all before this table.
#[test]
fn p39_map_and_frozen_map_agree() {
    assert_agrees(MAPS, 13);
}

/// Perspective 40: `AtomicI64` and `AtomicBool` share method names but not
/// result types: `swap` gives an i64 on one and a bool on the other.
#[test]
fn p40_atomics_agree() {
    assert_agrees(ATOMICS, 10);
}

/// Perspective 41: `BlockingCell<T>` and `BlockingRwCell<T>` return their
/// element type from a read and `void` from a write.
#[test]
fn p41_cells_agree() {
    assert_agrees(CELLS, 4);
}

/// Perspective 42: `Channel<T>`. `recv` yields the element type and
/// `recv_opt` yields `Option<T>`; both are suspension points. `send` and
/// `close` are void.
#[test]
fn p42_channels_agree() {
    assert_agrees(CHANNELS, 4);
}

/// Perspective 43: `Task<T>`. `result` yields `TaskResult<T>`, which the
/// structural walk previously defaulted to `i64` — an `await` on it would
/// have been typed as an integer.
#[test]
fn p43_tasks_agree() {
    assert_agrees(TASKS, 3);
}

/// Perspective 44: `CancellationToken`, including `attach`, whose result
/// type follows its argument rather than its receiver.
#[test]
fn p44_tokens_agree() {
    assert_agrees(TOKENS, 4);
}

/// Perspective 45: `TaskScope`, including `add` (argument-typed) and
/// `finish` (a fixed `Task<Result<void, Cancelled>>`).
#[test]
fn p45_scopes_agree() {
    assert_agrees(SCOPES, 5);
}

/// Perspective 46: a user class method that spells a builtin name is not
/// stolen by the table — it resolves to nothing and stays with class
/// dispatch, and the snippet therefore contains zero builtin calls.
#[test]
fn p46_user_methods_are_not_intrinsics() {
    assert_agrees(
        r#"
class Bag {
    pub count: i64;
    pub fn len(self) -> i64 { return self.count; }
    pub fn to_string(self) -> String { return "bag"; }
}

fn f() {
    let b = new Bag(3);
    let n = b.len();
    let s = b.to_string();
}
"#,
        0,
    );
}

/// Perspective 47: chained calls. The receiver of the second call is the
/// *result* of the first, so agreement on `freeze` is what makes agreement
/// on the following `len` possible at all.
#[test]
fn p47_chained_receivers_agree() {
    assert_agrees(
        r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    let size = xs.freeze().len();
}
"#,
        2,
    );
}

/// Perspective 48: a builtin call nested inside another expression is typed
/// the same way as one standing alone. The backend hoists suspension points
/// out of larger expressions, so nesting must not change the answer.
#[test]
fn p48_nested_positions_agree() {
    assert_agrees(
        r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    let total = xs.len() + xs.freeze().len();
    println(xs.to_string());
}
"#,
        4,
    );
}

/// Perspective 49: element types travel. A `Map<String, String>` and a
/// `Map<String, i64>` differ only in their value type, and `get` must follow
/// it on both.
#[test]
fn p49_element_types_travel() {
    assert_agrees(
        r#"
import std::collections::Map;

fn f() {
    let words: Map<String, String> = Map::new();
    words.insert("a", "b");
    let word = words.get("a");
    let counts: Map<String, i64> = Map::new();
    counts.insert("a", 1);
    let count = counts.get("a");
}
"#,
        4,
    );
}

/// Perspective 50: `Array<String>` — `pop` gives a String, not the `i64`
/// that a family-only answer would produce.
#[test]
fn p50_string_arrays_agree() {
    assert_agrees(
        r#"
import std::collections::Array;

fn f() {
    let names: Array<String> = ["ann", "ben"];
    let last = names.pop();
    let text = names.to_string();
}
"#,
        2,
    );
}

/// Perspective 51: the snippets in this module reach every intrinsic. A
/// family that loses its snippet stops being cross-checked, which is the
/// quiet way this protection would rot.
#[test]
fn p51_every_intrinsic_is_cross_checked() {
    let mut reached: std::collections::HashSet<Intrinsic> = std::collections::HashSet::new();
    for (src, expected) in [
        (SCALARS, 5),
        (
            "fn f(x: u64, c: char) { x.to_string(); c.to_string(); \"123\".parse_u16(); }",
            3,
        ),
        (INTEGERS, 8),
        (
            "fn f(x: u8, y: i64) { x.to_i64(); y.wrapping_to_u8(); y.checked_to_u8(); }",
            3,
        ),
        (STRINGS, 14),
        (ARRAYS, 7),
        (MAPS, 13),
        (ATOMICS, 10),
        (CELLS, 4),
        (CHANNELS, 4),
        (TASKS, 3),
        (TOKENS, 4),
        (SCOPES, 5),
    ] {
        reached.extend(assert_agrees(src, expected));
    }
    reached.extend(assert_agrees_with_access(
        r#"
fn f() {
    let xs = [1];
    xs.__abi_resize(2);
    let n = xs.__abi_len();
    let frozen = xs.__abi_copy();
    let text = ["a", "b"].__abi_concat();
    let quoted = "x".__abi_debug();
}
"#,
        5,
        true,
    ));
    let missing: Vec<Intrinsic> = Intrinsic::ALL
        .iter()
        .copied()
        .filter(|i| !reached.contains(i))
        .collect();
    assert!(
        missing.is_empty(),
        "intrinsics with no checker cross-check: {missing:?}"
    );
}

/// Perspective 52: the two suspension-point intrinsics are exactly the two
/// the checker treats as async-only. Getting this set wrong would either
/// drop a frame-narrowing hoist or add a spurious one.
#[test]
fn p52_suspension_points_are_the_channel_pair() {
    let suspending: Vec<Intrinsic> = Intrinsic::ALL
        .iter()
        .copied()
        .filter(|i| i.is_suspension_point())
        .collect();
    assert_eq!(
        suspending,
        vec![
            Intrinsic::ChannelSend,
            Intrinsic::ChannelRecv,
            Intrinsic::ChannelRecvOpt
        ]
    );
}

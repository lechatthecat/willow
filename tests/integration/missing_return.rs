//! E0205: a body that owes its caller a value must not run off the end
//! (willow-x8sj).
//!
//! Willow has no implicit tail expression — a body produces its value only
//! through `return` — and nothing used to check that one was reached. A
//! function declared `-> i64` whose body just ended compiled clean and handed
//! the caller the zero of the return type, so a missing `return match s { ... }`
//! read exactly like a downcast that never matched and sent debugging in the
//! wrong direction.
//!
//! The check is a reachability question, not a data-flow one: can control get to
//! the closing brace? Three things stop it — a `return`, a call declared to
//! return `!` (only `panic`), and a loop that cannot finish. Everything else in
//! this file is about which shapes count as which.
//!
//! Being imprecise is not symmetric. A divergence the analysis fails to see
//! rejects a correct program, so the accept cases below carry the weight; the
//! reject cases pin that the original hole is actually closed.
//!
//! An annotation must not change the verdict. A lambda with no `->` is typed by
//! whatever its `return`s produced, and that inferred type carries the same
//! obligation an annotated one does — perspectives 26-29 pin the case that used
//! to slip through, where the same body compiled clean without the annotation
//! and handed the caller a zero.
//!
//! 29 perspectives:
//!   1 no return at all             16 a static method
//!   2 `if` with no `else`          17 an `async fn`
//!   3 a trailing statement `match` 18 an `async` method
//!   4 both `if` arms return        19 an interface default body
//!   5 every `match` arm returns    20 an annotated lambda
//!   6 every arm panics             21 a lambda typed by its context
//!   7 a trailing `panic`           22 a lambda that returns on both paths
//!   8 `while true` with no break   23 `select`: one case falls through
//!   9 `while true` with a break    24 `main() -> Result<void, E>` may end
//!  10 a break belongs to the inner    implicitly, a plain function may not
//!     loop                         25 the diagnostic's labels and help
//!  11 a break inside a `match` arm 26 a lambda with an INFERRED return type
//!  12 `for` is never an ending     27 an inferred lambda returning on all
//!  13 a `void` body owes nothing      paths is accepted
//!  14 a constructor owes nothing   28 an inferred `void` lambda owes nothing
//!  15 an instance method           29 the inferred diagnostic says `inferred`,
//!                                     not `declared`
//!
//! The `main` exemption is the one place the rule steps aside, and it belongs to
//! the ENTRY POINT, not to a name. Perspectives 30-31 pin both halves: the real
//! entry point keeps the latitude, and an imported module's `main` — an ordinary
//! function called as `foo::main()` and mangled like any other — does not
//! (willow-ltkj).
//!
//!  30 the entry point keeps the exemption in a multi-file project
//!  31 an imported module's `main` does not inherit it

use super::support::{
    assert_compile_error_contains, compile_and_run, compile_error_stderr,
    compile_temp_project_and_run, compile_temp_project_error_stderr,
};

/// Require the program to compile and print `expected`.
fn assert_accepted(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "expected this to compile and run: {out}");
    assert_eq!(out, expected);
}

/// Require E0205 naming `what`, and require it to be the only error — a body
/// that falls off the end must not also trip a type mismatch on the way.
fn assert_missing_return(source: &str, what: &str) {
    let stderr = compile_error_stderr(source);
    let expected = format!("error[E0205]: not all paths through {what} return a value");
    assert!(
        stderr.contains(&expected),
        "expected `{expected}`:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("error[").count(),
        1,
        "E0205 should be the only error:\n{stderr}"
    );
}

// 1. The reported repro: a body with no `return` anywhere. It used to compile
//    and print 0.
#[test]
fn missing_return_01_no_return_at_all() {
    assert_missing_return(
        "fn f(x: i64) -> i64 {
    let y = x + 1;
}
fn main() { println(f(10)); }
",
        "`f`",
    );
}

// 2. An `if` with no `else`: the condition may be false, and then the closing
//    brace is what comes next.
#[test]
fn missing_return_02_if_without_else() {
    assert_missing_return(
        "fn g(x: i64) -> i64 {
    if x > 0 {
        return 1;
    }
}
fn main() { println(g(0 - 1)); }
",
        "`g`",
    );
}

// 3. The shape that motivated the bug report: a trailing `match` written as if
//    it were a tail expression. Every arm produces the right type and the whole
//    thing still returns 0.
#[test]
fn missing_return_03_a_trailing_statement_match() {
    assert_missing_return(
        "enum Shape { Square(i64), Circle(i64) }
fn which(s: Shape) -> i64 {
    match s {
        Square(sq) => sq,
        _ => 0 - 1,
    }
}
fn main() { println(which(Shape::Square(4))); }
",
        "`which`",
    );
}

// 4. Both arms return, so nothing follows the `if`.
#[test]
fn missing_return_04_both_if_arms_return() {
    assert_accepted(
        "fn sign(n: i64) -> i64 {
    if n > 0 {
        return 1;
    } else {
        return 0 - 1;
    }
}
fn main() { println(sign(3)); println(sign(0 - 3)); }
",
        "1\n-1\n",
    );
}

// 5. A statement `match` is an ending when EVERY arm is one. It is sound to
//    stop at the listed arms because a non-exhaustive match is already E1206.
#[test]
fn missing_return_05_every_match_arm_returns() {
    assert_accepted(
        "enum Shape { Square(i64), Circle(i64) }
fn measure(s: Shape) -> i64 {
    match s {
        Shape::Square(side) => { return side * side; }
        Shape::Circle(r) => { return 3 * r * r; }
    }
}
fn main() { println(measure(Shape::Square(4))); println(measure(Shape::Circle(2))); }
",
        "16\n12\n",
    );
}

// 6. `panic` is declared to return `!`, so an arm that calls it leaves too — a
//    body whose every arm panics has no `return` in it and is complete.
#[test]
fn missing_return_06_every_arm_panics() {
    assert_accepted(
        "fn reject(kind: i64) -> i64 {
    match kind {
        0 => panic(\"kind 0 is not supported\"),
        _ => panic(\"unknown kind\"),
    }
}
fn main() { println(1); }
",
        "1\n",
    );
}

// 7. The same thing as a trailing statement, covering the path an `if` above it
//    does not.
#[test]
fn missing_return_07_a_trailing_panic() {
    assert_accepted(
        "fn only_even(n: i64) -> i64 {
    if n % 2 == 0 {
        return n / 2;
    }
    panic(\"expected an even number\");
}
fn main() { println(only_even(10)); }
",
        "5\n",
    );
}

// 8. `while true` with no way out never finishes, so the function never returns
//    and owes nothing. This is the ordinary way to write a loop that only exits
//    by returning, so rejecting it would be gratuitous.
#[test]
fn missing_return_08_while_true_with_no_break() {
    assert_accepted(
        "fn first_multiple(step: i64) -> i64 {
    let mut n = step;
    while true {
        if n % 7 == 0 {
            return n;
        }
        n = n + step;
    }
}
fn main() { println(first_multiple(3)); }
",
        "21\n",
    );
}

// 9. A `break` means the loop CAN finish, so what follows it is reachable and
//    has to end the function itself.
#[test]
fn missing_return_09_while_true_with_a_break() {
    assert_missing_return(
        "fn count_to(limit: i64) -> i64 {
    let mut n = 0;
    while true {
        if n >= limit {
            break;
        }
        n = n + 1;
    }
}
fn main() { println(count_to(3)); }
",
        "`count_to`",
    );
}

// 10. A `break` belongs to the innermost loop, so an inner one does not make the
//     outer one escapable.
#[test]
fn missing_return_10_a_break_belongs_to_the_inner_loop() {
    assert_accepted(
        "fn grid(rows: i64) -> i64 {
    let mut total = 0;
    while true {
        for col in 0..3 {
            if col == 2 {
                break;
            }
            total = total + 1;
        }
        if total >= rows {
            return total;
        }
    }
}
fn main() { println(grid(5)); }
",
        "6\n",
    );
}

// 11. A `match` arm is the expression position that holds statements, so the
//     break scan has to descend into it — otherwise this loop is mistaken for
//     one nothing can escape.
#[test]
fn missing_return_11_a_break_inside_a_match_arm() {
    assert_missing_return(
        "fn countdown(start: i64) -> i64 {
    let mut n = start;
    while true {
        match n {
            0 => { break; }
            _ => { n = n - 1; }
        }
    }
}
fn main() { println(countdown(3)); }
",
        "`countdown`",
    );
}

// 12. A `for` head is never an ending however its body ends: an empty range runs
//     zero iterations.
#[test]
fn missing_return_12_a_for_loop_is_never_an_ending() {
    assert_missing_return(
        "fn first(limit: i64) -> i64 {
    for i in 0..limit {
        return i;
    }
}
fn main() { println(first(0)); }
",
        "`first`",
    );
}

// 13. A `void` body owes nothing, so it may end however it likes.
#[test]
fn missing_return_13_a_void_body_owes_nothing() {
    assert_accepted(
        "fn shout(n: i64) {
    println(n);
}
fn main() { shout(4); }
",
        "4\n",
    );
}

// 14. A constructor is `void` too, and one that returned a value would be E0841.
#[test]
fn missing_return_14_a_constructor_owes_nothing() {
    assert_accepted(
        "class Box {
    pub side: i64;
    pub init(self, side: i64) { self.side = side; }
    pub fn area() -> i64 { return self.side * self.side; }
}
fn main() { println(new Box(3).area()); }
",
        "9\n",
    );
}

// 15. An instance method is held to the same rule, and the diagnostic names it
//     `Class::method`.
#[test]
fn missing_return_15_an_instance_method() {
    assert_missing_return(
        "class Box {
    pub side: i64;
    pub init(self, side: i64) { self.side = side; }
    pub fn area() -> i64 { let s = self.side; }
}
fn main() { println(new Box(3).area()); }
",
        "`Box::area`",
    );
}

// 16. A static method has no receiver and the same obligation.
#[test]
fn missing_return_16_a_static_method() {
    assert_missing_return(
        "class Box {
    pub static fn unit() -> i64 { let one = 1; }
}
fn main() { println(Box::unit()); }
",
        "`Box::unit`",
    );
}

// 17. An `async fn` declares the AWAITED value, so the body owes that value on
//     the same terms.
#[test]
fn missing_return_17_an_async_function() {
    assert_missing_return(
        "async fn compute(x: i64) -> i64 {
    let y = x + 1;
}
async fn main() { println(await compute(1)); }
",
        "`compute`",
    );
}

// 18. The async method spelling of the same thing.
#[test]
fn missing_return_18_an_async_method() {
    assert_missing_return(
        "class Worker {
    pub async fn step(x: i64) -> i64 { let y = x; }
}
async fn main() { println(await new Worker().step(1)); }
",
        "`Worker::step`",
    );
}

// 19. An interface default body is a body like any other, and it is checked once
//     at the interface even when no class implements it.
#[test]
fn missing_return_19_an_interface_default_body() {
    assert_missing_return(
        "interface Sized {
    fn area(self) -> i64;
    fn doubled(self) -> i64 { let a = self.area(); }
}
class Box implements Sized {
    pub fn area() -> i64 { return 2; }
}
fn main() { println(new Box().doubled()); }
",
        "`Sized::doubled`",
    );
}

// 20. A block-bodied lambda returns only through `return`, so an annotated one
//     that ends without it is the same bug.
#[test]
fn missing_return_20_an_annotated_lambda() {
    assert_missing_return(
        "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    let scaled = |x: i64| -> i64 { let y = x * 10; };
    println(apply(scaled, 4));
}
",
        "this lambda",
    );
}

// 21. The same lambda with its return type coming from the context rather than
//     an annotation.
#[test]
fn missing_return_21_a_lambda_typed_by_its_context() {
    assert_missing_return(
        "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    let scaled: fn(i64) -> i64 = |x: i64| { let y = x * 10; };
    println(apply(scaled, 4));
}
",
        "this lambda",
    );
}

// 22. A lambda that returns on every path is accepted, so the rule did not just
//     outlaw block-bodied lambdas.
#[test]
fn missing_return_22_a_lambda_that_returns_on_both_paths() {
    assert_accepted(
        "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    let abs = |x: i64| -> i64 {
        if x < 0 {
            return 0 - x;
        }
        return x;
    };
    println(apply(abs, 0 - 4));
    println(apply(abs, 4));
}
",
        "4\n4\n",
    );
}

// 23. `select` runs the body of exactly one case, so it is an ending only when
//     every case is. A `lock` body is the other block-bearing statement: it runs
//     unconditionally, so a `return` inside it ends the function.
#[test]
fn missing_return_23_select_and_lock_bodies() {
    assert_missing_return(
        "async fn watch(ch: Channel<i64>) -> i64 {
    select {
        let v = ch.recv() => { return v; }
        sleep(10) => { println(\"late\"); }
    }
}
async fn main() {
    let ch = Channel<i64>::new();
    ch.send(4);
    println(await watch(ch));
}
",
        "`watch`",
    );
    assert_accepted(
        "async fn take(m: Mutex<i64>, amount: i64) -> bool {
    lock m as mut value {
        if value < amount {
            return false;
        }
        value = value - amount;
        return true;
    }
}
async fn main() {
    let m = Mutex<i64>::new(10);
    println(await take(m, 4));
    println(await take(m, 40));
}
",
        "true\nfalse\n",
    );
}

// 24. The entry point is the one body where running off the end means
//     something: `fn main() -> Result<void, E>` exits 0 on the implicit end
//     exactly as it does on `return Ok()` (willow-exg). A plain function with
//     the same return type gets no such latitude.
#[test]
fn missing_return_24_a_result_void_main_may_end_implicitly() {
    assert_accepted(
        "fn main() -> Result<void, String> {
    println(99);
}
",
        "99\n",
    );
    assert_missing_return(
        "fn step() -> Result<void, String> {
    println(1);
}
fn main() { println(1); }
",
        "`step`",
    );
}

// 25. The diagnostic itself: the code, both labels, and the help.
#[test]
fn missing_return_25_the_diagnostic_points_at_the_fall_through() {
    assert_compile_error_contains(
        "fn f(x: i64) -> i64 {
    let y = x + 1;
}
fn main() { println(f(10)); }
",
        &[
            "error[E0205]",
            "not all paths through `f` return a value",
            "control can reach the end of the body without returning",
            "`i64` declared here",
            "help: end every path with `return <value>;`",
        ],
    );
}

// 26. The hole: with no `->` the lambda's return type is INFERRED from the
//     `return`s in its body, and the check used to be skipped entirely on that
//     path. One annotation apart from perspective 20, the same body compiled
//     and returned 0 for every input that missed the `return`.
#[test]
fn missing_return_26_a_lambda_with_an_inferred_return_type() {
    assert_missing_return(
        "fn main() {
    let scaled = |x: i64| { if x > 0 { return x * 10; } };
    println(scaled(4));
}
",
        "this lambda",
    );
}

// 27. Inference must not become a blanket rejection either: a lambda with no
//     annotation whose every path returns is still accepted.
#[test]
fn missing_return_27_an_inferred_lambda_returning_on_all_paths() {
    assert_accepted(
        "fn main() {
    let abs = |x: i64| {
        if x < 0 {
            return 0 - x;
        }
        return x;
    };
    println(abs(0 - 4));
    println(abs(4));
}
",
        "4\n4\n",
    );
}

// 28. A lambda with no `return` at all infers `void`, and `void` owes nothing.
//     This is the case the new call must not fire on — the inferred type is the
//     result of the walk, so the check runs after it and sees `void` here.
#[test]
fn missing_return_28_an_inferred_void_lambda_owes_nothing() {
    assert_accepted(
        "fn main() {
    let shout = |x: i64| { println(x * 2); };
    shout(21);
}
",
        "42\n",
    );
}

// 29. Nothing was written down in the inferred case, so the secondary label
//     must not say the type was declared.
#[test]
fn missing_return_29_the_inferred_diagnostic_does_not_claim_a_declaration() {
    let source = "fn main() {
    let scaled = |x: i64| { if x > 0 { return x * 10; } };
    println(scaled(4));
}
";
    assert_compile_error_contains(
        source,
        &[
            "error[E0205]",
            "not all paths through this lambda return a value",
            "`i64` inferred from a `return` in this body",
        ],
    );
    assert!(
        !compile_error_stderr(source).contains("declared here"),
        "an inferred lambda return type was never declared anywhere"
    );
}

// 30. The exemption is real and must survive the change: the program's own
//     `main` still ends implicitly, in a project that has modules so the entry
//     program is chosen rather than assumed.
#[test]
fn missing_return_30_the_entry_point_keeps_the_exemption() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "helper.wi",
                "pub fn greet() -> String {
    return \"hi\";
}
",
            ),
            (
                "app.wi",
                "import helper;

fn main() -> Result<void, String> {
    println(helper::greet());
}
",
            ),
        ],
        "app.wi",
    );
    assert!(ok, "the entry point may still end implicitly: {out}");
    assert_eq!(out, "hi\n");
}

// 31. The other half. A module's `main` is not an entry point — it is called as
//     `foo::main()` and the backend mangles it like any other function — so a
//     fall-through hands the caller the zero of a `Result<void, E>`, which then
//     has a tag read out of it. Exempting it by NAME was the bug (willow-ltkj).
#[test]
fn missing_return_31_a_module_main_does_not_inherit_the_exemption() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "foo.wi",
                "pub fn main() -> Result<void, String> {
    println(\"oops\");
}
",
            ),
            (
                "app.wi",
                "import foo;

fn main() {
    match foo::main() {
        Ok(_) => println(\"ok\"),
        Err(e) => println(e),
    }
}
",
            ),
        ],
        "app.wi",
    );
    assert!(
        stderr.contains("error[E0205]") && stderr.contains("not all paths through `main`"),
        "a module `main` owes its caller a value like any other function:\n{stderr}"
    );
}

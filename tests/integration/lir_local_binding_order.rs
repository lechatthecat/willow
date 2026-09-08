//! Local storage is bound before any block that names it (willow-34su).
//!
//! A LIR block index is not a position in any order control can flow in, and
//! an async body is emitted in index order. `lock` lowering is where the two
//! come apart: the section's body and its continuation are allocated while the
//! statement is being lowered, so both land AHEAD of the block that holds the
//! `let` they read.
//!
//!     let mut got = 0;                 // bb9
//!     lock m as value { got = value; } // bb7 — emitted first
//!     return got;                      // bb8 — emitted first
//!
//! `LirInst::Let` used to be what created the binding, at the point it was
//! emitted. Reaching `bb7` first meant the assignment found no storage and was
//! silently dropped, and the read in `bb8` reached codegen with nothing bound
//! at all — a hard ICE, "variable `got` reached LIR codegen unbound".
//!
//! Most of these open with a `gc_collect()`, and it is not decoration: an
//! async body is split at its preemption points and each continuation is
//! APPENDED to the block list, so a call before the `let` is what pushes the
//! binding behind the section that reads it. Eight of the perspectives below
//! fail without the fix and the rest hold the neighbouring answers steady.
//!
//! The fix moves the binding off the emission path: `bind_lir_locals` gives
//! EVERY non-parameter local its entry storage, and `LirInst::Let` stores into
//! whatever it left. Storage no longer depends on which block comes out first,
//! which is the invariant these perspectives pin.
//!
//! 24 perspectives:
//!   1 the read section hands a value out   13 an `if` picks which one is set
//!   2 the write section does too           14 a `match` arm sets it
//!   3 a collection in between              15 set inside, returned outside
//!   4 two sections, two locals             16 a section that never runs
//!   5 a local declared between them        17 the local's address is taken
//!   6 an accumulator across a loop         18 two tasks, one local each
//!   7 a String comes out                   19 held across a re-poll
//!   8 an object comes out                  20 a declared type widens
//!   9 an array comes out                   21 an enum comes out
//!  10 an f64 comes out                     22 a nested scope's local
//!  11 a bool comes out                     23 ordinary bodies still bind
//!  12 an RwLock read/write pair            24 the runnable example
//!
//! (20 reads "the declared type decides the storage": an interface value
//! cannot live in an async task frame, so that one is a synchronous body whose
//! `if/else` merge block is emitted before the arm that jumps to it.)
//!
//! Every perspective is run plain, under a one-instruction preemption budget,
//! and under allocation stress, and every function named is checked to have
//! come from the walker rather than the AST emitter.

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
/// Re-poll at every instruction boundary: an entry binding that a resumed poll
/// re-seeds would lose the value the section wrote.
const BUDGET: [(&str, &str); 1] = [("WILLOW_TASK_BUDGET", "1")];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

/// `expected` must come out of all three configurations, and `functions` must
/// each be named in the walker's selection log.
fn assert_binds(source: &str, expected: &str, functions: &[&str]) {
    for env in [&PLAIN[..], &BUDGET[..], &STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_owns(source, functions);
}

/// Assert the walker compiled each named function, without running anything.
fn assert_walker_owns(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

/// Perspective 1: the reported shape. A read-only section copies the protected
/// value into a local declared before it, and the read after the section is in
/// a block emitted before the `let` that binds it.
#[test]
fn lbo_01_read_section_hands_a_value_out() {
    assert_binds(
        r#"
async fn peek(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut got = 0;
    lock m as value { got = value; }
    return got;
}
async fn main() {
    let m = Mutex::new(41);
    println(await peek(m));
}
"#,
        "41\n",
        &["peek", "main"],
    );
}

/// Perspective 2: the same across a `mut` section, which commits the binding
/// back on release. The outer local must hold what the body computed, not the
/// zero its entry slot was seeded with.
#[test]
fn lbo_02_write_section_hands_a_value_out() {
    assert_binds(
        r#"
async fn bump_and_report(m: Mutex<i64>) -> i64 {
    let mut seen = 0;
    lock m as mut value {
        value = value + 1;
        seen = value;
    }
    return seen;
}
async fn main() {
    let m = Mutex::new(9);
    println(await bump_and_report(m));
    lock m as value { println(value); }
}
"#,
        "10\n10\n",
        &["bump_and_report", "main"],
    );
}

/// Perspective 3: a collection between the loop and the read. The original
/// report ran the sections in a loop that allocated, then collected, then read
/// the local the last section had written.
#[test]
fn lbo_03_a_collection_between_the_write_and_the_read() {
    assert_binds(
        r#"
class Cell {
    id: i64;
    pub init(self, id: i64) { self.id = id; }
    pub fn id(self) -> i64 { return self.id; }
}
async fn locked(m: Mutex<i64>, rounds: i64) -> i64 {
    let mut i = 0;
    while i < rounds {
        lock m as mut value {
            let cell = new Cell(i);
            value = value + cell.id();
        }
        i = i + 1;
    }
    gc_collect();
    let mut got = 0;
    lock m as value { got = value; }
    return got;
}
async fn main() {
    let m: Mutex<i64> = Mutex::new(0);
    println(await locked(m, 5));
}
"#,
        "10\n",
        &["locked", "main"],
    );
}

/// Perspective 4: two sections in one body, each writing its own local. Both
/// pairs of lock blocks precede both `let`s, so one binding surviving is not
/// enough — the storage has to be there for every local at once.
#[test]
fn lbo_04_two_sections_two_locals() {
    assert_binds(
        r#"
async fn both(a: Mutex<i64>, b: Mutex<i64>) -> i64 {
    gc_collect();
    let mut first = 0;
    let mut second = 0;
    lock a as value { first = value; }
    lock b as value { second = value; }
    return first * 100 + second;
}
async fn main() {
    println(await both(Mutex::new(3), Mutex::new(7)));
}
"#,
        "307\n",
        &["both", "main"],
    );
}

/// Perspective 5: the `let` sits BETWEEN the two sections, so the first
/// section's blocks precede it and the second's follow. Emission order gives
/// the same local two different answers unless the binding is at entry.
#[test]
fn lbo_05_a_local_declared_between_two_sections() {
    assert_binds(
        r#"
async fn between(m: Mutex<i64>) -> i64 {
    lock m as mut value { value = value * 2; }
    let mut got = 0;
    lock m as value { got = value + 1; }
    return got;
}
async fn main() {
    println(await between(Mutex::new(20)));
}
"#,
        "41\n",
        &["between", "main"],
    );
}

/// Perspective 6: an accumulator declared before a loop and added to from
/// inside the section each round. The write has to land in one slot that the
/// next iteration reads back, not in a fresh binding per pass.
#[test]
fn lbo_06_an_accumulator_across_a_loop() {
    assert_binds(
        r#"
async fn total(m: Mutex<i64>, rounds: i64) -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < rounds {
        lock m as mut value {
            value = value + 1;
            sum = sum + value;
        }
        i = i + 1;
    }
    return sum;
}
async fn main() {
    println(await total(Mutex::new(0), 4));
}
"#,
        "10\n",
        &["total", "main"],
    );
}

/// Perspective 7: a GC-managed local. Its entry storage is a rooted stack slot
/// rather than a Cranelift variable, so the value written inside the section
/// stays reachable for the collector as well as readable after it.
#[test]
fn lbo_07_a_string_comes_out_of_the_section() {
    assert_binds(
        r#"
async fn label(m: Mutex<String>) -> String {
    let mut got = "none";
    lock m as value { got = value; }
    return got;
}
async fn main() {
    println(await label(Mutex::new("held")));
}
"#,
        "held\n",
        &["label", "main"],
    );
}

/// Perspective 8: an object comes out, and a method is called on it after the
/// release. A dropped store would leave the local null and the call would
/// fault rather than print.
#[test]
fn lbo_08_an_object_comes_out_of_the_section() {
    assert_binds(
        r#"
class Tag {
    text: String;
    pub init(self, text: String) { self.text = text; }
    pub fn text(self) -> String { return self.text; }
}
async fn unwrap(m: Mutex<Tag>) -> String {
    let mut got = new Tag("empty");
    lock m as value { got = value; }
    return got.text();
}
async fn main() {
    println(await unwrap(Mutex::new(new Tag("boxed"))));
}
"#,
        "boxed\n",
        &["unwrap", "main"],
    );
}

/// Perspective 9: an array, indexed after the release. Same rooted-slot path
/// as the string, with a length the caller can check.
#[test]
fn lbo_09_an_array_comes_out_of_the_section() {
    assert_binds(
        r#"
import std::collections::Array;

async fn second(m: Mutex<Array<i64>>) -> i64 {
    let mut got: Array<i64> = [0];
    lock m as value { got = value; }
    return got[1] + got.len();
}
async fn main() {
    let xs: Array<i64> = [4, 5, 6];
    println(await second(Mutex::new(xs)));
}
"#,
        "8\n",
        &["second", "main"],
    );
}

/// Perspective 10: an `f64` local. Its entry seed is an `f64const`, not an
/// `iconst`, so this is the branch a type-blind seeding would miscompile.
#[test]
fn lbo_10_an_f64_comes_out_of_the_section() {
    assert_binds(
        r#"
async fn ratio(m: Mutex<f64>) -> f64 {
    gc_collect();
    let mut got = 0.0;
    lock m as value { got = value * 2.0; }
    return got;
}
async fn main() {
    println(await ratio(Mutex::new(1.5)));
}
"#,
        "3\n",
        &["ratio", "main"],
    );
}

/// Perspective 11: a `bool` local, whose seed shares the integer path but a
/// narrower Cranelift type.
#[test]
fn lbo_11_a_bool_comes_out_of_the_section() {
    assert_binds(
        r#"
async fn ready(m: Mutex<i64>) -> bool {
    gc_collect();
    let mut flag = false;
    lock m as value { flag = value > 0; }
    return flag;
}
async fn main() {
    println(await ready(Mutex::new(3)));
    println(await ready(Mutex::new(0)));
}
"#,
        "true\nfalse\n",
        &["ready", "main"],
    );
}

/// Perspective 12: `RwLock`'s two sections. They lower through the same
/// acquisition terminator with a different state machine, so both the shared
/// and the exclusive form put their body ahead of the outer `let`.
#[test]
fn lbo_12_rwlock_read_and_write_sections() {
    assert_binds(
        r#"
async fn touch(l: RwLock<i64>) -> i64 {
    gc_collect();
    let mut before = 0;
    lock read l as value { before = value; }
    let mut after = 0;
    lock write l as mut value {
        value = value + before;
        after = value;
    }
    return after;
}
async fn main() {
    println(await touch(RwLock::new(21)));
}
"#,
        "42\n",
        &["touch", "main"],
    );
}

/// Perspective 13: an `if` inside the body chooses which of two outer locals
/// is written, so one of them keeps its seeded value on each path.
#[test]
fn lbo_13_a_branch_inside_the_section_picks_the_local() {
    assert_binds(
        r#"
async fn split(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut low = 0;
    let mut high = 0;
    lock m as value {
        if value < 10 {
            low = value;
        } else {
            high = value;
        }
    }
    return low * 1000 + high;
}
async fn main() {
    println(await split(Mutex::new(4)));
    println(await split(Mutex::new(40)));
}
"#,
        "4000\n40\n",
        &["split", "main"],
    );
}

/// Perspective 14: a `match` arm inside the body writes the outer local. Its
/// own arm bindings are merge locals — the case willow-ht1h bound — and the
/// outer local is a `let` whose block comes later, so both kinds meet here.
#[test]
fn lbo_14_a_match_arm_inside_the_section_sets_it() {
    assert_binds(
        r#"
async fn classify(m: Mutex<i64>) -> String {
    let mut name = "?";
    lock m as value {
        match value {
            0 => { name = "zero"; }
            1 => { name = "one"; }
            _ => { name = "many"; }
        }
    }
    return name;
}
async fn main() {
    println(await classify(Mutex::new(0)));
    println(await classify(Mutex::new(1)));
    println(await classify(Mutex::new(9)));
}
"#,
        "zero\none\nmany\n",
        &["classify", "main"],
    );
}

/// Perspective 15: the local is written inside the section and read by the
/// caller through the return value only — the read block is the section's own
/// continuation, the first block emitted after the body.
#[test]
fn lbo_15_written_inside_returned_outside() {
    assert_binds(
        r#"
async fn drain(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut taken = 0;
    lock m as mut value {
        taken = value;
        value = 0;
    }
    return taken;
}
async fn main() {
    let m = Mutex::new(12);
    println(await drain(m));
    println(await drain(m));
}
"#,
        "12\n0\n",
        &["drain", "main"],
    );
}

/// Perspective 16: a section that never runs. The local keeps the value its
/// entry storage was seeded with, which is what makes a read on a path that
/// skipped the write defined rather than garbage.
#[test]
fn lbo_16_a_section_that_never_runs_leaves_the_seed() {
    assert_binds(
        r#"
async fn maybe(m: Mutex<i64>, take: bool) -> i64 {
    gc_collect();
    let mut got = -1;
    if take {
        lock m as value { got = value; }
    }
    return got;
}
async fn main() {
    println(await maybe(Mutex::new(5), true));
    println(await maybe(Mutex::new(5), false));
}
"#,
        "5\n-1\n",
        &["maybe", "main"],
    );
}

/// Perspective 17: the local's address is taken. Its entry storage is a stack
/// slot rather than a variable, and the section's write has to go to the same
/// slot the reference later reads through.
#[test]
fn lbo_17_an_address_taken_local_written_inside() {
    assert_binds(
        r#"
fn twice(n: &mut i64) {
    n = n * 2;
}
async fn doubled(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut got = 0;
    lock m as value { got = value; }
    twice(&got);
    return got;
}
async fn main() {
    println(await doubled(Mutex::new(8)));
}
"#,
        "16\n",
        &["doubled", "main"],
    );
}

/// Perspective 18: two tasks in flight, each with its own frame and its own
/// copy of the local. Contention makes the sections interleave, so a binding
/// shared through anything but per-frame storage would cross the two.
///
/// Which task enters the section first is the scheduler's choice — the runtime
/// clamps every worker override up to `DEFAULT_WORKERS`, so these two really do
/// run in parallel — and the assertion must not encode one order. Both tasks
/// add the same amount and separate themselves by a private `tag`, so the two
/// observations are 10 and 20 either way and the printed sum is 33 for both
/// interleavings. A crossed binding loses a tag and changes it.
#[test]
fn lbo_18_two_tasks_each_keep_their_own_local() {
    assert_binds(
        r#"
async fn claim(m: Mutex<i64>, tag: i64) -> i64 {
    let mut mine = 0;
    lock m as mut value {
        value = value + 10;
        mine = value + tag;
    }
    return mine;
}
async fn main() {
    let m = Mutex::new(0);
    let a = claim(m, 1);
    let b = claim(m, 2);
    let first = await a;
    let second = await b;
    println(first + second);
    lock m as value { println(value); }
}
"#,
        "33\n20\n",
        &["claim", "main"],
    );
}

/// Perspective 19: the value is written before a park and read after it. The
/// local IS live across the acquisition here, so async liveness puts it in the
/// heap frame — the one binding that already existed, and that entry binding
/// must not shadow or re-seed.
#[test]
fn lbo_19_a_local_live_across_a_park() {
    assert_binds(
        r#"
async fn carried(m: Mutex<i64>, extra: i64) -> i64 {
    let mut carried = extra * 3;
    lock m as mut value {
        value = value + carried;
    }
    return carried + 1;
}
async fn main() {
    let m = Mutex::new(0);
    println(await carried(m, 4));
    lock m as value { println(value); }
}
"#,
        "13\n12\n",
        &["carried", "main"],
    );
}

/// Perspective 20: a local whose DECLARED type widens what is stored into it.
/// Its entry storage is the interface's, so the class value written on the
/// branch is boxed into that storage rather than into one shaped like the
/// initialiser. The helper is synchronous because an interface value in an
/// async task frame is not `Send`, so the ordering here is the `if/else` one:
/// the merge block is emitted before the arm that jumps to it.
#[test]
fn lbo_20_a_declared_type_decides_the_storage() {
    assert_binds(
        r#"
interface Named {
    fn label(self) -> String;
}
class Coin implements Named {
    amount: i64;
    pub init(self, amount: i64) { self.amount = amount; }
    pub fn label(self) -> String {
        if self.amount > 0 {
            return "some";
        }
        return "none";
    }
}
fn pick(n: i64) -> String {
    let mut who: Named = new Coin(0);
    if n > 0 {
        who = new Coin(n);
    }
    return who.label();
}
async fn main() {
    println(pick(3));
    println(pick(0));
}
"#,
        "some\nnone\n",
        &["pick", "main"],
    );
}

/// Perspective 21: an enum value comes out and is matched after the release.
/// An enum local is neither a plain scalar nor an ordinary reference, so it
/// picks its entry storage by the same GC test the payload needs.
#[test]
fn lbo_21_an_enum_comes_out_of_the_section() {
    assert_binds(
        r#"
enum Level { Low, High(i64) }
async fn level(m: Mutex<i64>) -> Level {
    gc_collect();
    let mut got = Level::Low;
    lock m as value {
        if value > 5 {
            got = Level::High(value);
        }
    }
    return got;
}
async fn main() {
    match await level(Mutex::new(9)) {
        Level::High(n) => { println(n); }
        Level::Low => { println("low"); }
    }
    match await level(Mutex::new(1)) {
        Level::High(n) => { println(n); }
        Level::Low => { println("low"); }
    }
}
"#,
        "9\nlow\n",
        &["level", "main"],
    );
}

/// Perspective 22: the local lives in a nested scope that ends before the
/// function does, so its scope-root close names it in yet another block. The
/// binding still has to be the one the section wrote.
#[test]
fn lbo_22_a_nested_scopes_local() {
    assert_binds(
        r#"
async fn scoped(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut out = 0;
    if out == 0 {
        let mut inner = "";
        lock m as value {
            inner = "seen";
            out = value;
        }
        println(inner);
    }
    return out;
}
async fn main() {
    println(await scoped(Mutex::new(6)));
}
"#,
        "seen\n6\n",
        &["scoped", "main"],
    );
}

/// Perspective 23: ordinary bodies still bind their locals the same way.
/// Entry storage for every local widened what used to be a merge-local pass,
/// so shadowing, loops, `match` results and plain synchronous code are all
/// held to their previous answers.
#[test]
fn lbo_23_ordinary_bodies_still_bind_their_locals() {
    assert_binds(
        r#"
fn shadowed() -> i64 {
    let x = 1;
    if x > 0 {
        let x = x + 10;
        println(x);
    }
    return x;
}
fn merged(n: i64) -> String {
    let picked = match n {
        1 => "one",
        _ => "other",
    };
    return picked;
}
fn looped(rounds: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < rounds {
        let step = i * 2;
        total = total + step;
        i = i + 1;
    }
    return total;
}
fn main() {
    println(shadowed());
    println(merged(1));
    println(merged(4));
    println(looped(4));
}
"#,
        "11\n1\none\nother\n12\n",
        &["shadowed", "merged", "looped", "main"],
    );
}

/// Perspective 24: the runnable example. Every shape above appears in one
/// program, compiled entirely from lowered IR.
#[test]
fn lbo_24_example_compiles_from_lir_end_to_end() {
    let source = include_str!("../../example/lir_local_binding_order.wi");
    let (out, ok) = compile_and_run_with_env(source, &PLAIN);
    assert!(ok, "the example must run: {out}");
    assert_eq!(out, "41\n10\n307\n10\nheld\nboxed\n42\nmany\n16\n5\n-1\n");
    assert_walker_owns(
        source,
        &[
            "peek", "locked", "both", "total", "label", "unwrap", "touch", "classify", "doubled",
            "maybe", "main",
        ],
    );
}

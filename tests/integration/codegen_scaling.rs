//! Structural scaling of emitted code and data (epic willow-ssl7).
//!
//! These tests do not time anything. Each one compiles a program whose SIZE is
//! parameterized, keeps the intermediate object, and counts symbols or
//! relocation records in it — a count that grows with the wrong parameter is
//! exactly the bottleneck the epic removes, and it is reproducible on every
//! target and build machine.
//!
//! Three emitters are covered.
//!
//! * Normal-exit defer cleanup (willow-xgfk). A function with `D` defers and
//!   `E` early exits used to emit one full flush per exit, i.e. `D * E`
//!   actions. The flush is now emitted once per distinct registration set and
//!   entered with a continuation selector, so the action count is `D` and the
//!   exits only contribute a jump each.
//! * `?` error conversion through `Into` (willow-76bf). The conversion site
//!   used to switch over every subclass of the static error class and call each
//!   one's `into` directly, so `S` conversion sites over `K` subclasses emitted
//!   `S * K` calls. It now goes through ordinary virtual dispatch: one indirect
//!   call per site, and the only per-class references left are the ones the
//!   class descriptors own anyway.
//! * Interface vtables (willow-ssl7.5). A vtable used to embed each super's
//!   table verbatim, so a diamond stored one copy of a shared ancestor per
//!   inheritance PATH — doubling per level in the hierarchy below. It now
//!   stores composed method pointers once followed by one pointer per direct
//!   super, so both the table count and the pointer count are linear in
//!   interfaces and edges.
//!
//! 23 perspectives:
//!   1 cleanup actions are independent of the exit count
//!   2 cleanup actions are linear in the defer count
//!   3 the shared flush still runs defers LIFO at an early return
//!   4 ... and on the fallthrough exit
//!   5 ... on `break` and `continue` exits of the same function
//!   6 ... on a `?` propagation exit
//!   7 nested scopes flush only the scopes they leave
//!   8 a defer body reading a local sees that path's value
//!   9 a recovered panic still runs the pending defers
//!  10 a reference-parameter call keeps its own local cleanup
//!  11 an async fn's defers survive the poll-shaped body
//!  12 an async cancellation runs the cleanup region that suspends
//!  13 `into` references are independent of the conversion-site count
//!  14 a non-virtual `into` stays a direct call
//!  15 an overriding subclass converts through its override
//!  16 the base class converts through its own `into`
//!  17 an inherited (not overridden) `into` reaches the base body
//!  18 one vtable per (class, reachable interface), not per path
//!  19 vtable pointers stay linear as the path count doubles
//!  20 widening reaches the shared root through either branch
//!  21 the deep diamond dispatches correctly under GC stress
//!  22 the runnable example's widening survives GC stress
//!  23 call instructions are counted apart from table slots

use super::support::{
    compile_and_collect_defined_symbols, compile_and_collect_relocation_targets_all,
    compile_and_collect_relocations_by_section, compile_and_run, compile_and_run_gc_stress,
};

/// No extra compiler environment: the ordinary debug build.
const PLAIN: [(&str, &str); 0] = [];

fn count_of(names: &[String], predicate: impl Fn(&str) -> bool) -> usize {
    names.iter().filter(|name| predicate(name)).count()
}

// ── willow-xgfk: one normal-exit flush per registration set ──────────────────

/// `D` defers, each printing a distinct number, and `E` early returns.
fn defers_and_exits(defers: usize, exits: usize) -> String {
    let mut source = String::from("fn work(n: i64) -> i64 {\n");
    for index in 0..defers {
        source.push_str(&format!("    defer println({});\n", 100 + index));
    }
    for exit in 0..exits {
        source.push_str(&format!("    if n == {exit} {{ return {exit}; }}\n"));
    }
    source.push_str("    return -1;\n}\nfn main() { println(work(0)); }\n");
    source
}

/// The emitted `println` call sites of the cleanup actions. `main`'s own call
/// adds the one constant.
fn cleanup_actions(defers: usize, exits: usize) -> usize {
    let names =
        compile_and_collect_relocation_targets_all(&defers_and_exits(defers, exits), &PLAIN);
    count_of(&names, |name| name == "willow_println_i64")
}

#[test]
fn scaling_01_cleanup_is_independent_of_the_exit_count() {
    // Eight defers, and one to thirty-two exits that all flush them: the
    // actions are emitted once, so only the jumps differ between these.
    for exits in [1usize, 2, 4, 8, 16, 32] {
        assert_eq!(
            cleanup_actions(8, exits),
            9,
            "eight defers behind {exits} exits"
        );
    }
}

#[test]
fn scaling_02_cleanup_is_linear_in_the_defer_count() {
    for defers in [1usize, 2, 4, 8, 16] {
        assert_eq!(
            cleanup_actions(defers, 8),
            defers + 1,
            "{defers} defers behind eight exits"
        );
    }
}

#[test]
fn scaling_03_shared_flush_runs_defers_lifo_at_an_early_return() {
    let (out, ok) = compile_and_run(&defers_and_exits(4, 4));
    assert!(ok, "{out}");
    assert_eq!(out, "103\n102\n101\n100\n0\n");
}

#[test]
fn scaling_04_shared_flush_runs_on_the_fallthrough_exit() {
    let (out, ok) = compile_and_run(
        "fn work(n: i64) -> i64 {
    defer println(1);
    defer println(2);
    if n == 7 { return 7; }
    return 0;
}
fn main() { println(work(3)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n0\n");
}

#[test]
fn scaling_05_shared_flush_runs_on_break_and_continue() {
    let (out, ok) = compile_and_run(
        "fn work(limit: i64) -> i64 {
    defer println(9);
    let mut total = 0;
    for i in 0..limit {
        defer println(i);
        if i == 1 { continue; }
        if i == 3 { break; }
        total = total + i;
    }
    return total;
}
fn main() { println(work(6)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n3\n9\n2\n");
}

#[test]
fn scaling_06_shared_flush_runs_on_a_try_propagation_exit() {
    let (out, ok) = compile_and_run(
        "fn fallible(n: i64) -> Result<i64, String> {
    if n == 0 { return Err(\"zero\"); }
    return Ok(n);
}
fn work(n: i64) -> Result<i64, String> {
    defer println(1);
    defer println(2);
    let v = fallible(n)?;
    defer println(3);
    return Ok(v);
}
fn main() {
    match work(0) { Ok(v) => println(v), Err(e) => println(e), }
    match work(5) { Ok(v) => println(v), Err(e) => println(e), }
}
",
    );
    assert!(ok, "{out}");
    // The `?` exit flushes the two registrations that exist at that point; the
    // success path also runs the third.
    assert_eq!(out, "2\n1\nzero\n3\n2\n1\n5\n");
}

#[test]
fn scaling_07_nested_scopes_flush_only_what_they_leave() {
    let (out, ok) = compile_and_run(
        "fn work(n: i64) -> i64 {
    defer println(1);
    if n > 0 {
        defer println(2);
        if n > 1 {
            defer println(3);
            return 30;
        }
        return 20;
    }
    return 10;
}
fn main() { println(work(2)); println(work(1)); println(work(0)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n2\n1\n30\n2\n1\n20\n1\n10\n");
}

#[test]
fn scaling_08_defer_body_reads_the_value_of_its_own_path() {
    let (out, ok) = compile_and_run(
        "fn work(n: i64) -> i64 {
    let label = n * 2;
    defer println(label);
    if n == 1 { return 100; }
    return 200;
}
fn main() { println(work(1)); println(work(5)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n100\n10\n200\n");
}

#[test]
fn scaling_09_recovered_panic_still_runs_pending_defers() {
    // The recovery-capable registration is deliberately excluded from sharing
    // only when it must be; the ordinary registrations beside it still run on
    // both the normal exit and the panic path that a `defer recover` ends.
    let (out, ok) = compile_and_run(
        "fn work(n: i64) -> i64 {
    if true {
        defer match recover() {
            Some(info) => println(\"recovered\"),
            None => {}
        }
        defer println(1);
        let values = [1, 2, 3];
        if n > 0 { return values[n]; }
    }
    return 0;
}
fn main() { println(work(1)); println(work(9)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n1\nrecovered\n0\n");
}

#[test]
fn scaling_10_reference_parameter_calls_keep_local_cleanup() {
    // An active reference-argument preparation carries per-site diagnostic
    // state, so its cleanup is deliberately NOT merged; the defers around it
    // still run in order.
    let (out, ok) = compile_and_run(
        "fn bump(value: &mut i64) { value = value + 1; }
fn work(n: i64) -> i64 {
    let mut total = 0;
    defer println(7);
    bump(&total);
    if n == 0 { return total; }
    bump(&total);
    return total;
}
fn main() { println(work(0)); println(work(1)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n1\n7\n2\n");
}

#[test]
fn scaling_11_async_defers_survive_the_poll_shaped_body() {
    let (out, ok) = compile_and_run(
        "async fn work(n: i64) -> i64 {
    defer println(1);
    defer println(2);
    await sleep(1);
    if n == 0 { return 10; }
    return 20;
}
async fn main() { println(await work(0)); println(await work(1)); }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n10\n2\n1\n20\n");
}

#[test]
fn scaling_12_async_cancellation_runs_a_suspending_cleanup_region() {
    // The cleanup region calls a function through the task-stack boundary, so
    // the cancellation callback is itself poll-shaped: its state dispatch
    // re-enters the region, which a cleanup shared through a block parameter
    // could not survive (the selector's block would not dominate the switch).
    let (out, ok) = compile_and_run(
        "fn cleanup(ok: bool) -> Result<void, String> {
    if ok { return Ok(); }
    return Err(\"cleanup failed\");
}
async fn normal() { defer { match cleanup(false) { Ok(_) => {}, Err(_) => println(\"normal cleanup\"), } } }
async fn waiting() { defer { match cleanup(false) { Ok(_) => {}, Err(_) => println(\"cancel cleanup\"), } } await sleep(5000); }
async fn main() {
    await normal();
    let task = waiting();
    await sleep(20);
    task.cancel();
    await sleep(50);
    println(task.is_cancelled());
}
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "normal cleanup\ncancel cleanup\ntrue\n");
}

// ── willow-76bf: one virtual conversion per `?` site ─────────────────────────

/// `K` subclasses overriding `into`, and `S` functions that propagate the base
/// error type with `?`.
fn into_conversions(subclasses: usize, sites: usize) -> String {
    let mut source = String::from(
        "class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
",
    );
    for index in 1..=subclasses {
        source.push_str(&format!(
            "class Sub{index}Err extends BaseErr {{ pub override fn into(self) -> AppErr {{ return new AppErr({}); }} }}\n",
            index + 1
        ));
    }
    source.push_str(
        "fn fails() -> Result<i64, BaseErr> { let e: BaseErr = new Sub1Err(); return Result::Err(e); }\n",
    );
    for site in 0..sites {
        source.push_str(&format!(
            "fn run{site}() -> Result<i64, AppErr> {{ let v = fails()?; return Result::Ok(v + {site}); }}\n"
        ));
    }
    source.push_str("fn main() {\n");
    for site in 0..sites {
        source.push_str(&format!(
            "    match run{site}() {{ Result::Ok(v) => println(v), Result::Err(e) => println(e.code), }}\n"
        ));
    }
    source.push_str("}\n");
    source
}

/// Every relocation naming an `into` body or its virtual thunk.
fn into_references(source: &str) -> usize {
    let names = compile_and_collect_relocation_targets_all(source, &PLAIN);
    count_of(&names, |name| {
        name.ends_with(".into") || name.contains(".into$")
    })
}

/// The same references split by the section role that holds them:
/// `(instructions, table slots)`. An instruction addressing `into` is a call
/// the emitter decided to make; a data slot holding it is a vtable or
/// class-descriptor entry that exists once per class either way.
fn into_references_by_role(source: &str) -> (usize, usize) {
    let relocations = compile_and_collect_relocations_by_section(source, &PLAIN, false);
    let mut text = 0;
    let mut data = 0;
    for (role, name) in &relocations {
        if !(name.ends_with(".into") || name.contains(".into$")) {
            continue;
        }
        if role == "text" {
            text += 1;
        } else {
            data += 1;
        }
    }
    (text, data)
}

#[test]
fn scaling_13_into_references_are_independent_of_the_site_count() {
    // Eight overriding subclasses: each contributes the references its own
    // descriptor and thunk need, and no conversion site adds any.
    let baseline = into_references(&into_conversions(8, 1));
    for sites in [2usize, 4, 8] {
        assert_eq!(
            into_references(&into_conversions(8, sites)),
            baseline,
            "eight subclasses converted at {sites} sites"
        );
    }
    // Those per-class references are the only thing that grows, and they grow
    // with the classes rather than with the conversions.
    assert!(
        into_references(&into_conversions(16, 1)) > baseline,
        "a new subclass owns new references"
    );
}

#[test]
fn scaling_14_a_non_virtual_into_stays_a_direct_call() {
    // No `open`/`override` pair, so nothing can replace the body: the site
    // calls it directly instead of loading a slot, which is one reference per
    // site plus the one the class descriptor owns.
    for sites in [1usize, 2, 4] {
        let mut source = String::from(
            "class AppErr { pub code: i64; }
class OneErr implements Into<AppErr> { pub fn into(self) -> AppErr { return new AppErr(7); } }
fn fails() -> Result<i64, OneErr> { return Result::Err(new OneErr()); }
",
        );
        for site in 0..sites {
            source.push_str(&format!(
                "fn run{site}() -> Result<i64, AppErr> {{ let v = fails()?; return Result::Ok(v + {site}); }}\n"
            ));
        }
        source.push_str("fn main() {\n");
        for site in 0..sites {
            source.push_str(&format!(
                "    match run{site}() {{ Result::Ok(v) => println(v), Result::Err(e) => println(e.code), }}\n"
            ));
        }
        source.push_str("}\n");
        let names = compile_and_collect_relocation_targets_all(&source, &PLAIN);
        assert_eq!(
            count_of(&names, |name| name.contains("into$virtual_thunk")),
            0,
            "nothing can override it, so no thunk: {names:?}"
        );
        // One call instruction per site, and the single data slot the class
        // descriptor owns however many sites there are.
        assert_eq!(into_references_by_role(&source), (sites, 1));
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "7\n".repeat(sites));
    }
}

#[test]
fn scaling_15_an_overriding_subclass_converts_through_its_override() {
    let (out, ok) = compile_and_run(&into_conversions(4, 1));
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn scaling_16_the_base_class_converts_through_its_own_into() {
    let (out, ok) = compile_and_run(
        "class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SubErr extends BaseErr { pub override fn into(self) -> AppErr { return new AppErr(99); } }
fn fails() -> Result<i64, BaseErr> { return Result::Err(new BaseErr()); }
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() { match run() { Result::Ok(v) => println(v), Result::Err(e) => println(e.code), } }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn scaling_17_an_inherited_into_reaches_the_base_body() {
    let (out, ok) = compile_and_run(
        "class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(5); }
}
class QuietErr extends BaseErr {}
fn fails() -> Result<i64, BaseErr> { let e: BaseErr = new QuietErr(); return Result::Err(e); }
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() { match run() { Result::Ok(v) => println(v), Result::Err(e) => println(e.code), } }
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

// ── willow-ssl7.5: shared super tables instead of embedded copies ────────────

/// A hierarchy whose inheritance PATH count doubles per level: every level has
/// two interfaces, each extending both interfaces of the level below, so there
/// are `2^(levels - 1)` distinct paths from the top interface to `Root` and
/// only `2 * levels` interfaces reachable from it.
fn doubling_diamond(levels: usize) -> String {
    let mut source = String::from("interface Root { fn ping(self) -> i64; }\n");
    for level in 1..=levels {
        let supers = if level == 1 {
            "Root".to_string()
        } else {
            format!("A{}, B{}", level - 1, level - 1)
        };
        source.push_str(&format!("interface A{level} extends {supers} {{}}\n"));
        source.push_str(&format!("interface B{level} extends {supers} {{}}\n"));
    }
    source.push_str("class C implements A");
    source.push_str(&levels.to_string());
    source.push_str(" { pub fn ping(self) -> i64 { return 7; } }\n");
    source.push_str("fn take(r: Root) -> i64 { return r.ping(); }\n");
    source.push_str(&format!(
        "fn main() {{ let t: A{levels} = new C(); println(take(t)); }}\n"
    ));
    source
}

#[test]
fn scaling_18_one_vtable_per_reachable_interface_not_per_path() {
    for levels in [2usize, 4, 8, 12] {
        let names = compile_and_collect_defined_symbols(&doubling_diamond(levels), &PLAIN);
        let tables = count_of(&names, |name| name.ends_with("$vtable"));
        // `A{levels}` plus both interfaces of every level below it plus `Root`.
        assert_eq!(
            tables,
            2 * levels,
            "{levels} levels defined {tables} tables"
        );
    }
}

#[test]
fn scaling_19_vtable_pointers_stay_linear_as_paths_double() {
    let mut counts = Vec::new();
    for levels in [2usize, 4, 8, 12] {
        let names = compile_and_collect_relocation_targets_all(&doubling_diamond(levels), &PLAIN);
        let pointers = count_of(&names, |name| name.ends_with("$vtable"));
        // One pointer per inheritance EDGE, plus the box construction in
        // `main`. A layout that embedded each super's table verbatim would
        // instead need one copy per path: 2^(levels - 1).
        assert!(
            pointers <= 8 * levels,
            "{levels} levels needed {pointers} vtable references"
        );
        counts.push((levels, pointers));
    }
    let (_, small) = counts[1];
    let (_, large) = counts[3];
    // Tripling the levels triples the edges; the path count grows 2^8 times.
    assert!(
        large < 4 * small,
        "vtable references grew from {small} to {large}"
    );
}

#[test]
fn scaling_20_widening_reaches_the_shared_root_through_either_branch() {
    let (out, ok) = compile_and_run(
        "interface Root { fn ping(self) -> i64; }
interface Left extends Root { fn left(self) -> i64; }
interface Right extends Root { fn right(self) -> i64; }
interface Both extends Left, Right { fn both(self) -> i64; }
class C implements Both {
    pub fn ping(self) -> i64 { return 1; }
    pub fn left(self) -> i64 { return 2; }
    pub fn right(self) -> i64 { return 3; }
    pub fn both(self) -> i64 { return 4; }
}
fn ping(r: Root) -> i64 { return r.ping(); }
fn main() {
    let b: Both = new C();
    println(b.both());
    println(ping(b));
    let l: Left = b;
    println(l.left());
    println(ping(l));
    let r: Right = b;
    println(r.right());
    println(ping(r));
}
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n1\n2\n1\n3\n1\n");
}

#[test]
fn scaling_21_deep_diamond_dispatches_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(&doubling_diamond(8));
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn scaling_22_the_widening_example_survives_gc_stress() {
    let (out, ok) =
        compile_and_run_gc_stress(include_str!("../../example/interface_diamond_widening.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "700\n7\n70\ncard\n7\n7\ncard\n");
}

// ── willow-76bf: instructions and table slots counted apart ──────────────────

#[test]
fn scaling_23_call_instructions_are_counted_apart_from_table_slots() {
    // The two numbers answer different questions, and only one of them was
    // ever the bug: how many conversion CALLS the emitter wrote, versus how
    // many tables mention a conversion at all. With eight overriding
    // subclasses every site dispatches indirectly through the vtable, so the
    // instruction count is zero at any number of sites, and the data slots are
    // the two each class owns anyway — its own body and its virtual thunk.
    let (_, baseline_data) = into_references_by_role(&into_conversions(8, 1));
    assert_eq!(baseline_data, 2 * 9, "nine classes, body and thunk each");
    for sites in [2usize, 4, 8] {
        let (text, data) = into_references_by_role(&into_conversions(8, sites));
        assert_eq!(
            (text, data),
            (0, baseline_data),
            "eight subclasses converted at {sites} sites"
        );
    }
    // Adding classes adds table slots, still with no call instruction.
    let (text, data) = into_references_by_role(&into_conversions(16, 1));
    assert_eq!(text, 0);
    assert_eq!(data, 2 * 17);
}

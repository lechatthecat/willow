use super::support::*;

#[path = "codegen/static_members.rs"]
mod static_members;

// ── Single-item imports (willow-om7, spec 10 / 12.2) ───────────────────────

fn math_module() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn mul(a: i64, b: i64) -> i64 { return a * b; }\nfn secret() -> i64 { return 99; }\n",
    )
}

// ── Module aliases + `::` access; `.` reserved for instances (willow-u98) ──

fn aliasable_math() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn square(n: i64) -> i64 { return n * n; }\n",
    )
}

// ── Import visibility + collision diagnostics (willow-pwa, spec 11/13) ─────

fn s5_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "a.wi",
            "module a;\npub fn f() -> i64 { return 1; }\npub fn dup() -> i64 { return 10; }\nfn hidden() -> i64 { return 9; }\n",
        ),
        (
            "b.wi",
            "module b;\npub fn g() -> i64 { return 2; }\npub fn dup() -> i64 { return 20; }\n",
        ),
    ]
}

fn s5_project(main: &str) -> Vec<(&'static str, &'static str)> {
    let mut v = s5_modules();
    v.insert(0, ("main.wi", Box::leak(main.to_string().into_boxed_str())));
    v
}

fn assert_catalog_lines(out: &str, cases: &[(&str, &str)]) {
    let actual = out.lines().collect::<Vec<_>>();
    assert_eq!(
        actual.len(),
        cases.len(),
        "catalog output line count mismatch:\n{out}"
    );
    for (index, ((name, expected), actual)) in cases.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            *actual,
            *expected,
            "catalog case {} ({name}) failed",
            index + 1
        );
    }
}

// ── willow-ca2: lexer numeric/comment diagnostics (end-to-end) ───────────────

// End-to-end: an integer literal that overflows i64 surfaces as E0052 through
// the full compiler (previously it was silently parsed as 0).

// ── LIR-walking backend (willow-0g8j) ────────────────────────────────────────
// Every function body compiles from the lowered IR (willow-0g8j.3 retired the
// AST emitter for function bodies, so a body outside the walker's reach is a
// compile error naming the construct rather than a second emitter's answer).
// The perspectives below pin the answers themselves: recursion, loops
// (range-for + while), f64 arithmetic, bool logic + unary, nested calls, early
// returns, assignment-heavy bodies, and panic call-chain instrumentation.

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];

/// Make the walker name every function it compiles from lowered IR.
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];

fn assert_program_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

// ── LIR backend: GC-managed values and rooting (willow-0g8j.1) ──────────────
// `String` is the first GC-managed type the LIR walker emits. Because the LIR
// is a FLAT basic-block graph with no scopes, a GC-managed local cannot use
// the AST path's per-`let` root push/pop (that would grow the shadow root
// stack once per loop iteration). Instead each GC local gets one
// null-initialized, entry-rooted stack slot that is simultaneously its storage
// and its root, and every `return` pops the whole set.
//
// 20 perspectives, continuing the numbering of the scalar tests above (12-31).
// Each either asserts the program's output outright, or runs it under
// WILLOW_GC_STRESS=alloc — the mode that collects at every allocation, so any
// unrooted live value is reclaimed and the printed text changes:
//
// 12 String param passed through and printed, 13 concat chain, 14 equality,
// 15 inequality, 16 String ternary, 17 loop accumulator, 18 mixed scalar/GC
// locals, 19 nested String calls, 20 a local live across a LATER allocation
// (the case the entry root exists for), 21 the same under GC stress, 22 loop
// accumulator under GC stress, 23 several GC locals live at once under stress,
// 24 a GC call argument rooted across a second argument's allocation,
// 25 concat lhs rooted across the rhs allocation, 26 equality operands survive
// a collection, 27 early return out of a loop leaves the root stack balanced,
// 28 deep recursion with GC locals (push/pop balance across frames), 29 a void
// function's implicit return pops its roots, 30 a GC slot still unassigned
// when a collection scans it (null-initialized, must not crash), 31 print vs
// println of a String.

/// Run `source` with a collection forced at every allocation and require the
/// same successful output.
fn assert_output_under_gc_stress(source: &str, expected: &str) {
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (out, ok) = compile_with_env_and_run_under(source, &PLAIN, &stress);
    assert!(ok, "GC-stress run failed: {out}");
    assert_eq!(out, expected);
}

/// The same, for a multi-file project.
fn assert_project_output(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, entry, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

/// A module whose public `Animal` is `open` and extended by `Dog`.
const ZOO_MODULE: &str = r#"
module zoo;

pub open class Animal {
    pub value: i64;
    pub open fn speak(self) -> i64 { return self.value; }
}

pub class Dog extends Animal {
    pub override fn speak(self) -> i64 { return self.value + 1000; }
}
"#;

// ── LIR walker: class -> interface boxing (willow-j260) ─────────────────────
// An interface-typed slot does not hold a class pointer, it holds a 16-byte GC
// box `[object | vtable]`. So every store of a class value into an interface
// position is a conversion the walker has to emit, and — because the box
// allocates AFTER the value expression has already produced a live object —
// every such site also has to count as allocating for rooting purposes.
//
// Perspectives j01-j21 are the eligibility half, in
// `src/backend/cranelift/lir_gen.rs`. j22-j36 below are the emitted-code half:
// one store site per test, first as a plain run and then under
// WILLOW_GC_STRESS=alloc, where a value left unrooted across the box
// allocation is reclaimed and the printed text changes.
//
// j22 widening `let`, j23 widening assignment, j24 boxed call argument,
// j25 boxed `return`, j26 boxed field store, j27 memberwise `new`,
// j28 explicit `init`, j29 boxed index-assign, j30 boxed `push`,
// j31 an interface value re-stored (must NOT be boxed twice), then under GC
// stress: j32 boxes built in a loop, j33 the owner rooted across a field
// store's box, j34 a boxed argument rooted across a later allocating argument,
// j35 the array handle rooted across a pushed box, j36 early return with live
// boxes leaves the root stack balanced.
//
// j37-j50 (willow-j260.1) cover the case the tests above never reach: ONE
// object boxed into TWO DIFFERENT interfaces. That splits into two properties
// nothing else pins down.
//
//   * vtable SELECTION. `resolve_vtable_id` is keyed `(class, interface)`, so
//     one class has as many vtables as it has interfaces. A regression that
//     resolved per-CLASS would still satisfy every j22-j36 test, because each
//     of those classes is behind exactly one interface. `Ends` below makes the
//     mistake observable: `Front` and `Back` declare the SAME two methods in
//     OPPOSITE order, so the wrong vtable answers `head()` with the tail.
//
//   * shared IDENTITY. A box copies a pointer, it does not copy the object, so
//     two boxes over one object must still see one set of fields. Mutating
//     through one box and reading back through the other is the proof; run
//     under GC stress it also proves the object survived the SECOND box's
//     allocation, which happens while the first box and the half-initialized
//     owner are already live.
//
// j37 two interfaces in one memberwise `new`, j38 mutate through one box and
// read through the other, j39 reversed slot order, j40 boxing order reversed,
// j41 two interface parameters at one call site, j42 two widening `let`s,
// j43 the same object boxed TWICE into the SAME interface, j44 re-pointing one
// field leaves the other's object alone, j45 one object in two differently
// typed arrays, j46 two interface-returning functions over one object; then
// under WILLOW_GC_STRESS=alloc: j47 identity survives the second box's
// allocation in a loop, j48 the first box and the half-initialized owner stay
// rooted across the second box, j49 the reversed-slot pair built in a loop,
// j50 two boxes of one object as two allocating call arguments.

/// Shared shape for the boxing tests. The only way to OBSERVE a box is to read
/// back through it, and when these tests were written a virtual `name()` call
/// was still outside the walker — so every read happens inside a class METHOD.
/// Dispatch has since joined the subset (willow-0g8j.6, the k24+ block below);
/// the reads stay in methods here so these tests keep isolating the STORE
/// side.
const BOXING_PRELUDE: &str = r##"
import std::collections::Array;

interface Named { fn name(self) -> String; }

class Item implements Named {
    pub label: String;
    pub fn name(self) -> String { return self.label; }
}

class Tag implements Named {
    pub n: i64;
    pub fn name(self) -> String { return "#" + self.n.toString(); }
}

class Cell {
    pub v: Named;
    pub fn read(self) -> String { return self.v.name(); }
}

class Row {
    pub xs: Array<Named>;
    pub fn joined(self) -> String {
        let mut o = "";
        let mut i = 0;
        while i < self.xs.len() { o = o + self.xs[i].name(); i = i + 1; }
        return o;
    }
}

fn named(s: String) -> Named { return new Item(s); }

// One class behind TWO interfaces (j37+). `Ticker` slot 0 is `tick`, `Counted`
// slot 0 is `count`, so a box that carried the wrong vtable would call the
// wrong function through the same slot index.
interface Ticker { fn tick(self); }
interface Counted { fn count(self) -> i64; }

class Meter implements Ticker, Counted {
    pub hits: i64;
    pub fn tick(self) { self.hits = self.hits + 1; }
    pub fn count(self) -> i64 { return self.hits; }
}

// Holds ONE object twice, once behind each interface.
class TwoWay {
    pub ticker: Ticker;
    pub reader: Counted;

    // Mutate through one box, read back through the OTHER. The answer is only
    // right if both boxes hold the same concrete object.
    pub fn bumpThenRead(self, times: i64) -> i64 {
        let mut i = 0;
        while i < times { self.ticker.tick(); i = i + 1; }
        return self.reader.count();
    }

    pub fn readOnly(self) -> i64 { return self.reader.count(); }
}

// The same two methods declared in OPPOSITE order, so the two interfaces
// disagree about which slot holds which method.
interface Front { fn head(self) -> String; fn tail(self) -> String; }
interface Back { fn tail(self) -> String; fn head(self) -> String; }

class Ends implements Front, Back {
    pub a: String;
    pub b: String;
    pub fn head(self) -> String { return self.a; }
    pub fn tail(self) -> String { return self.b; }
}

class Pair {
    pub front: Front;
    pub back: Back;

    // Reads each method through BOTH interfaces: a per-class vtable would make
    // the two halves disagree.
    pub fn crossed(self) -> String {
        return self.front.head() + self.back.head()
             + self.front.tail() + self.back.tail();
    }
}

class Meters {
    pub tickers: Array<Ticker>;
    pub readers: Array<Counted>;

    // Bump every element of one array, then total the OTHER array — which
    // holds the same objects behind different boxes.
    pub fn bumpAllThenTotal(self) -> i64 {
        let mut i = 0;
        while i < self.tickers.len() { self.tickers[i].tick(); i = i + 1; }
        let mut total = 0;
        let mut k = 0;
        while k < self.readers.len() { total = total + self.readers[k].count(); k = k + 1; }
        return total;
    }
}
"##;

fn boxing_source(body: &str) -> String {
    format!("{BOXING_PRELUDE}{body}")
}

// ── LIR walker: interface dispatch (willow-0g8j.6) ─────────────────────────
// Reading back THROUGH an interface: `s.area()` loads `[object | vtable]` out
// of the box, indexes the vtable by the method's declaration-order slot and
// issues an indirect call with the concrete object as the receiver. Nothing in
// the emitted code knows the class, so the only thing standing between a
// correct call and a jump into the wrong function is the slot — which
// eligibility resolves the same way the emitter does.
//
// Perspectives k01-k23 are the eligibility half, in
// `src/backend/cranelift/lir_gen.rs`. k24-k40 below are the emitted-code half:
//
// k24 two classes behind one interface pick different implementations,
// k25 a later slot with arguments, k26 an INHERITED slot (`extends`),
// k27 a DEFAULT body vs a class override, k28 a void method in statement
// position, k29 an array element receiver in a loop, k30 a field-read
// receiver, k31 a temporary receiver, k32 dispatch feeding dispatch,
// k33 an interface-returning free function's result dispatched on,
// k34 every slot of one interface called in one function, k35 recursion
// through the interface; then under WILLOW_GC_STRESS=alloc: k36 the receiver
// rooted across an allocating argument, k37 boxes built and dispatched in a
// loop, k38 a temporary receiver rooted across its own argument; and for the
// debug call chain: k39 a panic inside a dispatched method carries the method
// frame, k40 an argument panic is NOT attributed to it.

/// Shared shape for the dispatch tests: one inherited slot (`name`), one
/// required slot with an argument (`scaled`), one default body (`twice`) and a
/// void slot (`stamp`), across two implementing classes. Unlike
/// [`BOXING_PRELUDE`], the reads here are FREE functions — that is the point.
const DISPATCH_PRELUDE: &str = r##"
import std::collections::Array;

interface Named { fn name(self) -> String; }

interface Shape extends Named {
    fn area(self) -> i64;
    fn scaled(self, factor: i64) -> i64;
    fn tagged(self, extra: String) -> String;
    fn stamp(self);
    fn twice(self) -> i64 { return self.area() + self.area(); }
}

class Square implements Shape {
    pub side: i64;
    pub fn name(self) -> String { return "square"; }
    pub fn area(self) -> i64 { return self.side * self.side; }
    pub fn scaled(self, factor: i64) -> i64 { return self.area() * factor; }
    pub fn tagged(self, extra: String) -> String { return "square:" + extra; }
    pub fn stamp(self) { println("sq"); }
}

class Rect implements Shape {
    pub w: i64;
    pub h: i64;
    pub fn name(self) -> String { return "rect"; }
    pub fn area(self) -> i64 { return self.w * self.h; }
    pub fn scaled(self, factor: i64) -> i64 { return self.area() * factor; }
    pub fn tagged(self, extra: String) -> String { return "rect:" + extra; }
    pub fn stamp(self) { println("rect"); }
    pub fn twice(self) -> i64 { return self.area() * 2; }
}

class Holder {
    pub shape: Shape;
}
"##;

fn dispatch_source(body: &str) -> String {
    format!("{DISPATCH_PRELUDE}{body}")
}

// ── Interface dispatch with REFERENCE parameters (willow-0g8j.9) ────────────
//
// A `&`/`&mut` parameter is passed as a POINTER. The interface tables used to
// keep parameter TYPES only, so `emit_interface_dispatch` built its
// `call_indirect` signature from types and passed every argument by value —
// the concrete method then dereferenced an integer and the program crashed.
// These tests run the real thing end to end.
//
// Perspectives 19..30 of willow-0g8j.9 (1..18 are the type-checker tests):
// 19 `&mut i64` mutates the caller's local · 20 `& i64` reads it ·
// 21 a `&mut String` place (GC-managed) · 22 the same under GC stress ·
// 23 a field place · 24 an array element · 25 an inherited (`extends`) slot ·
// 26 mixed value and reference parameters in one signature · 27 two `&mut`
// parameters, distinct places · 28 a default-bodied slot · 29 two classes
// behind one call site · 30 a reference call through an interface, with the
// debug diagnostic hook emitted.

/// Interface slots that differ in how their parameter is passed, not in its
/// type: `nudge` takes `&mut i64`, `peek` takes `& i64`, `weigh` takes a plain
/// `i64`. Any confusion between the three is a pointer/value confusion.
const REFMODE_PRELUDE: &str = r##"
import std::collections::Array;

interface Base { fn nudge(self, value: &mut i64); }

interface Scale extends Base {
    fn peek(self, value: & i64) -> i64;
    fn weigh(self, value: i64) -> i64;
    fn rename(self, label: &mut String);
    fn spread(self, lo: &mut i64, hi: &mut i64);
    fn double(self, value: &mut i64) { self.nudge(&value); self.nudge(&value); }
}

class Step implements Scale {
    pub by: i64;
    pub fn nudge(self, value: &mut i64) { value = value + self.by; }
    pub fn peek(self, value: & i64) -> i64 { return value + self.by; }
    pub fn weigh(self, value: i64) -> i64 { return value * self.by; }
    pub fn rename(self, label: &mut String) { label = label + "!"; }
    pub fn spread(self, lo: &mut i64, hi: &mut i64) { lo = lo - self.by; hi = hi + self.by; }
}

class Jump implements Scale {
    pub by: i64;
    pub fn nudge(self, value: &mut i64) { value = value * self.by; }
    pub fn peek(self, value: & i64) -> i64 { return value * self.by; }
    pub fn weigh(self, value: i64) -> i64 { return value + self.by; }
    pub fn rename(self, label: &mut String) { label = "<" + label + ">"; }
    pub fn spread(self, lo: &mut i64, hi: &mut i64) { lo = lo * self.by; hi = hi * self.by; }
    pub fn double(self, value: &mut i64) { value = value * self.by * self.by; }
}

class Cell { pub n: i64; }
"##;

fn refmode_source(body: &str) -> String {
    format!("{REFMODE_PRELUDE}{body}")
}

/// A reference ARGUMENT used to leave the LIR subset, so these programs were
/// the mixed-path case; the walker takes them whole now (willow-0g8j.2.13), and
/// what is pinned is the answer.
fn assert_refmode_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

/// [`assert_output_under_gc_stress`] for a reference-argument program (see
/// [`assert_refmode_output`]).
fn assert_refmode_output_under_gc_stress(source: &str, expected: &str) {
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (out, ok) = compile_with_env_and_run_under(source, &PLAIN, &stress);
    assert!(ok, "GC-stress run failed: {out}");
    assert_eq!(out, expected);
}

// ── Every body walks the LIR (willow-0g8j.3) ────────────────────────────────
// Stage 5 retired the AST emitter for function bodies. There is no fallback
// left to fall into and no environment variable that restores one: a function
// the walker cannot take is a COMPILE ERROR that names the function and the
// construct that blocked it. That is what makes the differential-turned-direct
// tests above meaningful — a lowering or eligibility regression can no longer
// hide behind a second emitter quietly producing the same answer.
//
// Scope: this polices `compile_function_named` (sync and async free functions
// and `main`). Class methods go through `compile_class_method`, which has the
// same hard requirement; `example/lir_gc_objects.wi` below covers that side.
//
// Perspectives 39-51:
// 39 an all-eligible program compiles AND runs, 40 an ineligible function
// fails the build, 41 the diagnostic names the offending function, 42 the
// diagnostic gives the eligibility reason, 43 only the ineligible function is
// named when eligible ones sit beside it, 44 `main(args)` is inside the
// subset, 45 no environment setting brings the fallback back, 46 the failure
// is reported once, at the first blocked function, 47 a method body outside
// the subset fails the same way, 48 a non-suspending async body walks the LIR,
// 48b so does one that awaits, 49-51 the shipped examples have every function
// on the LIR path.

/// A function the walker cannot compile, plus an eligible one, so a test can
/// check exactly which name is reported.
///
/// Every earlier stand-in here kept getting promoted into the subset — plain
/// class field access (willow-0g8j.5), class-to-interface widening
/// (willow-j260), dispatch through an interface box (willow-0g8j.6), then
/// `Option`/`Result` themselves (willow-0g8j.2.1), and lowering now alpha-renames
/// shadowed bindings (willow-0g8j.2.10), then reference parameters
/// (willow-0g8j.2.7), then scalar map keys (willow-0g8j.3). What is left is a
/// map keyed by a REFERENCE that is not a `String`: the runtime's key is
/// `Int(i64) | Str(String)`, so it would read the inner map's pointer as a
/// `WillowString`, and no widening of the walker can change that.
const LIR_MIXED_SOURCE: &str = r#"
import std::collections::Map;

fn eligible(a: i64) -> i64 { return a + 1; }

fn unsupported() -> Map<Map<String, i64>, i64> { return Map::new(); }

fn main() { println(eligible(1)); }
"#;

// ── Maps and frozen collections on the LIR path (willow-0g8j.7) ─────────────
// The emitted-code half of willow-0g8j.7; the eligibility half is the c01-c24
// perspectives in `src/backend/cranelift/lir_gen.rs`. Every program below runs
// on both paths and must print the same thing, and c42-c46 repeat that under
// `WILLOW_GC_STRESS=alloc` — the mode that collects at every allocation, so a
// receiver or value left unrooted across an allocating call is reclaimed and
// the output changes.
//
// c25 an empty map filled by `insert`, c26 `contains` both ways, c27
// `toString` (sorted by key, so the text is deterministic), c28 an i64-keyed
// map with String values, c29 a map as a parameter, c30 a map as a RETURN
// value, c31 `freeze` into a `FrozenMap` read through `len`/`contains`,
// c32 an array frozen and then indexed, c33 a `FrozenArray` parameter summed
// in a loop, c34 a map whose values are arrays, c35 a map of maps, c36 inserts
// inside a loop, c37 a map built and frozen with only the frozen copy kept,
// c38 the receiver rooted across an allocating VALUE expression, c39 the same
// across an allocating KEY expression, c40 a map local live across a later
// allocation, c41 map/array/String locals live at once; then under GC stress:
// c42 the insert loop, c43 freeze, c44 arrays as map values, c45 a map handed
// between frames, c46 a frozen array indexed in a loop.

const MAP_IMPORTS: &str = "import std::collections::Map;\nimport std::collections::Array;\n";

/// A collection program with both collection imports prepended.
fn map_source(body: &str) -> String {
    format!("{MAP_IMPORTS}{body}")
}

// ── Debug-build integer division guards (willow-l9lx) ───────────────────────
// `/` and `%` used to die with a raw hardware signal; debug builds now emit a
// located runtime panic, consistent with the array bounds panics. 20
// perspectives: 1 div-by-zero message+location (LIR path), 2 same on the AST
// path, 3 rem-by-zero, 4 MIN/-1 overflow, 5 MIN%-1 overflow, 6 non-zero exit,
// 7 call-stack frame present, 8-9 normal div/rem unaffected on both paths,
// 10 runtime-value divisor, 11 guard inside a loop, 12 guard in a class
// method, 13 guard in an async fn, 14 f64 division by zero is NOT trapped,
// 15 constant operands still guarded, 16 computed-zero divisor, 17 rem in a
// LIR loop, 18 message names the source file, 19 zero mid-chain, 20 negative
// dividend unaffected.

fn div_panic_output(source: &str) -> String {
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "expected a runtime panic, got success: {out}");
    out
}

fn assert_dfr_runs(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

fn assert_dfr_runs_gc_stress(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

fn assert_dfr_compile_fails(source: &str) {
    let (ok, stderr) = compile_with_compiler_env(source, &[]);
    assert!(!ok, "expected compile failure, stderr:\n{stderr}");
}

fn assert_dfr_compile_error(source: &str, needle: &str) {
    let (ok, stderr) = compile_with_compiler_env(source, &[]);
    assert!(!ok, "expected compile failure, stderr:\n{stderr}");
    assert!(stderr.contains(needle), "{stderr}");
}

fn assert_dfr_exit_contains(source: &str, needles: &[&str]) {
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "{out}");
    for needle in needles {
        assert!(out.contains(needle), "{out}");
    }
}

// Explicit Result handling inside defer bodies (willow-oorh).
//
// 22 perspectives:
// 01 plain-call Err is discarded without panic/propagation
// 02 plain-call Ok is discarded
// 03 cleanup Err does not replace a parent Result return
// 04 deferred match handles Err after the body
// 05 deferred match selects Ok
// 06 deferred block can bind and match a Result
// 07 the value of the whole deferred match is discarded
// 08 call/match/block entries retain one shared LIFO order
// 09 direct-call argument side effects still happen at registration
// 10 match scrutinee evaluation is delayed until scope exit
// 11 `defer return ...` is E0905
// 12 return nested in a deferred block is E0905
// 13 return in a deferred match arm is E0905
// 14 `?` in a deferred block is E0905
// 15 `?` in a deferred match scrutinee is E0905
// 16 break in a deferred body is E0905 even inside an outer loop
// 17 continue in a deferred body is E0905 even inside an outer loop
// 18 await/select-style suspension in a deferred body is E0905
// 19 an async cleanup call nested in a body is E0905
// 20 async normal-exit and cancellation paths execute a registered body once
// 21 async cancellation can read a lexical value from its task frame
// 22 `?` in a direct-call receiver/argument is also E0905

const DEFER_RESULT_HELPER: &str = r#"
fn cleanup(ok: bool) -> Result<void, String> {
    if ok { return Ok(); }
    return Err("cleanup failed");
}
"#;

// ── std::net v1 (willow-2s3.1) ───────────────────────────────────────
// Numeric-address bind is synchronous; connect/accept/read/write return Tasks
// whose poll functions park on the platform netpoll backend.

const NET_ECHO_SOURCE: &str = r#"
import std::net;

async fn exchange() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, "ping"))?;
    let server = (await accepting)?;
    let text = (await net::read_async(server, 1024))?;
    net::shutdown(client)?;
    net::shutdown(server)?;
    return Ok(text);
}

async fn main() {
    match await exchange() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#;

#[path = "codegen/arguments.rs"]
mod arguments;
#[path = "codegen/array_growth.rs"]
mod array_growth;
#[path = "codegen/array_loop_access.rs"]
mod array_loop_access;
#[path = "codegen/array_loops.rs"]
mod array_loops;
#[path = "codegen/arrays.rs"]
mod arrays;
#[path = "codegen/async_gc.rs"]
mod async_gc;
#[path = "codegen/awaiter_suspend.rs"]
mod awaiter_suspend;
#[path = "codegen/channels.rs"]
mod channels;
#[path = "codegen/closures.rs"]
mod closures;
#[path = "codegen/collection_display.rs"]
mod collection_display;
#[path = "codegen/collection_inference.rs"]
mod collection_inference;
#[path = "codegen/constructors.rs"]
mod constructors;
#[path = "codegen/defer.rs"]
mod defer;
#[path = "codegen/divergence.rs"]
mod divergence;
#[path = "codegen/expression_roots.rs"]
mod expression_roots;
#[path = "codegen/filesystem.rs"]
mod filesystem;
#[path = "codegen/formatting.rs"]
mod formatting;
#[path = "codegen/function_values.rs"]
mod function_values;
#[path = "codegen/integer_division.rs"]
mod integer_division;
#[path = "codegen/interface_boxing.rs"]
mod interface_boxing;
#[path = "codegen/interface_dispatch.rs"]
mod interface_dispatch;
#[path = "codegen/interface_references.rs"]
mod interface_references;
#[path = "codegen/lambda_inference.rs"]
mod lambda_inference;
#[path = "codegen/lir_arrays.rs"]
mod lir_arrays;
#[path = "codegen/lir_classes.rs"]
mod lir_classes;
#[path = "codegen/lir_collections.rs"]
mod lir_collections;
#[path = "codegen/lir_coverage.rs"]
mod lir_coverage;
#[path = "codegen/lir_enums.rs"]
mod lir_enums;
#[path = "codegen/lir_execution.rs"]
mod lir_execution;
#[path = "codegen/lir_roots.rs"]
mod lir_roots;
#[path = "codegen/loop_control.rs"]
mod loop_control;
#[path = "codegen/maps.rs"]
mod maps;
#[path = "codegen/match_returns.rs"]
mod match_returns;
#[path = "codegen/modules.rs"]
mod modules;
#[path = "codegen/multiple_interfaces.rs"]
mod multiple_interfaces;
#[path = "codegen/nested_assignment.rs"]
mod nested_assignment;
#[path = "codegen/network.rs"]
mod network;
#[path = "codegen/option_construction.rs"]
mod option_construction;
#[path = "codegen/option_result.rs"]
mod option_result;
#[path = "codegen/range_loops.rs"]
mod range_loops;
#[path = "codegen/range_values.rs"]
mod range_values;
#[path = "codegen/root_lifetimes.rs"]
mod root_lifetimes;
#[path = "codegen/select.rs"]
mod select;
#[path = "codegen/std_imports.rs"]
mod std_imports;
#[path = "codegen/string_equality.rs"]
mod string_equality;
#[path = "codegen/string_gc.rs"]
mod string_gc;
#[path = "codegen/task_await.rs"]
mod task_await;
#[path = "codegen/temporary_roots.rs"]
mod temporary_roots;
#[path = "codegen/trap_contracts.rs"]
mod trap_contracts;

//! Caller impact through function values and generic instantiations
//! (willow-25da.29.57): a closure reaches the calls of its type key inside its
//! own instantiation, never the callers of unrelated instantiations.
use std::{collections::BTreeSet, fs, path::PathBuf};
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{Direction, Impact, Limits, Snapshot},
    diagnostics::HumanEmitter,
};

struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-closure-impact-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("project.toml"),
            "[willow]\nmanifest-version = 1\n[project]\nname = \"p\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n",
        )
        .unwrap();
        for (path, source) in files {
            fs::write(root.join("src").join(path), source).unwrap();
        }
        Self(fs::canonicalize(root).unwrap())
    }

    fn snapshot(&self) -> Snapshot {
        let entry = self.0.join("src/main.wi");
        CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &CompilerOptions::debug(),
            Some(self.0.clone()),
        )
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `module::name` of a snapshot function (module = source file stem).
fn label(snapshot: &Snapshot, id: &str) -> String {
    let f = snapshot.functions.iter().find(|f| f.id == id).unwrap();
    let module = std::path::Path::new(&f.module)
        .file_stem()
        .map_or(String::new(), |s| s.to_string_lossy().into_owned());
    format!("{module}::{}", f.name)
}

fn id(snapshot: &Snapshot, name: &str) -> String {
    let matches: Vec<_> = snapshot
        .functions
        .iter()
        .filter(|f| label(snapshot, &f.id) == name)
        .collect();
    assert_eq!(matches.len(), 1, "{name}");
    matches[0].id.clone()
}

fn impact_with(snapshot: &Snapshot, seed: &str, direction: Direction, limits: Limits) -> Impact {
    snapshot
        .impact(&[id(snapshot, seed)], direction, limits, None)
        .unwrap()
}

fn callers(snapshot: &Snapshot, seed: &str) -> (BTreeSet<String>, bool) {
    let impact = impact_with(snapshot, seed, Direction::Callers, Limits::default());
    let names = impact
        .nodes
        .iter()
        .map(|n| label(snapshot, &n.id))
        .collect();
    (names, impact.unknown)
}

const PQ: &str = "module pq;
pub class Heap<T> {
    less: closure(T, T) -> bool;
    pub init(self, less: closure(T, T) -> bool) { self.less = less; }
    pub fn push(self, x: T) -> bool { return self.check(x); }
    fn check(self, x: T) -> bool { let less = self.less; return less(x, x); }
}
pub fn merge_sort<T>(x: T, less: closure(T, T) -> bool) -> bool { return less(x, x); }
";
const DATA: &str = "module data;
pub class Patient { pub id: i64; }
pub class Item { pub key: i64; }
";
const POLICY: &str = "module policy;
import data::Patient;
pub fn before(a: Patient, b: Patient) -> bool { return a.id < b.id; }
";
const SIM: &str = "module sim;
import pq::Heap;
import data::Patient;
import policy::before;
pub fn run(bias: i64) -> bool {
    let less = |a: Patient, b: Patient| before(a, b) && bias >= 0;
    let h = new Heap<Patient>(less);
    return h.push(new Patient(1));
}
";
const FUZZ: &str = "module fuzz;
import pq::{Heap, merge_sort};
import data::Item;
pub fn run(k: i64) -> bool {
    let h = new Heap<Item>(|a: Item, b: Item| a.key < b.key + k);
    return h.push(new Item(1)) && merge_sort(new Item(2), |a: Item, b: Item| a.key == b.key + k);
}
pub fn ints(k: i64) -> bool { return merge_sort(3, |a: i64, b: i64| a < b + k); }
";
const MAIN: &str = "import sim;
import fuzz;
fn main() {
    println(sim::run(0).toString());
    println(fuzz::run(0).toString());
    println(fuzz::ints(0).toString());
}
";

fn triage() -> Snapshot {
    Project::new(&[
        ("pq.wi", PQ),
        ("data.wi", DATA),
        ("policy.wi", POLICY),
        ("sim.wi", SIM),
        ("fuzz.wi", FUZZ),
        ("main.wi", MAIN),
    ])
    .snapshot()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn closure_impact_instantiations_and_keys_exclude_unrelated_callers() {
    // P1 closure field of one instantiation, P2 generic function with an
    // unrelated lambda, P3 primitive-keyed lambda, P4 cross-module imports,
    // P5 complete coverage (no global unknown).
    let snapshot = triage();
    let (names, unknown) = callers(&snapshot, "policy::before");
    let lambda = names
        .iter()
        .find(|n| n.starts_with("sim::<lambda"))
        .cloned();
    assert!(lambda.is_some(), "{names:?}");
    let mut expected = set(&[
        "policy::before",
        "pq::Heap::check",
        "pq::Heap::push",
        "sim::run",
        "main::main",
    ]);
    expected.insert(lambda.unwrap());
    assert_eq!(names, expected);
    assert!(!unknown);
}

#[test]
fn closure_impact_unrelated_instantiation_reaches_only_its_own_callers() {
    // P6 the other instantiation's lambda is reached only through Heap<Item>.
    let snapshot = triage();
    let item_lambda = snapshot
        .functions
        .iter()
        .find(|f| f.name.starts_with("<lambda run@") && f.module.ends_with("fuzz.wi"))
        .map(|f| label(&snapshot, &f.id))
        .unwrap();
    let (names, unknown) = callers(&snapshot, &item_lambda);
    assert!(names.contains("fuzz::run"), "{names:?}");
    assert!(!names.contains("sim::run"), "{names:?}");
    assert!(!unknown);
}

#[test]
fn closure_impact_callees_direction_keeps_source_graph() {
    // P7 callee impact is still the aggregate source graph.
    let snapshot = triage();
    let impact = impact_with(&snapshot, "sim::run", Direction::Callees, Limits::default());
    let names: BTreeSet<_> = impact
        .nodes
        .iter()
        .map(|n| label(&snapshot, &n.id))
        .collect();
    assert!(names.contains("pq::Heap::push"), "{names:?}");
    assert!(names.contains("policy::before"), "{names:?}");
}

#[test]
fn closure_impact_distances_skip_instance_unions() {
    // P8 instance unions add no hop: on the shared chain before <- lambda <-
    // bucket <- check <- push <- run <- main, distances equal those of the
    // aggregate traversal.
    let snapshot = triage();
    let mut aggregate = snapshot.clone();
    aggregate.semantic = serde_json::from_value({
        let mut semantic = serde_json::to_value(&snapshot.semantic).unwrap();
        semantic.as_object_mut().unwrap().remove("instances");
        semantic
    })
    .unwrap();
    let distances = |snapshot: &Snapshot| {
        impact_with(
            snapshot,
            "policy::before",
            Direction::Callers,
            Limits::default(),
        )
        .nodes
        .into_iter()
        .map(|n| (label(snapshot, &n.id), (n.distance, n.graph_distance)))
        .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (instance, aggregate) = (distances(&snapshot), distances(&aggregate));
    for name in [
        "pq::Heap::check",
        "pq::Heap::push",
        "sim::run",
        "main::main",
    ] {
        assert_eq!(instance[name], aggregate[name], "{name}");
    }
    assert_eq!(instance["pq::Heap::check"], (2, 3));
    assert_eq!(instance["main::main"], (5, 6));
}

#[test]
fn closure_impact_limits_count_functions() {
    // P9 max_nodes counts functions, not instance nodes; P10 max_depth.
    let snapshot = triage();
    let limited = impact_with(
        &snapshot,
        "policy::before",
        Direction::Callers,
        Limits {
            max_nodes: 3,
            max_depth: 32,
        },
    );
    // The seed, its lambda and the synthetic bucket fill the budget.
    assert_eq!(limited.nodes.len(), 2);
    assert!(limited.truncated);
    let shallow = impact_with(
        &snapshot,
        "policy::before",
        Direction::Callers,
        Limits {
            max_nodes: 100,
            max_depth: 1,
        },
    );
    assert!(shallow.truncated);
    assert!(shallow.nodes.iter().all(|n| n.distance <= 1));
}

#[test]
fn closure_impact_instances_survive_serialization_and_old_snapshots_fall_back() {
    // P11 roundtrip keeps the instance graph; P12 a snapshot without it
    // (older format) still loads and stays sound (aggregate graph).
    let snapshot = triage();
    let json = serde_json::to_value(&snapshot).unwrap();
    assert!(json["semantic"]["instances"].is_object());
    let restored: Snapshot = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(
        callers(&restored, "policy::before"),
        callers(&snapshot, "policy::before")
    );
    let mut old = json;
    old["semantic"].as_object_mut().unwrap().remove("instances");
    let old: Snapshot = serde_json::from_value(old).unwrap();
    let (names, _) = callers(&old, "policy::before");
    assert!(names.contains("fuzz::run"), "aggregate fallback: {names:?}");
    assert!(names.is_superset(&callers(&snapshot, "policy::before").0));
}

fn single(main: &str) -> Snapshot {
    Project::new(&[("main.wi", main)]).snapshot()
}

#[test]
fn closure_impact_named_function_values_are_keyed_by_signature() {
    // P13 named function values; P14 a different signature at the same arity.
    let snapshot = single(
        "fn inc(x: i64) -> i64 { return x + 1; }
fn half(x: f64) -> f64 { return x / 2.0; }
fn apply_i(f: fn(i64) -> i64) -> i64 { return f(1); }
fn apply_f(f: fn(f64) -> f64) -> f64 { return f(1.0); }
fn use_i() -> i64 { return apply_i(inc); }
fn use_f() -> f64 { return apply_f(half); }
fn main() { use_i(); use_f(); }",
    );
    let (names, unknown) = callers(&snapshot, "main::inc");
    assert!(names.contains("main::apply_i"), "{names:?}");
    assert!(!names.contains("main::apply_f"), "{names:?}");
    assert!(!names.contains("main::use_f"), "{names:?}");
    assert!(!unknown);
}

#[test]
fn closure_impact_fn_and_closure_types_share_a_key() {
    // P15 `fn` and `closure` of one signature stay conservatively merged.
    let snapshot = single(
        "fn inc(x: i64) -> i64 { return x + 1; }
fn apply_fn(f: fn(i64) -> i64) -> i64 { return f(1); }
fn apply_closure(f: closure(i64) -> i64) -> i64 { return f(1); }
fn main() { let k = 1; apply_fn(inc); apply_closure(|x: i64| x + k); }",
    );
    let (names, _) = callers(&snapshot, "main::inc");
    assert!(names.contains("main::apply_fn"), "{names:?}");
    assert!(names.contains("main::apply_closure"), "{names:?}");
}

#[test]
fn closure_impact_same_key_values_stay_conservative() {
    // P16 two lambdas of one type: a call cannot tell them apart.
    let snapshot = single(
        "fn target(x: i64) -> bool { return x > 0; }
fn first(f: closure(i64) -> bool) -> bool { return f(1); }
fn second(f: closure(i64) -> bool) -> bool { return f(2); }
fn main() { let k = 1; first(|x: i64| target(x + k)); second(|x: i64| x < k); }",
    );
    let (names, _) = callers(&snapshot, "main::target");
    assert!(names.contains("main::first"), "{names:?}");
    assert!(names.contains("main::second"), "{names:?}");
}

#[test]
fn closure_impact_opaque_value_keeps_coverage_note() {
    // P17 a value without a Willow body keeps the unknown coverage note.
    let snapshot = single(
        "fn add(a: f64, b: f64) -> f64 { return a + b; }
fn apply(f: fn(f64, f64) -> f64) -> f64 { return f(1.0, 2.0); }
fn main() { apply(pow); apply(add); }",
    );
    let (names, unknown) = callers(&snapshot, "main::add");
    assert!(names.contains("main::apply"), "{names:?}");
    assert!(unknown);
}

#[test]
fn closure_impact_unkeyed_value_reaches_every_typed_call() {
    // P18 a value whose type has no key (builtin container) is a wildcard.
    let snapshot = single(
        "import std::collections::Map;
fn target() -> i64 { return 1; }
fn count(m: Map<String, i64>) -> i64 { return target() + m.len(); }
fn apply_map(f: fn(Map<String, i64>) -> i64) -> i64 { return f(Map::new()); }
fn apply_int(f: closure(i64) -> i64) -> i64 { return f(1); }
fn main() { let k = 1; apply_map(count); apply_int(|x: i64| x + k); }",
    );
    let (names, _) = callers(&snapshot, "main::target");
    assert!(names.contains("main::apply_map"), "{names:?}");
    assert!(names.contains("main::apply_int"), "{names:?}");
}

#[test]
fn closure_impact_class_arguments_merge_and_arrays_split_keys() {
    // P19 class type arguments are not part of a function key (an instance
    // name `Wrap$mono$..` carries none to compare), so they stay merged;
    // P20 array element types still split keys.
    let snapshot = single(
        "import std::collections::Array;
class Wrap<T> { pub v: T; }
class A { pub n: i64; }
class B { pub n: i64; }
fn target(x: Wrap<A>) -> bool { return x.v.n > 0; }
fn on_b(x: Wrap<B>) -> bool { return x.v.n > 0; }
fn on_arr(xs: Array<A>) -> bool { return xs.len() > 0; }
fn apply_a(f: fn(Wrap<A>) -> bool) -> bool { return f(new Wrap<A>(new A(1))); }
fn apply_b(f: fn(Wrap<B>) -> bool) -> bool { return f(new Wrap<B>(new B(1))); }
fn apply_arr(f: fn(Array<A>) -> bool) -> bool { let xs: Array<A> = [new A(1)]; return f(xs); }
fn main() { apply_a(target); apply_b(on_b); apply_arr(on_arr); }",
    );
    let (names, unknown) = callers(&snapshot, "main::target");
    assert!(names.contains("main::apply_a"), "{names:?}");
    assert!(names.contains("main::apply_b"), "{names:?}");
    assert!(!names.contains("main::apply_arr"), "{names:?}");
    assert!(!unknown);
}

const QUEUE: &str = "module queue;
pub interface Queue<T> { fn push(self, x: T) -> bool; }
pub class Fast<T> implements Queue<T> {
    less: closure(T, T) -> bool;
    pub init(self, less: closure(T, T) -> bool) { self.less = less; }
    pub fn push(self, x: T) -> bool { let less = self.less; return less(x, x); }
}
pub fn feed<T>(q: Queue<T>, x: T) -> bool { return q.push(x); }
";

#[test]
fn closure_impact_generic_interface_dispatch_is_per_instantiation() {
    // P21 interface receiver `Queue<A>` never reaches `Fast<B>::push`;
    // P22 a generic helper over the interface is split per instantiation.
    let snapshot = Project::new(&[
        ("queue.wi", QUEUE),
        (
            "main.wi",
            "import queue::{Queue, Fast, feed};
class A { pub n: i64; }
class B { pub n: i64; }
fn target(x: A) -> bool { return x.n > 0; }
fn use_a(k: i64) -> bool { let q: Queue<A> = new Fast<A>(|x: A, y: A| target(x) && k == 0); return q.push(new A(1)); }
fn use_b(k: i64) -> bool { let q: Queue<B> = new Fast<B>(|x: B, y: B| x.n > k); return feed(q, new B(1)); }
fn main() { use_a(0); use_b(0); }",
        ),
    ])
    .snapshot();
    let (names, unknown) = callers(&snapshot, "main::target");
    assert!(names.contains("main::use_a"), "{names:?}");
    assert!(names.contains("queue::Fast::push"), "{names:?}");
    assert!(!names.contains("main::use_b"), "{names:?}");
    assert!(!names.contains("queue::feed"), "{names:?}");
    assert!(!unknown);
}

#[test]
fn closure_impact_mixed_interface_receivers_union_their_instantiations() {
    // P23 a caller using both instantiations reaches both.
    let snapshot = Project::new(&[
        ("queue.wi", QUEUE),
        (
            "main.wi",
            "import queue::{Queue, Fast};
class A { pub n: i64; }
class B { pub n: i64; }
fn target(x: B) -> bool { return x.n > 0; }
fn both(k: i64) -> bool {
    let a: Queue<A> = new Fast<A>(|x: A, y: A| x.n > k);
    let b: Queue<B> = new Fast<B>(|x: B, y: B| target(x) && k == 0);
    return a.push(new A(1)) && b.push(new B(1));
}
fn main() { both(0); }",
        ),
    ])
    .snapshot();
    let (names, _) = callers(&snapshot, "main::target");
    assert!(names.contains("main::both"), "{names:?}");
}

#[test]
fn closure_impact_non_generic_interface_keeps_its_union() {
    // P24 a non-generic interface is not split: every implementation stays.
    let snapshot = single(
        "interface Speaker { fn speak(self) -> i64; }
class Dog implements Speaker { pub fn speak(self) -> i64 { return 1; } }
class Cat implements Speaker { pub fn speak(self) -> i64 { return 2; } }
fn talk(s: Speaker) -> i64 { return s.speak(); }
fn main() { talk(new Dog()); talk(new Cat()); }",
    );
    let (names, _) = callers(&snapshot, "main::Cat::speak");
    assert!(names.contains("main::talk"), "{names:?}");
}

#[test]
fn closure_impact_uninstantiated_template_does_not_mark_coverage_unknown() {
    // P25 a never-instantiated generic template cannot run: no global note.
    let snapshot = single(
        "fn target() -> i64 { return 1; }
fn unused<T>(x: T, f: closure(T) -> i64) -> i64 { return f(x); }
fn main() { target(); }",
    );
    let (names, unknown) = callers(&snapshot, "main::target");
    assert_eq!(names, set(&["main::target", "main::main"]));
    assert!(!unknown);
}

#[test]
fn closure_impact_instance_graph_scales_linearly() {
    // P26 n instantiations, each with its own lambda: instance nodes and
    // edges stay linear and each caller impact visits a constant number of
    // edges independent of n.
    let mut visits = vec![];
    for n in [4, 16, 64] {
        let mut main = String::from(
            "class Heap<T> {
    less: closure(T, T) -> bool;
    pub init(self, less: closure(T, T) -> bool) { self.less = less; }
    pub fn push(self, x: T) -> bool { let less = self.less; return less(x, x); }
}
",
        );
        for i in 0..n {
            main.push_str(&format!(
                "class C{i} {{ pub v: i64; }}
fn t{i}(a: C{i}) -> bool {{ return a.v > 0; }}
fn use{i}(k: i64) -> bool {{ let h = new Heap<C{i}>(|a: C{i}, b: C{i}| t{i}(a) && k == 0); return h.push(new C{i}(1)); }}
"
            ));
        }
        main.push_str("fn main() {");
        for i in 0..n {
            main.push_str(&format!("use{i}(0);"));
        }
        main.push('}');
        let snapshot = single(&main);
        let json = serde_json::to_value(&snapshot).unwrap();
        let instances = &json["semantic"]["instances"];
        let nodes = instances["owners"].as_array().unwrap().len();
        let edges: usize = instances["callees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_array().unwrap().len())
            .sum();
        assert!(nodes <= 8 * n + 16, "n={n} nodes={nodes}");
        assert!(edges <= 8 * n + 16, "n={n} edges={edges}");
        let impact = impact_with(
            &snapshot,
            &format!("main::t{}", n - 1),
            Direction::Callers,
            Limits::default(),
        );
        assert!(!impact.unknown);
        let names: BTreeSet<_> = impact
            .nodes
            .iter()
            .map(|n| label(&snapshot, &n.id))
            .collect();
        assert!(names.contains(&format!("main::use{}", n - 1)));
        assert!(n == 1 || !names.contains("main::use0"), "{names:?}");
        println!(
            "instances n={n} nodes={nodes} edges={edges} impact_edge_visits={}",
            impact.edge_visits
        );
        visits.push(impact.edge_visits);
    }
    // main fans out to every use; nothing else grows with n.
    assert!(visits[2] - visits[1] <= 64, "{visits:?}");
}

/// P27: the runnable example (`example/closure_impact`) keeps fuzz-only
/// closures and the `Queue<Item>` caller out of `policy::before`'s impact.
#[test]
fn closure_impact_runnable_example() {
    let project = Project::new(&[
        (
            "data.wi",
            include_str!("../example/closure_impact/src/data.wi"),
        ),
        (
            "policy.wi",
            include_str!("../example/closure_impact/src/policy.wi"),
        ),
        ("pq.wi", include_str!("../example/closure_impact/src/pq.wi")),
        (
            "sim.wi",
            include_str!("../example/closure_impact/src/sim.wi"),
        ),
        (
            "fuzz.wi",
            include_str!("../example/closure_impact/src/fuzz.wi"),
        ),
        (
            "main.wi",
            include_str!("../example/closure_impact/src/main.wi"),
        ),
    ]);
    let snapshot = project.snapshot();
    let (names, unknown) = callers(&snapshot, "policy::before");
    assert!(!unknown, "{names:?}");
    for expected in [
        "pq::Heap::check",
        "pq::Heap::offer",
        "sim::admit",
        "sim::run",
        "main::main",
    ] {
        assert!(names.contains(expected), "missing {expected}: {names:?}");
    }
    assert!(!names.iter().any(|n| n.starts_with("fuzz::")), "{names:?}");
    let (names, _) = callers(&snapshot, "pq::Heap::check");
    assert!(
        names.contains("fuzz::feed") && names.contains("fuzz::run"),
        "{names:?}"
    );
}

const SIZED: &str = "pub interface Sized<T> { fn size(self) -> i64; }";

#[test]
fn closure_impact_class_implementing_several_instantiations_serves_each() {
    // P28 `Two implements Sized<A>, Sized<B>`: a `Sized<B>` caller reaches it,
    // while `One` (only `Sized<A>`) stays out of the `Sized<B>` caller.
    let snapshot = Project::new(&[
        ("sized.wi", SIZED),
        (
            "main.wi",
            "import sized::Sized;
class A { pub n: i64; }
class B { pub n: i64; }
fn target() -> i64 { return 7; }
fn other() -> i64 { return 8; }
class Two implements Sized<A>, Sized<B> { pub fn size(self) -> i64 { return target(); } }
class One implements Sized<A> { pub fn size(self) -> i64 { return other(); } }
fn use_a(s: Sized<A>) -> i64 { return s.size(); }
fn use_b(s: Sized<B>) -> i64 { return s.size(); }
fn main() { println((use_a(new One()) + use_a(new Two()) + use_b(new Two())).toString()); }",
        ),
    ])
    .snapshot();
    let (names, unknown) = callers(&snapshot, "main::target");
    assert!(names.contains("main::use_a"), "{names:?}");
    assert!(names.contains("main::use_b"), "{names:?}");
    assert!(!unknown);
    let (names, _) = callers(&snapshot, "main::other");
    assert!(names.contains("main::use_a"), "{names:?}");
    assert!(!names.contains("main::use_b"), "{names:?}");
}

#[test]
fn closure_impact_inherited_instantiation_reaches_overrides() {
    // P29 `Sub` inherits `Sized<A>` from `Base`: its override is reached from a
    // `Sized<A>` caller, and not from a `Sized<B>` caller.
    let snapshot = Project::new(&[
        ("sized.wi", SIZED),
        (
            "main.wi",
            "import sized::Sized;
class A { pub n: i64; }
class B { pub n: i64; }
fn target() -> i64 { return 7; }
open class Base implements Sized<A> { pub open fn size(self) -> i64 { return 1; } }
class Sub extends Base { pub override fn size(self) -> i64 { return target(); } }
class Other implements Sized<B> { pub fn size(self) -> i64 { return 2; } }
fn use_a(s: Sized<A>) -> i64 { return s.size(); }
fn use_b(s: Sized<B>) -> i64 { return s.size(); }
fn main() { println((use_a(new Sub()) + use_b(new Other())).toString()); }",
        ),
    ])
    .snapshot();
    let (names, unknown) = callers(&snapshot, "main::target");
    assert!(names.contains("main::use_a"), "{names:?}");
    assert!(!names.contains("main::use_b"), "{names:?}");
    assert!(!unknown);
}

#[test]
fn closure_impact_self_receiver_in_default_body_keeps_the_union() {
    // P30 A dependency's generic interface default body runs as a
    // monomorphized copy: its `self.m()` names the instance's method, which
    // carries no dispatch targets, and the interface's dispatch names only
    // the template body. Both must still connect `X::m` to `use_d`, while the
    // keyed `other.m()` reroutes.
    let snapshot = Project::new(&[
        (
            "pair.wi",
            "pub class B { pub n: i64; }
pub interface Pair<T> {
    fn m(self) -> i64;
    fn d(self, other: Pair<B>) -> i64 { return self.m() + other.m(); }
}",
        ),
        (
            "main.wi",
            "import pair::Pair;
import pair::B;
class A { pub n: i64; }
fn target() -> i64 { return 7; }
fn other() -> i64 { return 2; }
class X implements Pair<A> { pub fn m(self) -> i64 { return target(); } }
class Y implements Pair<B> { pub fn m(self) -> i64 { return other(); } }
fn use_d(p: Pair<A>, q: Pair<B>) -> i64 { return p.d(q); }
fn main() { println(use_d(new X(), new Y()).toString()); }",
        ),
    ])
    .snapshot();
    let (names, _) = callers(&snapshot, "main::target");
    assert!(names.contains("pair::Pair::$default$d"), "{names:?}");
    assert!(names.contains("main::use_d"), "{names:?}");
    let (names, _) = callers(&snapshot, "main::other");
    assert!(names.contains("main::use_d"), "{names:?}");
}

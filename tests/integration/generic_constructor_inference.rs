//! `new C(args)` infers a generic class's type arguments from its constructor
//! arguments, across modules and at runtime.
use super::support::*;

const SHAPES: &str = "import std::collections::Array;
pub open class Base { pub id: i64; }
pub class Tagged<T> extends Base { pub tag: T; }
pub class Bag<T> { pub items: Array<T>; pub fn size(self) -> i64 { return self.items.len(); } }
pub class Kv<K, V> { pub k: K; pub v: V; pub init(self, k: K, v: V) { self.k = k; self.v = v; } }
pub class Item { pub n: i64; }
pub fn item() -> Item { return new Item(9); }
pub class Holder<T> { pub item: Item; pub v: T; }
";

#[test]
fn inferred_generic_constructors_run() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;
class Heap<T> { pub items: Array<T>; pub before: fn(T, T) -> bool; }
class Box<T> { pub v: T; }
class Pair<A, B> { pub a: A; pub b: B; pub init(self, x: A, y: B) { self.a = x; self.b = y; } }
fn wrap<T>(v: T) -> Box<T> { return new Box(v); }
fn main() {
    let h = new Heap([3, 1], |a, b| a < b);
    let f = h.before;
    println(h.items.len());
    println(f(1, 2));
    let p = new Pair(\"s\", 2.5);
    println(p.a);
    let e = new Heap([], |a: String, b: String| -> bool { return a < b; });
    println(e.items.len());
    println(new Box(new Box(7)).v.v);
    println(wrap(\"w\").v);
}
",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\ntrue\ns\n0\n7\nw\n");
}

#[test]
fn inferred_generic_constructors_from_imported_templates_run() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("shapes.wi", SHAPES),
            (
                "main.wi",
                "import shapes;
import shapes::{Bag, Tagged, Holder, item};
fn main() {
    println(new Bag([1, 2, 3]).size());
    println(new shapes::Kv(\"k\", 7).v);
    let t = new Tagged(1, \"t\");
    println(t.id);
    println(t.tag);
    println(new Holder(item(), 2.5).item.n);
    println(new Holder<i64>(item(), 1).v);
    println(new Tagged<String>(4, \"u\").id);
}
",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n7\n1\nt\n9\n1\n4\n");
}

#[test]
fn imported_generic_constructor_reports_the_unresolved_parameter() {
    let project = TestProject::new(
        "generic_new_imported_uninferred",
        &[
            ("shapes.wi", SHAPES),
            (
                "main.wi",
                "import shapes::Bag;\nfn main() { let b = new Bag([]); println(b.size()); }\n",
            ),
        ],
    );
    let output = project.check("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("cannot infer type parameter `T` of generic class `Bag`"),
        "{stderr}"
    );
    assert!(stderr.contains("new Bag<T>(...)"), "{stderr}");
    assert_eq!(stderr.matches("error[").count(), 1, "{stderr}");
}

#[test]
fn generic_constructor_inference_example_runs() {
    let (out, ok) = compile_and_run(include_str!(
        "../../example/generic_constructor_inference.wi"
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "1\napple\nanswer\n42\n2.5\n");
}

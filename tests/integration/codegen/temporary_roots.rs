use super::*;

// ── GC-managed temporary rooting (willow-5mb) ────────────────────────────────

// Chain of string concatenations: intermediate r1 = (a + b) must survive
// the GC that runs during the second concat allocation.
#[test]
fn test_gc_tmp_string_concat_chain_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
class Names {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let n = new Names("Ada", "Lovelace");
    let s = n.first + " " + n.last;
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// Method return values used directly in concat must be safe.
#[test]
fn test_gc_tmp_method_return_in_concat_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn bang(s: String) -> String { return s + "!"; }
fn main() {
    let s = bang("hello") + bang("world");
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello!world!\n");
}

// Object literal with String fields: partially-initialised object must not
// be collected while field initialisers are still being evaluated.
#[test]
fn test_gc_tmp_object_literal_not_collected_during_init() {
    let (out, ok) = compile_and_run(
        r#"
fn make_str(s: String) -> String { return s + "."; }
class Rec {
    pub a: String;
    pub b: String;
    pub fn both(self) -> String { return self.a + self.b; }
}
fn main() {
    let r = new Rec(make_str("x"), make_str("y"));
    println(r.both());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "x.y.\n");
}

// 4-level concat chain stress test.
#[test]
fn test_gc_tmp_four_level_concat_chain() {
    let (out, ok) = compile_and_run(
        r#"
class W { pub v: String; pub fn get(self) -> String { return self.v; } }
fn main() {
    let a = new W("a");
    let b = new W("b");
    let c = new W("c");
    let d = new W("d");
    let s = a.get() + b.get() + c.get() + d.get();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abcd\n");
}

//! Explicit perspectives: 1 empty new; 2 contextual literals; 3 with_len;
//! 4 byte extremes; 5 indexed reads; 6 indexed writes; 7 adjacent byte isolation;
//! 8 push doubling; 9 pop/regrowth; 10 len; 11 iteration; 12 slice independence;
//! 13 empty slices; 14 concatenation; 15 self concatenation; 16 empty concat;
//! 17 equal contents; 18 unequal lengths; 19 unequal/order-sensitive contents;
//! 20 generic parameters/return; 21 generic construction; 22 fixed arrays;
//! 23 nested arrays; 24 generic fields; 25 closure capture; 26 async loans;
//! 27 async returned arrays; 28 freeze independence; 29 moving/major GC;
//! 30 captured element reference across growth; 31 negative/upper bounds;
//! 32 empty pop; 33 invalid slice; 34 impossible allocation; 35 release parity;
//! 36 word/reference regression; 37 generated operation counts; 38 sort/fill.
use super::support::*;

fn modes(source: &str, expected: &str) {
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn packed_byte_arrays_example() {
    modes(
        include_str!("../../example/packed_byte_arrays.wi"),
        "255\ntrue\n393\n[7, 128, 255]\n255\n4\n",
    );
}

#[test]
fn packed_byte_arrays_operations_and_generic_storage() {
    modes(
        r#"
import std::collections::Array;
fn identity<T>(a: Array<T>) -> Array<T> { return a; }
fn make<T>(value: T) -> Array<T> { return Array<T>::with_len(3, value); }
fn add<T>(a: Array<T>, value: T) { a.push(value); }
class Box<T> { pub values: Array<T>; }
fn main() {
 let empty = Array<u8>::new(); println(empty.len());
 let a = make<u8>(255); a[1] = 128; a[2] = 0;
 println(a.to_string()); println(identity<u8>(a)[1]);
 let b = a.slice(1, 3); b[0] = 7;
 println(a[1]); println(b.to_string());
 println(a.slice(0, 0).len()); println(a.slice(3, 3).len());
 println(empty.concat(a).content_equals(a));
 println(a.concat(empty).content_equals(a));
 println(empty.content_equals(Array<u8>::new()));
 let joined = a.concat(a); println(joined.to_string());
 joined[0] = 1; println(a[0]);
 println(a.content_equals(b));
 let reversed: Array<u8> = [0, 128, 255]; println(a.content_equals(reversed));
 a.sort(); println(a.content_equals(reversed));
 a.fill(42); println(a.to_string());
 let grown: Array<u8> = [];
 for i in 0..257 { add<u8>(grown, i.wrapping_to_u8()); }
 let mut sum = 0; for v in grown { sum += v.to_i64(); }
 println(sum); println(grown.len()); println(grown.pop());
 grown.push(255); println(grown[255]); println(grown[256]);
 let fixed: [u8; 3] = [0, 128, 255]; fixed[1] = 200;
 let fixed_empty: [u8; 0] = []; println(fixed_empty.len());
 let nested: Array<Array<u8>> = [a, grown];
 let holder = new Box<u8>(nested[1]);
 let captured = || holder.values[128];
 gc_minor_collect(); gc_collect();
 println(fixed[0]); println(fixed[1]); println(fixed[2]);
 println(captured()); println(nested[0][0]);
 let frozen = grown.freeze(); grown[0] = 99;
 gc_minor_collect(); gc_collect(); println(frozen[0]); println(grown[0]);
}
"#,
        "0\n[255, 128, 0]\n128\n128\n[7, 0]\n0\n0\ntrue\ntrue\ntrue\n[255, 128, 0, 255, 128, 0]\n255\nfalse\nfalse\ntrue\n[42, 42, 42]\n32640\n257\n0\n255\n255\n0\n0\n200\n255\n128\n42\n0\n99\n",
    );
}

#[test]
fn packed_byte_arrays_async_and_captured_owner() {
    modes(
        r#"
import std::collections::Array;
async fn modify(values: Array<u8>) {
 await sleep(1); values.push(255); gc_minor_collect(); values[0] = 128;
}
async fn produce() -> Array<u8> { await sleep(1); return [0, 200, 255]; }
fn growing(values: Array<u8>) -> u8 { values.push(9); gc_minor_collect(); gc_collect(); return 77; }
fn set(value: &mut u8, next: u8) { gc_minor_collect(); value = next; }
async fn main() {
 let fresh: Array<u8> = [0]; await modify(fresh);
 println(fresh.to_string());
 let returned = await produce(); println(returned.to_string());
 let one: Array<u8> = [1];
 set(&one[0], growing(one));
 // An element borrow retains the captured buffer, not the grown handle.
 println(one.to_string());
 let fixed: [u8; 2] = [0, 255]; set(&fixed[0], 128);
 gc_collect(); println(fixed[0]); println(fixed[1]);
}
"#,
        "[128, 255]\n[0, 200, 255]\n[1, 9]\n128\n255\n",
    );
}

#[test]
fn packed_byte_arrays_failure_parity() {
    let cases = [
        ("println(a[-1]);", "array index out of bounds"),
        ("println(a[1]);", "array index out of bounds"),
        ("a[-1] = 0;", "array index out of bounds"),
        ("a[1] = 0;", "array index out of bounds"),
        ("a.pop(); a.pop();", "cannot pop from an empty array"),
        ("a.slice(0, 2);", "array slice out of bounds"),
        (
            "Array<TYPE>::with_len(-1, 0);",
            "array length must be non-negative",
        ),
        (
            "Array<TYPE>::with_len(9223372036854775807, 0);",
            "array capacity too large",
        ),
    ];
    for ty in ["u8", "i64"] {
        for (body, message) in cases {
            let source = format!(
                "import std::collections::Array; fn main() {{ let a: Array<{ty}> = [0]; {} }}",
                body.replace("TYPE", ty)
            );
            let (out, ok) = compile_and_run_check_exit(&source);
            assert!(!ok, "{ty}: {body}: {out}");
            assert!(out.contains(message), "{ty}: {body}: {out}");
        }
    }
}

#[test]
fn packed_byte_arrays_shared_methods_preserve_other_types() {
    modes(
        r#"
import std::collections::Array;
fn main() {
 let ints = [1, 2]; println(ints.concat([3]).to_string());
 println(ints.content_equals([1, 2]));
 println([1.5, 2.5].concat([3.5]).to_string());
 println([true].concat([false]).content_equals([true, false]));
 let text = ["a", "b"]; let joined = text.concat(text);
 gc_collect(); println(joined.to_string());
 println(joined.content_equals(["a", "b", "a", "b"]));
 println([[1]].concat([[2]])[1][0]);
}
"#,
        "[1, 2, 3]\ntrue\n[1.5, 2.5, 3.5]\ntrue\n[\"a\", \"b\", \"a\", \"b\"]\ntrue\n2\n",
    );
}

#[test]
fn packed_byte_arrays_codegen_scales_by_operations() {
    // Each nonconstant checked read has one cold fallback. Adding supported
    // widths must not emit extra width variants at any of these call sites.
    for ty in ["u8", "i64"] {
        for count in [1, 4, 16] {
            let mut source = format!(
                "import std::collections::Array; fn probe(a: Array<{ty}>, i: i64) -> {ty} {{ let mut v: {ty} = 0;"
            );
            for offset in 0..count {
                source.push_str(&format!("v = v ^ a[i + {offset}];"));
            }
            source.push_str("return v; } fn main() { println(probe([1, 2, 3], 0)); }");
            let names = compile_and_collect_relocation_targets_all(&source, &[]);
            let gets = names
                .iter()
                .filter(|s| s.ends_with("willow_array_get"))
                .count();
            assert_eq!(gets, count);
            println!("packed-codegen type={ty} operations={count} access_fallbacks={gets}");
        }
    }
}

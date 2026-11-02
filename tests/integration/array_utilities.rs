use super::support::*;

#[test]
fn array_utilities_example() {
    let source = include_str!("../../example/array_utilities.wi");
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "7\n[2, 7, 7]\n[7, 7]\ntrue\n1\n[4, 4, 4]\nfirst\nsecond\nthird\n"
        );
    }
}

// Each case is an independently compiled acceptance perspective.
#[test]
fn array_utilities_values_and_boundaries() {
    for (label, body, expected) in [
        (
            "empty construction",
            "let a = Array::with_len(0, 9); a.sort(); println(a.len()); println(a.contains(9)); println(a.index_of(9).is_none());",
            "0\nfalse\ntrue\n",
        ),
        (
            "singleton",
            "let a = [9]; a.sort(); println(a.remove(0)); println(a.len());",
            "9\n0\n",
        ),
        (
            "insertion positions",
            "let a: Array<i64> = []; a.insert(0, 2); a.insert(0, 1); a.insert(2, 4); a.insert(2, 3); println(a.toString());",
            "[1, 2, 3, 4]\n",
        ),
        (
            "removal positions",
            "let a = [1, 2, 3, 4]; println(a.remove(0)); println(a.remove(1)); println(a.remove(1)); println(a.toString());",
            "1\n3\n4\n[2]\n",
        ),
        (
            "first match and absent",
            "let a = [4, 2, 4]; println(a.index_of(4).unwrap()); println(a.index_of(2).unwrap()); println(a.index_of(9).is_none());",
            "0\n1\ntrue\n",
        ),
        (
            "string content equality",
            "let a = [\"ab\", \"cd\"]; println(a.contains(\"a\" + \"b\")); println(a.index_of(\"cd\").unwrap());",
            "true\n1\n",
        ),
        (
            "boolean fill and search",
            "let a = Array<bool>::with_len(2, false); a.fill(true); println(a.contains(false)); println(a.index_of(true).unwrap());",
            "false\n0\n",
        ),
        (
            "floating search",
            "let a = [1.5, -0.0]; println(a.contains(0.0)); println(a.index_of(1.5).unwrap());",
            "true\n0\n",
        ),
        (
            "slice independent",
            "let a = [1, 2, 3]; let b = a.slice(0, 3); b[0] = 9; println(a[0]); println(b[0]); println(a.slice(3, 3).len()); println(a.slice(0, 0).len());",
            "1\n9\n0\n0\n",
        ),
        (
            "duplicate sort",
            "let a = [2, 1, 2, 1, 2]; a.sort(); println(a.toString());",
            "[1, 1, 2, 2, 2]\n",
        ),
        (
            "sorted sort",
            "let a = [1, 2, 3, 4, 5]; a.sort(); println(a.toString());",
            "[1, 2, 3, 4, 5]\n",
        ),
        (
            "reverse even sort",
            "let a = [6, 5, 4, 3, 2, 1]; a.sort(); println(a.toString());",
            "[1, 2, 3, 4, 5, 6]\n",
        ),
        (
            "integer extremes",
            "let a = [9223372036854775807, -9223372036854775807 - 1, 0]; a.sort(); println(a.toString());",
            "[-9223372036854775808, 0, 9223372036854775807]\n",
        ),
        (
            "floating order",
            "let a = [2.5, -1.0, 0.0, 2.5]; a.sort(); println(a.toString());",
            "[-1, 0, 2.5, 2.5]\n",
        ),
        (
            "descending comparator",
            "let a = [1, 4, 2, 3]; a.sort_by(|a, b| a > b); println(a.toString());",
            "[4, 3, 2, 1]\n",
        ),
        (
            "boolean comparator",
            "let a = [true, false, true, false]; a.sort_by(|a, b| !a && b); println(a.toString());",
            "[false, false, true, true]\n",
        ),
        (
            "swap and same slot",
            "let a = [1, 2, 3]; a.swap(0, 2); a.swap(1, 1); println(a.toString());",
            "[3, 2, 1]\n",
        ),
        (
            "fixed array nonresizing",
            "let a: [i64; 3] = [3, 1, 2]; a.sort(); a.swap(0, 2); println(a.slice(0, 3).toString()); a.fill(8); println(a[1]);",
            "[3, 2, 1]\n8\n",
        ),
        (
            "contextual construction",
            "let a: Array<String> = Array::with_len(2, \"x\"); println(a.join(\"-\"));",
            "x-x\n",
        ),
        (
            "aliased construction",
            "let a = Items::with_len(2, 3); println(a.toString());",
            "[3, 3]\n",
        ),
    ] {
        let source = format!(
            "import std::collections::Array; import std::collections::Array as Items; fn main() {{ {body} }}"
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{label}: {out}");
        assert_eq!(out, expected, "{label}");
    }
}

#[test]
fn array_utilities_invalid_bounds() {
    for (body, message) in [
        (
            "Array::with_len(-1, 0);",
            "array length must be non-negative",
        ),
        ("[1].slice(-1, 0);", "array slice out of bounds"),
        ("[1].slice(1, 0);", "array slice out of bounds"),
        ("[1].slice(0, 2);", "array slice out of bounds"),
        ("[1].insert(-1, 2);", "array insert out of bounds"),
        ("[1].insert(2, 2);", "array insert out of bounds"),
        ("[1].remove(-1);", "array remove out of bounds"),
        ("[1].remove(1);", "array remove out of bounds"),
        (
            "let a: Array<i64> = []; a.remove(0);",
            "array remove out of bounds",
        ),
        ("[1].swap(0, 1);", "out of bounds"),
    ] {
        let source = format!("import std::collections::Array; fn main() {{ {body} }}");
        let (out, ok) = compile_and_run_check_exit(&source);
        assert!(!ok, "{body}: {out}");
        assert!(out.contains(message), "{body}: {out}");
    }
}

#[test]
fn array_utilities_reject_invalid_types_and_fixed_resize() {
    for body in [
        "let a: [i64; 1] = [1]; a.insert(0, 2);",
        "let a: [i64; 1] = [1]; a.remove(0);",
        "[1].fill(true);",
        "[1].contains(true);",
        "[1].index_of(true);",
        "[1].slice(false, 1);",
        "[1].insert(0, false);",
        "[1].sort_by(|a, b| 1);",
        "Array<i64>::with_len(1, true);",
        "Array::with_len(1);",
        "Array<i64>::with_len(true, 1);",
        "[1].freeze().fill(2);",
    ] {
        let source = format!("import std::collections::Array; fn main() {{ {body} }}");
        assert!(expect_compile_error(&source), "{body}");
    }
}

#[test]
fn array_utilities_gc_and_pair_storage() {
    let source = r#"
import std::collections::Array;
class Cell { pub value: i64; }
interface Key { fn key() -> i64; }
class Item implements Key { pub value: i64; pub fn key() -> i64 { return self.value; } }
fn main() {
    let cell = new Cell(7);
    let cells = Array::with_len(3, cell);
    let copied = cells.slice(0, 3);
    cells.insert(1, new Cell(9));
    println(cells.remove(0).value);
    cells.sort_by(|a, b| { gc_collect(); return a.value < b.value; });
    cell.value = 8;
    println(copied[2].value);
    println(cells[2].value);
    let opts: Array<Option<i64>> = Array::with_len(4, Some(3));
    opts.insert(1, Some(1));
    opts.insert(2, None);
    println(opts.remove(2).is_none());
    let subset = opts.slice(0, opts.len());
    opts.fill(Some(8));
    subset.sort_by(|a, b| a.unwrap() < b.unwrap());
    gc_collect();
    println(subset[0].unwrap());
    println(subset[4].unwrap());
    println(opts[1].unwrap());
    let items: Array<Key> = Array::with_len(2, new Item(4));
    items.insert(1, new Item(2));
    items.insert(3, new Item(6));
    items.insert(4, new Item(1));
    let snapshot = items.slice(0, items.len());
    items.sort_by(|a, b| { gc_collect(); return a.key() < b.key(); });
    println(items.remove(0).key());
    println(items[3].key());
    println(snapshot[4].key());
    let strings = Array::with_len(3, "kept" + "!");
    strings.insert(1, "x");
    println(strings.remove(2));
    strings.sort_by(|a, b| { gc_collect(); return a.len() < b.len(); });
    println(strings.slice(0, 3).join(","));
}
"#;
    for run in [
        compile_and_run,
        compile_and_run_release,
        compile_and_run_gc_stress_all,
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "7\n8\n9\ntrue\n1\n3\n8\n1\n6\n1\nkept!\nx,kept!,kept!\n"
        );
    }
}

#[test]
fn array_utilities_comparison_scaling() {
    let source = r#"
import std::collections::Array;
class Counter { pub value: i64; }
fn main() {
    for n in [16, 64, 256, 1024] {
        for shape in 0..4 {
            let a = Array::with_len(n, 0);
            for i in 0..n {
                if shape == 0 { a[i] = i; }
                if shape == 1 { a[i] = n - i; }
                if shape == 2 { a[i] = 7; }
                if shape == 3 { a[i] = (i * 37) % 11; }
            }
            let count = new Counter(0);
            a.sort_by(|x, y| { count.value = count.value + 1; return x < y; });
            for i in 1..n { if a[i - 1] > a[i] { panic("not sorted"); } }
            println(count.value);
        }
    }
}
"#;
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    let counts: Vec<u64> = out.lines().map(|line| line.parse().unwrap()).collect();
    assert_eq!(counts.len(), 16);
    for (n, group) in [16_u64, 64, 256, 1024]
        .into_iter()
        .zip(counts.chunks_exact(4))
    {
        // At most two comparisons per heap level in build/extract, with
        // linear heap construction. Count operations, not elapsed time.
        let bound = 2 * n * u64::from(n.ilog2()) + 2 * n;
        for &count in group {
            assert!(count <= bound, "n={n}: {count} > {bound}");
        }
        eprintln!("array sort n={n} counts={group:?} bound={bound}");
    }
}

#[test]
fn array_utilities_constructor_arguments_once_and_nested() {
    let source = r#"
import std::collections::Array;
fn count() -> i64 { println("count"); return 2; }
fn value() -> i64 { println("value"); return 7; }
fn main() {
    let a = Array::with_len(count(), value());
    println(a[1]);
    let b = Array::with_len(2, Array::with_len(3, Array::with_len(1, 4)));
    println(b[1][2][0]);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "count\nvalue\n7\n4\n");
}

#[test]
fn array_utilities_float_nan_order() {
    let source = r#"
fn main() {
    let nan = f64::parse("NaN").unwrap();
    let a = [nan, 3.0, -1.0, nan, 0.0];
    a.sort();
    println(a[0]); println(a[1]); println(a[2]);
    println(a[3] != a[3]); println(a[4] != a[4]);
    println(a.contains(nan));
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n0\n3\ntrue\ntrue\nfalse\n");
}

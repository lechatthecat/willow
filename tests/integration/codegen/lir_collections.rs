use super::*;

#[test]
fn lir_diff_c25_empty_map_filled_by_insert() {
    assert_program_output(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    m.insert("a", 3);
    return m.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c26_contains_hit_and_miss() {
    assert_program_output(
        &map_source(
            r#"
fn has(key: String) -> bool {
    let m: Map<String, i64> = Map::new();
    m.insert("here", 1);
    return m.contains(key);
}
fn main() { println(has("here")); println(has("gone")); }
"#,
        ),
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_c27_map_to_string() {
    // The runtime sorts by key, so this text is stable on both paths.
    assert_program_output(
        &map_source(
            r#"
fn render() -> String {
    let m: Map<String, i64> = Map::new();
    m.insert("b", 2);
    m.insert("a", 1);
    return m.toString();
}
fn main() { println(render()); }
"#,
        ),
        "{a: 1, b: 2}\n",
    );
}

#[test]
fn lir_diff_c28_int_keyed_map_with_string_values() {
    // The other admitted key type, and a value that is a GC handle rather than
    // a word — both halves of the `MapKey` ABI in one program.
    assert_program_output(
        &map_source(
            r#"
fn render() -> String {
    let m: Map<i64, String> = Map::new();
    m.insert(2, "two");
    m.insert(1, "one");
    return m.toString();
}
fn main() { println(render()); }
"#,
        ),
        "{1: \"one\", 2: \"two\"}\n",
    );
}

#[test]
fn lir_diff_c29_map_parameter() {
    assert_program_output(
        &map_source(
            r#"
fn size(m: Map<String, i64>) -> i64 { return m.len(); }
fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("x", 1);
    m.insert("y", 2);
    println(size(m));
}
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c30_map_returned_across_frames() {
    // The callee's map must survive the return — it is live only through the
    // caller's slot once the callee's roots are popped.
    assert_program_output(
        &map_source(
            r#"
fn make(n: i64) -> Map<i64, i64> {
    let m: Map<i64, i64> = Map::new();
    let mut i = 0;
    while i < n { m.insert(i, i * i); i = i + 1; }
    return m;
}
fn main() {
    let m = make(4);
    println(m.len());
    println(m.contains(3));
}
"#,
        ),
        "4\ntrue\n",
    );
}

#[test]
fn lir_diff_c31_map_freeze_and_read() {
    assert_program_output(
        &map_source(
            r#"
fn frozen() -> FrozenMap<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("k", 7);
    return m.freeze();
}
fn main() {
    let g = frozen();
    println(g.len());
    println(g.contains("k"));
    println(g.contains("nope"));
}
"#,
        ),
        "1\ntrue\nfalse\n",
    );
}

#[test]
fn lir_diff_c32_array_freeze_and_index() {
    assert_program_output(
        &map_source(
            r#"
fn frozen() -> FrozenArray<i64> {
    let xs: Array<i64> = [10, 20, 30];
    return xs.freeze();
}
fn main() {
    let ys = frozen();
    println(ys.len());
    println(ys[0]);
    println(ys[2]);
}
"#,
        ),
        "3\n10\n30\n",
    );
}

#[test]
fn lir_diff_c33_frozen_array_parameter_in_a_loop() {
    assert_program_output(
        &map_source(
            r#"
fn total(xs: FrozenArray<i64>) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() { sum = sum + xs[i]; i = i + 1; }
    return sum;
}
fn main() {
    let xs: Array<i64> = [1, 2, 3, 4];
    println(total(xs.freeze()));
}
"#,
        ),
        "10\n",
    );
}

#[test]
fn lir_diff_c34_map_of_arrays() {
    // The value is a GC handle: the store goes through the same discipline as
    // any other reference, and the array must outlive the insert.
    assert_program_output(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, Array<i64>> = Map::new();
    m.insert("a", [1, 2]);
    m.insert("b", [3]);
    return m.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c35_map_of_maps() {
    assert_program_output(
        &map_source(
            r#"
fn inner(v: i64) -> Map<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("v", v);
    return m;
}
fn build() -> i64 {
    let outer: Map<String, Map<String, i64>> = Map::new();
    outer.insert("one", inner(1));
    outer.insert("two", inner(2));
    return outer.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c36_inserts_in_a_loop() {
    // The map local is live across every iteration's allocations, and the keys
    // come out of a frozen array — both collection kinds in one loop.
    assert_program_output(
        &map_source(
            r#"
fn build(keys: FrozenArray<String>) -> i64 {
    let m: Map<String, i64> = Map::new();
    let mut i = 0;
    while i < keys.len() {
        m.insert(keys[i] + "!", i);
        i = i + 1;
    }
    return m.len();
}
fn main() {
    let keys: Array<String> = ["a", "b", "c", "d", "e"];
    println(build(keys.freeze()));
}
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c37_only_the_frozen_copy_is_kept() {
    // The mutable map goes out of scope; the frozen copy is an independent
    // object, so it must not observe anything the source did afterwards.
    assert_program_output(
        &map_source(
            r#"
fn snapshot() -> FrozenMap<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    let g = m.freeze();
    m.insert("b", 2);
    return g;
}
fn main() {
    let g = snapshot();
    println(g.len());
    println(g.contains("b"));
}
"#,
        ),
        "1\nfalse\n",
    );
}

#[test]
fn lir_diff_c38_receiver_rooted_across_allocating_value() {
    // `a + b` allocates a String between the receiver's evaluation and the
    // insert call. An unrooted receiver would be collected under stress.
    assert_program_output(
        &map_source(
            r#"
fn build(a: String, b: String) -> String {
    let m: Map<String, String> = Map::new();
    m.insert("k", a + b);
    return m.toString();
}
fn main() { println(build("x", "y")); }
"#,
        ),
        "{k: \"xy\"}\n",
    );
}

#[test]
fn lir_diff_c39_receiver_rooted_across_allocating_key() {
    assert_program_output(
        &map_source(
            r#"
fn build(a: String, b: String) -> String {
    let m: Map<String, i64> = Map::new();
    m.insert(a + b, 1);
    return m.toString();
}
fn main() { println(build("x", "y")); }
"#,
        ),
        "{xy: 1}\n",
    );
}

#[test]
fn lir_diff_c40_map_live_across_a_later_allocation() {
    assert_program_output(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    let s = "unrelated" + " allocation";
    let xs: Array<i64> = [1, 2, 3];
    xs.push(1);
    return m.len() + xs.len();
}
fn main() { println(build()); }
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c41_map_array_and_string_locals_at_once() {
    assert_program_output(
        &map_source(
            r#"
fn mixed() -> String {
    let m: Map<String, i64> = Map::new();
    let xs: Array<i64> = [1, 2];
    let s = "n=";
    m.insert("count", xs.len());
    return s + m.toString() + xs.toString();
}
fn main() { println(mixed()); }
"#,
        ),
        "n={count: 2}[1, 2]\n",
    );
}

#[test]
fn lir_diff_c42_insert_loop_under_gc_stress() {
    assert_output_under_gc_stress(
        &map_source(
            r#"
fn build(keys: FrozenArray<String>) -> String {
    let m: Map<String, i64> = Map::new();
    let mut i = 0;
    while i < keys.len() {
        m.insert(keys[i] + "!", i);
        i = i + 1;
    }
    return m.toString();
}
fn main() {
    let keys: Array<String> = ["a", "b", "c"];
    println(build(keys.freeze()));
}
"#,
        ),
        "{a!: 0, b!: 1, c!: 2}\n",
    );
}

#[test]
fn lir_diff_c43_freeze_under_gc_stress() {
    // `freeze` copies, so the source is rooted across the copy's allocation.
    assert_output_under_gc_stress(
        &map_source(
            r#"
fn snapshot() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    let g = m.freeze();
    let xs: Array<i64> = [1, 2, 3];
    return g.len() + xs.len();
}
fn main() { println(snapshot()); }
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c44_arrays_as_map_values_under_gc_stress() {
    assert_output_under_gc_stress(
        &map_source(
            r#"
fn build(n: i64) -> i64 {
    let m: Map<i64, Array<i64>> = Map::new();
    let mut i = 0;
    while i < n {
        m.insert(i, [i, i + 1]);
        i = i + 1;
    }
    return m.len();
}
fn main() { println(build(4)); }
"#,
        ),
        "4\n",
    );
}

#[test]
fn lir_diff_c45_map_handed_between_frames_under_gc_stress() {
    assert_output_under_gc_stress(
        &map_source(
            r#"
fn make() -> Map<String, String> {
    let m: Map<String, String> = Map::new();
    m.insert("a", "alpha");
    m.insert("b", "beta");
    return m;
}
fn grow(m: Map<String, String>) -> Map<String, String> {
    m.insert("c", "gamma");
    return m;
}
fn main() { println(grow(make()).toString()); }
"#,
        ),
        "{a: \"alpha\", b: \"beta\", c: \"gamma\"}\n",
    );
}

#[test]
fn lir_diff_c46_frozen_array_loop_under_gc_stress() {
    assert_output_under_gc_stress(
        &map_source(
            r#"
fn total(xs: FrozenArray<String>) -> String {
    let mut i = 0;
    let mut out = "";
    while i < xs.len() { out = out + xs[i]; i = i + 1; }
    return out;
}
fn main() {
    let xs: Array<String> = ["a", "b", "c"];
    println(total(xs.freeze()));
}
"#,
        ),
        "abc\n",
    );
}
